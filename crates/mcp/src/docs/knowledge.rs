//! Component, command and Molang reference from the metadata in a bedrock-samples release:
//! `json_schemas/` (each component's schema with descriptions and defaults),
//! `command_modules/mojang-commands.json` and `molang_modules/mojang-molang-queries.json`,
//! plus the vanilla files that use a component.

use std::{
    collections::HashMap,
    fs,
    path::{Component, Path, PathBuf},
};

use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    samples::{self, Samples},
    schemas,
};
use crate::{
    error::{Error, Result, bail},
    install::Install,
};

const MAX_MATCHES: usize = 20;
const MAX_EXAMPLES: usize = 15;
/// Vanilla files read at most when looking for examples.
const MAX_VANILLA_FILES: usize = 5000;
/// `$ref`s followed at most from one schema node.
const MAX_REF_HOPS: usize = 8;
/// How deep `oneOf`/`anyOf` alternatives are followed for a component's shapes.
const MAX_FORM_DEPTH: usize = 3;
const MAX_LISTED: usize = 40;
const MAX_ENUM_VALUES: usize = 60;
const COMMANDS: &str = "command_modules/mojang-commands.json";
const MOLANG: &str = "molang_modules/mojang-molang-queries.json";

/// A file type whose components the metadata documents.
struct Kind {
    name: &'static str,
    /// The schema folder under `json_schemas/`, which holds one folder per format version.
    folder: &'static str,
    /// The file in a version folder that lists the components.
    index: &'static str,
    /// The rockide document id of the same file type.
    rockide: &'static str,
    /// The vanilla folder in the release with files of this type, if it is kept.
    vanilla: Option<&'static str>,
}

const KINDS: [Kind; 6] = [
    Kind {
        name: "entity",
        folder: "server/entity",
        index: "Entity component definitions.json",
        rockide: "bp/entities",
        vanilla: Some("behavior_pack/entities"),
    },
    Kind {
        name: "item",
        folder: "server/item",
        index: "Item Components.json",
        rockide: "bp/items",
        vanilla: Some("behavior_pack/items"),
    },
    Kind {
        name: "block",
        folder: "server/block_components",
        index: "Block Components.json",
        rockide: "bp/blocks",
        vanilla: Some("behavior_pack/blocks"),
    },
    Kind {
        name: "biome",
        folder: "server/biome",
        index: "Biome Components.json",
        rockide: "bp/biomes",
        vanilla: Some("behavior_pack/biomes"),
    },
    Kind {
        name: "client_biome",
        folder: "client/biome",
        index: "Client Biome Components.json",
        rockide: "rp/biomes",
        vanilla: None,
    },
    Kind {
        name: "particle",
        folder: "client/particles",
        index: "particle_effect component.json",
        rockide: "rp/particles",
        vanilla: Some("resource_pack/particles"),
    },
];

pub fn kind_names() -> Vec<&'static str> {
    KINDS.iter().map(|kind| kind.name).collect()
}

/// `%20` and other percent escapes decoded, as `$ref`s write file names.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escaped = (bytes[index] == b'%')
            .then(|| text.get(index + 1..index + 3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(byte) => {
                out.push(byte);
                index += 3;
            }
            None => {
                out.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The schema files of one release, each parsed once per request.
struct Schemas {
    root: PathBuf,
    files: HashMap<PathBuf, Option<Value>>,
}

impl Schemas {
    fn new(metadata: &Path) -> Self {
        Self {
            root: metadata.join("json_schemas"),
            files: HashMap::new(),
        }
    }

    fn file(&mut self, path: &Path) -> Option<&Value> {
        self.files
            .entry(path.to_owned())
            .or_insert_with(|| serde_json::from_slice(&fs::read(path).ok()?).ok())
            .as_ref()
    }

    /// The file and node a `$ref` in `from` points to, kept inside the schema tree.
    fn target(&self, from: &Path, reference: &str) -> Option<(PathBuf, String)> {
        let (file, pointer) = reference.split_once('#').unwrap_or((reference, ""));
        let path = if file.is_empty() {
            from.to_owned()
        } else {
            let mut path = from.parent()?.to_owned();
            for part in Path::new(&percent_decode(file)).components() {
                match part {
                    Component::Normal(part) => path.push(part),
                    Component::ParentDir => {
                        path.pop();
                    }
                    Component::CurDir => {}
                    _ => return None,
                }
            }
            path
        };
        path.starts_with(&self.root)
            .then(|| (path, percent_decode(pointer)))
    }

    /// `node` (read from `file`) with its `$ref`s followed: the final node and its file.
    fn follow(&mut self, file: &Path, node: &Value) -> (PathBuf, Value) {
        let mut current = (file.to_owned(), node.clone());
        for _ in 0..MAX_REF_HOPS {
            let Some(reference) = current.1["$ref"].as_str() else {
                break;
            };
            let Some((path, pointer)) = self.target(&current.0, reference) else {
                break;
            };
            let Some(next) = self
                .file(&path)
                .and_then(|value| value.pointer(&pointer))
                .cloned()
            else {
                break;
            };
            current = (path, next);
        }
        current
    }

    /// A short name for the type of `node`.
    fn type_label(&mut self, file: &Path, node: &Value) -> String {
        if node.get("$ref").is_some() {
            let (path, target) = self.follow(file, node);
            return match target["type"].as_str() {
                Some(scalar) if scalar != "object" && scalar != "array" => scalar.to_owned(),
                _ => target["title"].as_str().map_or_else(
                    || {
                        path.file_stem()
                            .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned())
                    },
                    str::to_owned,
                ),
            };
        }
        if let Some(alternatives) = node["oneOf"]
            .as_array()
            .or_else(|| node["anyOf"].as_array())
        {
            let mut labels: Vec<String> = Vec::new();
            for alternative in alternatives {
                let label = self.type_label(file, alternative);
                if !labels.contains(&label) {
                    labels.push(label);
                }
            }
            return labels.join(" | ");
        }
        match &node["type"] {
            Value::String(ty) if ty == "array" && node.get("items").is_some() => {
                format!("array of {}", self.type_label(file, &node["items"]))
            }
            Value::String(ty) => ty.clone(),
            Value::Array(types) => types
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" | "),
            _ if node.get("properties").is_some() => "object".to_owned(),
            _ => "any".to_owned(),
        }
    }

    /// One property of a component.
    fn property(&mut self, file: &Path, name: &str, node: &Value) -> Value {
        let (target_file, target) = self.follow(file, node);
        let mut property = json!({ "name": name, "type": self.type_label(file, node) });
        let pick = |key: &str| {
            node.get(key)
                .or_else(|| target.get(key))
                .filter(|value| !value.is_null())
                .cloned()
        };
        for key in ["default", "description", "minimum", "maximum", "deprecated"] {
            if let Some(value) = pick(key).filter(|value| value.as_str() != Some("")) {
                property[key] = value;
            }
        }
        if let Some(values) = target["enum"].as_array() {
            property["values"] = values.iter().take(MAX_LISTED).cloned().collect();
        }
        let fields = self.object_fields(&target_file, &target);
        if !fields.is_empty() {
            property["fields"] = fields.into();
        }
        property
    }

    /// The property names of an object node, or of its object alternatives.
    fn object_fields(&mut self, file: &Path, node: &Value) -> Vec<String> {
        let mut names: Vec<String> = node["properties"]
            .as_object()
            .map(|properties| properties.keys().cloned().collect())
            .unwrap_or_default();
        if names.is_empty()
            && let Some(alternatives) = node["oneOf"]
                .as_array()
                .or_else(|| node["anyOf"].as_array())
        {
            for alternative in alternatives.clone() {
                let (_, target) = self.follow(file, &alternative);
                if let Some(properties) = target["properties"].as_object() {
                    names.extend(properties.keys().cloned());
                }
            }
        }
        names.truncate(MAX_LISTED);
        names
    }

    /// The shapes a component's value takes (`boolean`, `object`, ...) and the properties
    /// of its object shapes, in their documented order.
    fn forms(
        &mut self,
        file: &Path,
        node: &Value,
        depth: usize,
        forms: &mut Vec<String>,
        properties: &mut Vec<Value>,
    ) {
        let (file, node) = self.follow(file, node);
        if let Some(alternatives) = node["oneOf"]
            .as_array()
            .or_else(|| node["anyOf"].as_array())
        {
            if depth < MAX_FORM_DEPTH {
                for alternative in alternatives.clone() {
                    self.forms(&file, &alternative, depth + 1, forms, properties);
                }
            }
            return;
        }
        let Some(own) = node["properties"].as_object() else {
            let mut form = self.type_label(&file, &node);
            if let (Some(min), Some(max)) = (node.get("minimum"), node.get("maximum")) {
                form = format!("{form} ({min} to {max})");
            }
            if !forms.contains(&form) {
                forms.push(form);
            }
            return;
        };
        if !forms.iter().any(|form| form == "object") {
            forms.push("object".to_owned());
        }
        let mut ordered: Vec<(&String, &Value)> = own.iter().collect();
        ordered.sort_by_key(|(_, value)| value["x-ordinal-index"].as_u64().unwrap_or(u64::MAX));
        for (name, value) in ordered {
            if !properties
                .iter()
                .any(|property| property["name"] == name.as_str())
            {
                let property = self.property(&file, name, value);
                properties.push(property);
            }
        }
    }
}

/// The newest version folder of `kind` that has its component list.
fn index_file(root: &Path, kind: &Kind) -> Option<PathBuf> {
    let folder = root.join(kind.folder);
    let mut versions: Vec<(Vec<u32>, PathBuf)> = fs::read_dir(&folder)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let version: Vec<u32> = name
                .split('.')
                .map(str::parse)
                .collect::<Result<_, _>>()
                .ok()?;
            Some((version, entry.path().join(kind.index)))
        })
        .filter(|(_, index)| index.is_file())
        .collect();
    versions.sort();
    versions.pop().map(|(_, index)| index)
}

/// `minecraft:` is assumed when no namespace is given.
fn component_name(name: &str) -> String {
    let name = name.trim().to_lowercase();
    if name.contains(':') {
        name
    } else {
        format!("minecraft:{name}")
    }
}

/// Edit distance, for typos in names.
pub(super) fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, a) in a.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, b) in b.iter().enumerate() {
            let next = (previous + usize::from(a != *b))
                .min(row[j] + 1)
                .min(row[j + 1] + 1);
            previous = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

/// Whether `candidate` is close to `wanted`: one contains the other, or a small typo.
fn close(candidate: &str, wanted: &str) -> bool {
    candidate.contains(wanted) || (wanted.len() >= 4 && distance(candidate, wanted) <= 2)
}

/// The vanilla files of `kind` that mention `name` as a JSON key or value.
fn vanilla_examples(dir: &Path, kind: &Kind, name: &str) -> Value {
    let Some(folder) = kind.vanilla else {
        return Value::Null;
    };
    let needle = format!("\"{name}\"");
    let mut files = Vec::new();
    let mut pending = vec![dir.join(folder)];
    let mut read = 0;
    while let Some(next) = pending.pop() {
        for entry in fs::read_dir(&next).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            read += 1;
            if read > MAX_VANILLA_FILES {
                break;
            }
            if fs::read_to_string(&path).is_ok_and(|text| text.contains(&needle))
                && let Ok(relative) = path.strip_prefix(dir)
            {
                files.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    files.sort();
    let count = files.len();
    files.truncate(MAX_EXAMPLES);
    json!({ "count": count, "files": files })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentRequest {
    pub component: String,
    #[serde(default)]
    pub kind: Option<String>,
}

/// Looks a component up in rockide/schemas: its schema id and schema.
pub type Rockide<'a> = &'a dyn Fn(&str, &str) -> Result<Value>;

fn rockide_schema(kind: &str, component: &str) -> Result<Value> {
    schemas::get(&schemas::Get {
        id: None,
        kind: Some(kind.to_owned()),
        component: Some(component.to_owned()),
        format_version: None,
    })
}

/// Properties from a rockide schema, for components Mojang's metadata gives no shape.
fn rockide_properties(schema: &Value) -> Vec<Value> {
    schema["properties"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, node)| {
            let ty = match (&node["type"], node["$ref"].as_str()) {
                (Value::String(ty), _) => ty.clone(),
                (_, Some(reference)) => reference
                    .rsplit(['/', '#'])
                    .next()
                    .unwrap_or(reference)
                    .trim_end_matches(".json")
                    .to_owned(),
                _ => "any".to_owned(),
            };
            let mut property = json!({ "name": name, "type": ty });
            if let Some(description) = node["description"].as_str().filter(|text| !text.is_empty())
            {
                property["description"] = description.into();
            }
            property
        })
        .collect()
}

fn component_reply(
    samples: &Samples,
    request: &ComponentRequest,
    rockide: Rockide,
) -> Result<Value> {
    let name = component_name(&request.component);
    let kinds: Vec<&Kind> = match request.kind.as_deref().map(str::trim) {
        None | Some("") => KINDS.iter().collect(),
        Some(wanted) => vec![
            KINDS
                .iter()
                .find(|kind| kind.name == wanted)
                .ok_or_else(|| {
                    Error::msg(format!(
                        "No component kind {wanted:?}; kinds: {}.",
                        kind_names().join(", ")
                    ))
                })?,
        ],
    };
    let mut schemas = Schemas::new(&samples.metadata());
    let mut found = Vec::new();
    let mut similar = Vec::new();
    for kind in kinds {
        let Some(index) = index_file(&schemas.root, kind) else {
            continue;
        };
        let Some(listed) = schemas
            .file(&index)
            .and_then(|value| value["properties"].as_object())
            .cloned()
        else {
            continue;
        };
        if let Some(entry) = listed.get(&name) {
            found.push((kind, index, entry.clone()));
            continue;
        }
        let bare = name.split_once(':').map_or(name.as_str(), |(_, bare)| bare);
        for (component, entry) in &listed {
            let candidate = component
                .split_once(':')
                .map_or(component.as_str(), |(_, bare)| bare);
            if close(candidate, bare) {
                similar.push(json!({ "kind": kind.name, "component": component, "description": entry["description"] }));
            }
        }
    }
    let mut result =
        json!({ "source": samples.source(), "component": name, "notes": samples.notes });
    let (kind, index, entry) = match found.len() {
        0 => {
            result["found"] = false.into();
            result["match_count"] = similar.len().into();
            similar.truncate(MAX_MATCHES);
            result["matches"] = similar.into();
            return Ok(result);
        }
        1 => found.remove(0),
        _ => {
            let kinds: Vec<&str> = found.iter().map(|(kind, _, _)| kind.name).collect();
            bail!(
                "{name} is a component of several kinds ({}); pass kind to pick one.",
                kinds.join(", ")
            );
        }
    };
    let mut forms = Vec::new();
    let mut properties = Vec::new();
    let (file, schema) = schemas.follow(&index, &entry);
    if entry.get("$ref").is_some() {
        schemas.forms(&index, &entry, 0, &mut forms, &mut properties);
    }
    result["found"] = true.into();
    result["kind"] = kind.name.into();
    let description = entry["description"]
        .as_str()
        .or(schema["description"].as_str());
    result["description"] = description.into();
    if entry["deprecated"] == true {
        result["deprecated"] = true.into();
    }
    result["format_version"] = schema["x-format-version"].clone();
    if let Ok(relative) = file.strip_prefix(samples.dir.as_path()) {
        result["schema_file"] = relative.to_string_lossy().replace('\\', "/").into();
    }
    result["forms"] = forms.into();
    if let Some(required) = schema["required"].as_array() {
        result["required"] = required.clone().into();
    }
    let mut notes: Vec<String> = Vec::new();
    match rockide(kind.rockide, &name) {
        Ok(rockide) => {
            result["rockide_schema"] = rockide["id"].clone();
            if properties.is_empty() {
                let fallback = rockide_properties(&rockide["schema"]);
                if !fallback.is_empty() {
                    notes.push(format!(
                        "Mojang's metadata gives {name} no properties; these come from the rockide schema {}.",
                        rockide["id"].as_str().unwrap_or_default()
                    ));
                    result["properties_source"] = "rockide".into();
                    properties = fallback;
                }
            }
        }
        Err(error) => {
            result["rockide_schema"] = Value::Null;
            notes.push(format!("No rockide schema: {error}"));
        }
    }
    result["properties"] = properties.into();
    result["vanilla_examples"] = vanilla_examples(&samples.dir, kind, &name);
    if let Some(all) = result["notes"].as_array_mut() {
        all.extend(notes.into_iter().map(Value::from));
    }
    Ok(result)
}

pub fn component(install: &Install, request: &ComponentRequest) -> Result<Value> {
    let samples = samples::open(install)?;
    component_reply(&samples, request, &rockide_schema)
}

/// A metadata file of the release, parsed.
fn read_metadata(samples: &Samples, file: &str) -> Result<Value> {
    let path = samples.metadata().join(file);
    let bytes = fs::read(&path)
        .map_err(|error| Error::msg(format!("{} lacks {file}: {error}", samples.release.tag)))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| Error::msg(format!("{} is not valid JSON: {error}", path.display())))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NameRequest {
    pub name: String,
}

/// The values of the command enum `ty` names (`AIMASSISTACTIONSET` is `AimAssistActionSet`).
fn enum_values(enums: &[Value], ty: &str) -> Option<Vec<Value>> {
    let found = enums.iter().find(|item| {
        item["name"]
            .as_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(ty))
    })?;
    Some(
        found["values"]
            .as_array()?
            .iter()
            .filter_map(|value| value["value"].as_str().map(Value::from))
            .collect(),
    )
}

fn command_detail(command: &Value, enums: &[Value]) -> Value {
    let name = command["name"].as_str().unwrap_or_default();
    let overloads: Vec<Value> = command["overloads"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|overload| {
            let mut usage = format!("/{name}");
            let params: Vec<Value> = overload["params"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|param| {
                    let param_name = param["name"].as_str().unwrap_or_default();
                    let ty = param["type"]["name"].as_str().unwrap_or_default();
                    let optional = param["is_optional"] == true;
                    let (open, close) = if optional { ('[', ']') } else { ('<', '>') };
                    usage.push_str(&format!(" {open}{param_name}: {ty}{close}"));
                    let mut entry = json!({ "name": param_name, "type": ty, "optional": optional });
                    // Long lists (every item or block id) are counted, not listed.
                    match enum_values(enums, ty) {
                        Some(values) if values.len() > MAX_ENUM_VALUES => {
                            entry["value_count"] = values.len().into();
                        }
                        Some(values) => entry["values"] = values.into(),
                        None => {}
                    }
                    entry
                })
                .collect();
            json!({ "usage": usage, "params": params })
        })
        .collect();
    let aliases: Vec<&str> = command["aliases"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|alias| alias["name"].as_str())
        .collect();
    json!({
        "name": name,
        "description": command["description"],
        "aliases": aliases,
        "permission_level": command["permission_level"],
        "requires_cheats": command["requires_cheats"],
        "overloads": overloads,
    })
}

fn command_reply(samples: &Samples, request: &NameRequest) -> Result<Value> {
    let data = read_metadata(samples, COMMANDS)?;
    let wanted = request.name.trim().trim_start_matches('/').to_lowercase();
    let wanted = wanted.split_whitespace().next().unwrap_or_default();
    let commands = data["commands"].as_array().map_or(&[][..], Vec::as_slice);
    let enums = data["command_enums"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    let mut result = json!({ "source": samples.source(), "notes": samples.notes });
    let named = |command: &&Value| {
        command["name"] == wanted
            || command["aliases"]
                .as_array()
                .is_some_and(|aliases| aliases.iter().any(|alias| alias["name"] == wanted))
    };
    if let Some(command) = commands.iter().find(named) {
        result["found"] = true.into();
        if let (Some(result), Value::Object(fields)) =
            (result.as_object_mut(), command_detail(command, enums))
        {
            result.extend(fields);
        }
        return Ok(result);
    }
    let matches: Vec<Value> = commands
        .iter()
        .filter(|command| {
            let name = command["name"].as_str().unwrap_or_default();
            let description = command["description"]
                .as_str()
                .unwrap_or_default()
                .to_lowercase();
            close(name, wanted) || description.contains(wanted)
        })
        .map(|command| json!({ "name": command["name"], "description": command["description"] }))
        .collect();
    result["found"] = false.into();
    result["match_count"] = matches.len().into();
    result["matches"] = matches.into_iter().take(MAX_MATCHES).collect();
    Ok(result)
}

pub fn command(install: &Install, request: &NameRequest) -> Result<Value> {
    command_reply(&samples::open(install)?, request)
}

/// `q.health` and `health` both name `query.health`; `abs` names `math.abs`.
fn molang_names(name: &str) -> Vec<String> {
    let name = name.trim().to_lowercase();
    match name.split_once('.') {
        Some(("q", rest)) => vec![format!("query.{rest}")],
        Some(_) => vec![name],
        None => vec![format!("query.{name}"), format!("math.{name}")],
    }
}

fn molang_reply(samples: &Samples, request: &NameRequest) -> Result<Value> {
    let data = read_metadata(samples, MOLANG)?;
    let all: Vec<(&str, &Value)> = [("queries", "query"), ("math_functions", "math")]
        .into_iter()
        .flat_map(|(key, kind)| {
            data[key]
                .as_array()
                .into_iter()
                .flatten()
                .map(move |entry| (kind, entry))
        })
        .collect();
    let mut result = json!({ "source": samples.source(), "notes": samples.notes });
    let names = molang_names(&request.name);
    if let Some((kind, entry)) = names
        .iter()
        .find_map(|name| all.iter().find(|(_, entry)| entry["name"] == name.as_str()))
    {
        result["found"] = true.into();
        result["kind"] = (*kind).into();
        for key in [
            "name",
            "description",
            "return_type",
            "min_args",
            "max_args",
            "version_ranges",
        ] {
            if let Some(value) = entry.get(key) {
                result[key] = value.clone();
            }
        }
        return Ok(result);
    }
    let wanted = names[0].rsplit('.').next().unwrap_or_default().to_owned();
    let matches: Vec<Value> = all
        .iter()
        .filter(|(_, entry)| {
            let name = entry["name"].as_str().unwrap_or_default();
            let bare = name.rsplit('.').next().unwrap_or_default();
            let description = entry["description"]
                .as_str()
                .unwrap_or_default()
                .to_lowercase();
            close(bare, &wanted) || description.contains(&wanted)
        })
        .map(|(_, entry)| json!({ "name": entry["name"], "description": entry["description"] }))
        .collect();
    result["found"] = false.into();
    result["match_count"] = matches.len().into();
    result["matches"] = matches.into_iter().take(MAX_MATCHES).collect();
    Ok(result)
}

pub fn molang(install: &Install, request: &NameRequest) -> Result<Value> {
    molang_reply(&samples::open(install)?, request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{docs::samples::Release, testdir::TestDir};

    fn write(root: &Path, path: &str, value: Value) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, value.to_string()).unwrap();
    }

    /// A release with a tempt-like entity component, a health-like one with no schema, an
    /// item component with two shapes, two commands and Molang entries.
    fn fixture() -> (TestDir, Samples) {
        let temp = TestDir::new("knowledge");
        let root = temp.path();
        let schemas = "metadata/json_schemas/server";
        write(
            root,
            &format!("{schemas}/entity/1.0.0/Entity component definitions.json"),
            json!({ "properties": {
                "minecraft:behavior.tempt": { "$ref": "./minecraft_behavior.tempt.json", "description": "Lets a mob be tempted." },
                "minecraft:health": { "description": "Dynamic value" }
            }}),
        );
        write(
            root,
            &format!("{schemas}/entity/0.9.0/Entity component definitions.json"),
            json!({ "properties": {} }),
        );
        write(
            root,
            &format!("{schemas}/entity/1.0.0/minecraft_behavior.tempt.json"),
            json!({
                "description": "Lets a mob be tempted.",
                "x-format-version": "1.0.0",
                "properties": {
                    "within_radius": { "default": 0, "description": "Distance in blocks.", "type": "number", "x-ordinal-index": 1 },
                    "speed_multiplier": { "default": 1, "description": "Speed while tempted.", "type": "number", "x-ordinal-index": 0 },
                    "items": { "type": "array", "items": { "$ref": "../../common/1.0.0/Item%20Descriptor.json" }, "description": "Items that tempt." },
                    "on_start": { "$ref": "../0.9.0/Trigger.json", "description": "Event when tempted." }
                }
            }),
        );
        write(
            root,
            &format!("{schemas}/entity/0.9.0/Trigger.json"),
            json!({ "title": "Trigger", "type": "object", "properties": { "event": {}, "target": {} } }),
        );
        write(
            root,
            &format!("{schemas}/common/1.0.0/Item Descriptor.json"),
            json!({ "title": "Item Descriptor", "oneOf": [{ "type": "string" }, { "type": "object" }] }),
        );
        write(
            root,
            &format!("{schemas}/item/1.0.0/Item Components.json"),
            json!({ "properties": {
                "minecraft:damage": { "$ref": "./minecraft_damage.json", "description": "Extra attack damage." }
            }}),
        );
        write(
            root,
            &format!("{schemas}/item/1.0.0/minecraft_damage.json"),
            json!({ "oneOf": [
                { "type": "integer", "minimum": 0, "maximum": 100 },
                { "type": "object", "properties": { "value": { "type": "integer", "default": 0, "description": "Extra damage." } } }
            ]}),
        );
        write(
            root,
            "behavior_pack/entities/cow.json",
            json!({ "minecraft:entity": { "components": { "minecraft:behavior.tempt": {} } } }),
        );
        write(
            root,
            "behavior_pack/entities/stone_golem.json",
            json!({ "minecraft:entity": {} }),
        );
        write(
            root,
            &format!("metadata/{COMMANDS}"),
            json!({
                "command_enums": [{ "name": "GameMode", "values": [{ "value": "survival" }, { "value": "creative" }] }],
                "commands": [
                    { "name": "gamemode", "description": "Sets a player's game mode.", "aliases": [{ "name": "gm" }], "permission_level": 1, "requires_cheats": true,
                      "overloads": [{ "name": "1", "params": [
                          { "name": "gameMode", "is_optional": false, "type": { "name": "GAMEMODE" } },
                          { "name": "player", "is_optional": true, "type": { "name": "SELECTION" } }
                      ] }] },
                    { "name": "give", "description": "Gives an item to a player.", "aliases": [], "overloads": [] }
                ]
            }),
        );
        write(
            root,
            &format!("metadata/{MOLANG}"),
            json!({
                "queries": [{ "name": "query.health", "description": "Returns the health of the entity.", "min_args": 0, "return_type": "float", "version_ranges": [{ "first_version": "0.0.0", "query_sets": ["default"] }] }],
                "math_functions": [{ "name": "math.abs", "description": "Absolute value.", "min_args": 1, "max_args": 1 }]
            }),
        );
        let samples = Samples {
            release: Release::parse("v1.0.0.1").unwrap(),
            dir: root.to_owned(),
            game: None,
            notes: vec![],
        };
        (temp, samples)
    }

    fn no_rockide(_: &str, _: &str) -> Result<Value> {
        bail!("No component in rockide.")
    }

    fn component(samples: &Samples, request: Value, rockide: Rockide) -> Value {
        component_reply(samples, &serde_json::from_value(request).unwrap(), rockide).unwrap()
    }

    #[test]
    fn a_component_lists_its_properties_in_order_with_examples() {
        let (_temp, samples) = fixture();
        let reply = component(
            &samples,
            json!({ "component": "behavior.tempt" }),
            &no_rockide,
        );
        assert_eq!(
            (reply["found"].as_bool(), reply["kind"].as_str()),
            (Some(true), Some("entity"))
        );
        assert_eq!(reply["description"], "Lets a mob be tempted.");
        assert_eq!(
            reply["schema_file"],
            "metadata/json_schemas/server/entity/1.0.0/minecraft_behavior.tempt.json"
        );
        let properties = reply["properties"].as_array().unwrap();
        assert_eq!(
            properties[0],
            json!({ "name": "speed_multiplier", "type": "number", "default": 1, "description": "Speed while tempted." })
        );
        assert_eq!(properties[1]["name"], "within_radius");
        let items = properties.iter().find(|p| p["name"] == "items").unwrap();
        assert_eq!(items["type"], "array of Item Descriptor");
        let trigger = properties.iter().find(|p| p["name"] == "on_start").unwrap();
        assert_eq!(
            (trigger["type"].as_str(), trigger["fields"].clone()),
            (Some("Trigger"), json!(["event", "target"]))
        );
        assert_eq!(
            reply["vanilla_examples"],
            json!({ "count": 1, "files": ["behavior_pack/entities/cow.json"] })
        );
        assert_eq!(reply["rockide_schema"], Value::Null);
    }

    #[test]
    fn components_with_several_shapes_list_each() {
        let (_temp, samples) = fixture();
        let reply = component(
            &samples,
            json!({ "component": "minecraft:damage", "kind": "item" }),
            &no_rockide,
        );
        assert_eq!(reply["forms"], json!(["integer (0 to 100)", "object"]));
        assert_eq!(reply["properties"][0]["name"], "value");
    }

    #[test]
    fn components_without_a_mojang_schema_use_rockide_and_say_so() {
        let (_temp, samples) = fixture();
        let rockide = |kind: &str, component: &str| {
            assert_eq!((kind, component), ("bp/entities", "minecraft:health"));
            Ok(
                json!({ "id": "bedrock/server/entity/health", "schema": { "properties": { "max": { "type": "number", "description": "" } } } }),
            )
        };
        let reply = component(&samples, json!({ "component": "health" }), &rockide);
        assert_eq!(reply["rockide_schema"], "bedrock/server/entity/health");
        assert_eq!(reply["properties_source"], "rockide");
        assert_eq!(
            reply["properties"],
            json!([{ "name": "max", "type": "number" }])
        );
        assert!(
            reply["notes"][0]
                .as_str()
                .unwrap()
                .contains("no properties")
        );
    }

    #[test]
    fn unknown_components_list_close_names() {
        let (_temp, samples) = fixture();
        let reply = component(
            &samples,
            json!({ "component": "minecraft:behavior.tmpt" }),
            &no_rockide,
        );
        assert_eq!(reply["found"], false);
        assert_eq!(reply["matches"][0]["component"], "minecraft:behavior.tempt");
        let error = component_reply(
            &samples,
            &serde_json::from_value(json!({ "component": "x", "kind": "mob" })).unwrap(),
            &no_rockide,
        )
        .unwrap_err();
        assert!(error.to_string().contains("kinds: entity, item"), "{error}");
    }

    #[test]
    fn commands_show_usage_and_enum_values() {
        let (_temp, samples) = fixture();
        let request = |name: &str| NameRequest {
            name: name.to_owned(),
        };
        let reply = command_reply(&samples, &request("/gm creative")).unwrap();
        assert_eq!(reply["name"], "gamemode");
        assert_eq!(
            reply["overloads"][0]["usage"],
            "/gamemode <gameMode: GAMEMODE> [player: SELECTION]"
        );
        assert_eq!(
            reply["overloads"][0]["params"][0]["values"],
            json!(["survival", "creative"])
        );
        let missing = command_reply(&samples, &request("item")).unwrap();
        assert_eq!(missing["found"], false);
        assert_eq!(missing["matches"][0]["name"], "give");
    }

    #[test]
    fn molang_names_take_short_forms() {
        let (_temp, samples) = fixture();
        let request = |name: &str| NameRequest {
            name: name.to_owned(),
        };
        for name in ["query.health", "q.health", "Health"] {
            let reply = molang_reply(&samples, &request(name)).unwrap();
            assert_eq!(
                (reply["kind"].as_str(), reply["name"].as_str()),
                (Some("query"), Some("query.health")),
                "{name}"
            );
        }
        assert_eq!(
            molang_reply(&samples, &request("abs")).unwrap()["kind"],
            "math"
        );
        let missing = molang_reply(&samples, &request("q.healt")).unwrap();
        assert_eq!(missing["matches"][0]["name"], "query.health");
    }
}
