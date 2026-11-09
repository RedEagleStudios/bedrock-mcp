//! The Script API reference: the module metadata in a bedrock-samples release
//! (`metadata/script_modules/@minecraft/<module>_<version>.json`: every type, member,
//! parameter type, range, default, privilege and error, but no prose) joined with the JSDoc
//! of the npm package for the same module version (descriptions, parameter docs, examples).

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    dts::{self, Doc},
    ensure_package,
    knowledge::distance,
    samples::{self, Release, Samples},
};
use crate::{
    error::{Error, Result, bail},
    install::Install,
};

const DEFAULT_MODULE: &str = "@minecraft/server";
const MAX_MATCHES: usize = 25;
const MAX_MEMBERS: usize = 200;
/// Base classes followed at most when looking up an inherited member.
const MAX_BASE_DEPTH: usize = 8;
/// Parsed module files kept for the process; each is a few MB once parsed.
const MAX_PARSED: usize = 6;
/// The lists of named types in a module file, with the kind each holds.
const TYPE_LISTS: [(&str, &str); 5] = [
    ("classes", "class"),
    ("interfaces", "interface"),
    ("enums", "enum"),
    ("errors", "error"),
    ("type_aliases", "type_alias"),
];

/// Documentation by declaration, from one package's `index.d.ts`.
pub type Docs = HashMap<String, Doc>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lookup {
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub module: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

/// One module file: `server_2.11.0.json` is `@minecraft/server` 2.11.0.
#[derive(Clone, Debug, PartialEq)]
struct ModuleFile {
    name: String,
    version: String,
    path: PathBuf,
}

impl ModuleFile {
    fn is_beta(&self) -> bool {
        self.version.contains('-')
    }
}

/// `2.12.0-beta` orders after `2.11.0` and before `2.12.0`.
fn version_key(version: &str) -> (Vec<u32>, bool) {
    let (numbers, suffix) = version.split_once('-').unwrap_or((version, ""));
    let numbers = numbers
        .split('.')
        .filter_map(|part| part.parse().ok())
        .collect();
    (numbers, suffix.is_empty())
}

fn module_files(dir: &Path) -> Vec<ModuleFile> {
    let mut files: Vec<ModuleFile> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let file = entry.file_name().to_string_lossy().into_owned();
            let (short, version) = file.strip_suffix(".json")?.rsplit_once('_')?;
            Some(ModuleFile {
                name: format!("@minecraft/{short}"),
                version: version.to_owned(),
                path: entry.path(),
            })
        })
        .collect();
    files.sort_by(|a, b| {
        (&a.name, version_key(&a.version)).cmp(&(&b.name, version_key(&b.version)))
    });
    files
}

/// `server` and `@minecraft/server` both name `@minecraft/server`.
fn module_name(given: Option<&str>) -> String {
    match given.map(str::trim).filter(|name| !name.is_empty()) {
        None => DEFAULT_MODULE.to_owned(),
        Some(name) if name.starts_with('@') => name.to_owned(),
        Some(name) => format!("@minecraft/{name}"),
    }
}

/// The module file `version` picks: `stable` (default: the newest without a suffix), `beta`
/// (the newest beta) or an exact version; an npm beta version such as
/// `2.12.0-beta.1.26.60-stable` names the file `2.12.0-beta`.
fn choose(
    files: &[ModuleFile],
    name: &str,
    version: Option<&str>,
) -> Result<(ModuleFile, Option<String>)> {
    let versions: Vec<&ModuleFile> = files.iter().filter(|file| file.name == name).collect();
    if versions.is_empty() {
        let mut names: Vec<&str> = files
            .iter()
            .map(|file| file.name.as_str())
            .filter(|name| !name.ends_with("-bindings"))
            .collect();
        names.dedup();
        bail!(
            "No script module {name} in this release; modules: {}.",
            names.join(", ")
        );
    }
    let listed = || {
        versions
            .iter()
            .map(|file| file.version.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let newest = |beta: bool| versions.iter().rev().find(|file| file.is_beta() == beta);
    match version.map(str::trim).unwrap_or("stable") {
        "stable" | "" => match newest(false) {
            Some(file) => Ok(((*file).clone(), None)),
            None => {
                let file = newest(true).expect("versions is not empty");
                let note = format!(
                    "{name} has no stable version; using beta {}, which needs the Beta APIs experiment.",
                    file.version
                );
                Ok(((*file).clone(), Some(note)))
            }
        },
        "beta" => newest(true)
            .map(|file| ((*file).clone(), None))
            .ok_or_else(|| {
                Error::msg(format!(
                    "{name} has no beta version; versions: {}.",
                    listed()
                ))
            }),
        exact => {
            let wanted = match exact.find("-beta") {
                Some(at) => &exact[..at + "-beta".len()],
                None => exact,
            };
            versions
                .iter()
                .find(|file| file.version == wanted)
                .map(|file| ((*file).clone(), None))
                .ok_or_else(|| {
                    Error::msg(format!(
                        "{name} has no version {exact}; versions: {}.",
                        listed()
                    ))
                })
        }
    }
}

/// The npm version whose `index.d.ts` documents module `version` in `release`: stable
/// versions as they are, betas tagged with the game version they shipped in.
fn npm_version(version: &str, release: &Release) -> String {
    match version.strip_suffix("-beta") {
        Some(base) if release.preview => format!(
            "{base}-beta.{}-preview.{}",
            release.line(),
            release.version[3]
        ),
        Some(base) => format!("{base}-beta.{}-stable", release.line()),
        None => version.to_owned(),
    }
}

/// `path` parsed, kept for the process.
fn parsed(path: &Path) -> Result<Arc<Value>> {
    static PARSED: Mutex<Vec<(PathBuf, Arc<Value>)>> = Mutex::new(Vec::new());
    let mut cache = PARSED.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some((_, value)) = cache.iter().find(|(cached, _)| cached == path) {
        return Ok(value.clone());
    }
    let bytes = fs::read(path)
        .map_err(|error| Error::msg(format!("Cannot read {}: {error}", path.display())))?;
    let value: Arc<Value> =
        Arc::new(serde_json::from_slice(&bytes).map_err(|error| {
            Error::msg(format!("{} is not valid JSON: {error}", path.display()))
        })?);
    if cache.len() >= MAX_PARSED {
        cache.remove(0);
    }
    cache.push((path.to_owned(), value.clone()));
    Ok(value)
}

/// The file that holds a module's API: the module's own, or the bindings module it wraps
/// (`server_2.11.0.json` names `server-bindings_2.11.0.json` as its `parentModule`).
fn api_path(dir: &Path, path: &Path, parent: Option<&Value>) -> Result<PathBuf> {
    let Some(parent) = parent.filter(|parent| parent.is_object()) else {
        return Ok(path.to_owned());
    };
    let (Some(name), Some(version)) = (parent["name"].as_str(), parent["version"].as_str()) else {
        bail!(
            "{} has a parentModule without name and version.",
            path.display()
        );
    };
    let short = name.strip_prefix("@minecraft/").unwrap_or(name);
    Ok(dir.join(format!("{short}_{version}.json")))
}

/// A module's API from its metadata.
struct Api<'a>(&'a Value);

impl<'a> Api<'a> {
    fn list(&self, key: &str) -> &'a [Value] {
        self.0[key].as_array().map_or(&[], Vec::as_slice)
    }

    /// The named type `name`, exact or else ignoring case, and its kind.
    fn find_type(&self, name: &str) -> Option<(&'static str, &'a Value)> {
        let all = || {
            TYPE_LISTS
                .iter()
                .flat_map(|(key, kind)| self.list(key).iter().map(move |value| (*kind, value)))
        };
        all().find(|(_, value)| value["name"] == name).or_else(|| {
            all().find(|(_, value)| {
                value["name"]
                    .as_str()
                    .is_some_and(|found| found.eq_ignore_ascii_case(name))
            })
        })
    }

    fn find_in(&self, key: &str, name: &str) -> Option<&'a Value> {
        let list = self.list(key);
        list.iter().find(|value| value["name"] == name).or_else(|| {
            list.iter().find(|value| {
                value["name"]
                    .as_str()
                    .is_some_and(|found| found.eq_ignore_ascii_case(name))
            })
        })
    }

    /// The members a type declares itself, with their kinds.
    fn own_members(
        owner: &'a Value,
        owner_kind: &str,
    ) -> impl Iterator<Item = (&'static str, &'a Value)> {
        let list = |key: &str| owner[key].as_array().map_or(&[][..], Vec::as_slice);
        let constant = if owner_kind == "enum" {
            "enum_value"
        } else {
            "constant"
        };
        list("functions")
            .iter()
            .map(|value| {
                let kind = if value["is_constructor"] == true {
                    "constructor"
                } else {
                    "method"
                };
                (kind, value)
            })
            .chain(list("properties").iter().map(|value| ("property", value)))
            .chain(list("constants").iter().map(move |value| (constant, value)))
    }

    /// Member `name` of type `owner`, declared there or on a base type: the declaring type's
    /// name, the member's kind and its metadata.
    fn member(
        &self,
        owner: &'a Value,
        owner_kind: &str,
        name: &str,
    ) -> Option<(String, &'static str, &'a Value)> {
        let mut current = (owner_kind, owner);
        for _ in 0..MAX_BASE_DEPTH {
            let (kind, value) = current;
            let found = Self::own_members(value, kind)
                .find(|(_, member)| member["name"] == name)
                .or_else(|| {
                    Self::own_members(value, kind).find(|(_, member)| {
                        member["name"]
                            .as_str()
                            .is_some_and(|found| found.eq_ignore_ascii_case(name))
                    })
                });
            if let Some((member_kind, member)) = found {
                return Some((value["name"].as_str()?.to_owned(), member_kind, member));
            }
            let base = value["base_types"].as_array()?.first()?["name"].as_str()?;
            current = self.find_type(base)?;
        }
        None
    }
}

/// The bound type a value of type `ty` has, through `optional`.
fn bound_type(ty: &Value) -> Option<&str> {
    if ty["name"] == "optional" {
        return bound_type(&ty["optional_type"]);
    }
    (ty["is_bind_type"] == true)
        .then(|| ty["name"].as_str())
        .flatten()
}

/// A metadata type as TypeScript.
fn render(ty: &Value) -> String {
    let name = ty["name"].as_str().unwrap_or("unknown");
    let list = |key: &str| -> Vec<String> {
        ty[key]
            .as_array()
            .into_iter()
            .flatten()
            .map(render)
            .collect()
    };
    match name {
        "optional" => format!("{} | undefined", render(&ty["optional_type"])),
        "variant" => list("variant_types").join(" | "),
        "array" => {
            let element = render(&ty["element_type"]);
            if element.contains(' ') {
                format!("({element})[]")
            } else {
                format!("{element}[]")
            }
        }
        "map" => format!(
            "Record<{}, {}>",
            render(&ty["key_type"]),
            render(&ty["value_type"])
        ),
        "closure" => {
            let closure = &ty["closure_type"];
            let arguments: Vec<String> = closure["argument_types"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .map(|(index, argument)| format!("arg{index}: {}", render(argument)))
                .collect();
            format!(
                "({}) => {}",
                arguments.join(", "),
                render(&closure["return_type"])
            )
        }
        "promise" => format!("Promise<{}>", render(&ty["promise_type"])),
        "generator" => {
            let generator = &ty["generator_type"];
            format!(
                "Generator<{}, {}, {}>",
                render(&generator["yield_type"]),
                render(&generator["return_type"]),
                render(&generator["next_type"])
            )
        }
        "int8" | "int16" | "int32" | "int64" | "uint8" | "uint16" | "uint32" | "uint64"
        | "float" | "double" => "number".to_owned(),
        _ => name.to_owned(),
    }
}

/// The names of the errors a type can throw.
fn errors(ty: &Value) -> Vec<String> {
    ty["error_types"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|error| {
            let name = error["name"].as_str()?;
            Some(match error["from_module"]["name"].as_str() {
                Some(module) => format!("{module}.{name}"),
                None => name.to_owned(),
            })
        })
        .collect()
}

fn privileges(value: &Value, key: &str) -> Vec<String> {
    value[key]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|privilege| privilege["name"].as_str().map(str::to_owned))
        .collect()
}

/// A member as TypeScript, from its metadata.
fn member_signature(kind: &str, member: &Value) -> String {
    let name = member["name"].as_str().unwrap_or_default();
    match kind {
        "method" | "constructor" | "function" => {
            let arguments: Vec<String> = member["arguments"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|argument| {
                    let ty = &argument["type"];
                    let name = argument["name"].as_str().unwrap_or_default();
                    if ty["name"] == "optional" {
                        format!("{name}?: {}", render(&ty["optional_type"]))
                    } else {
                        format!("{name}: {}", render(ty))
                    }
                })
                .collect();
            let prefix = if member["is_static"] == true {
                "static "
            } else {
                ""
            };
            format!(
                "{prefix}{name}({}): {}",
                arguments.join(", "),
                render(&member["return_type"])
            )
        }
        "enum_value" | "constant" => format!("{name} = {}", member["value"]),
        _ => {
            let prefix = if member["is_read_only"] == true {
                "readonly "
            } else {
                ""
            };
            format!("{prefix}{name}: {}", render(&member["type"]))
        }
    }
}

/// A symbol found in a module.
struct Hit<'a> {
    /// The symbol as resolved, e.g. `world.afterEvents.entityDie`.
    path: String,
    kind: &'static str,
    /// The declaration the docs list it under, e.g. `WorldAfterEvents.entityDie`.
    doc_key: String,
    value: &'a Value,
}

/// `Entity`, `Entity.applyDamage`, `world`, `world.afterEvents.entityDie` or a top-level
/// function or constant; members are looked up on the type of what precedes them.
fn resolve<'a>(api: &Api<'a>, symbol: &str) -> Option<Hit<'a>> {
    let mut segments = symbol.trim().split('.');
    let first = segments.next()?;
    let (mut hit, mut current) = if let Some(object) = api.find_in("objects", first) {
        let name = object["name"].as_str()?.to_owned();
        let hit = Hit {
            path: name.clone(),
            kind: "object",
            doc_key: name,
            value: object,
        };
        (hit, bound_type(&object["type"]).map(str::to_owned))
    } else if let Some((key, kind)) = [("functions", "function"), ("constants", "constant")]
        .into_iter()
        .find(|(key, _)| api.find_in(key, first).is_some())
    {
        let value = api.find_in(key, first)?;
        let name = value["name"].as_str()?.to_owned();
        (
            Hit {
                path: name.clone(),
                kind,
                doc_key: name,
                value,
            },
            None,
        )
    } else {
        let (kind, value) = api.find_type(first)?;
        let name = value["name"].as_str()?.to_owned();
        (
            Hit {
                path: name.clone(),
                kind,
                doc_key: name.clone(),
                value,
            },
            Some(name),
        )
    };
    for segment in segments {
        let (owner_kind, owner) = api.find_type(current.as_deref()?)?;
        let (declared_on, kind, member) = api.member(owner, owner_kind, segment)?;
        let name = member["name"].as_str()?;
        current = bound_type(&member["type"]).map(str::to_owned);
        hit = Hit {
            path: format!("{}.{name}", hit.path),
            kind,
            doc_key: format!("{declared_on}.{name}"),
            value: member,
        };
    }
    Some(hit)
}

/// A searchable symbol.
struct Entry {
    path: String,
    kind: &'static str,
    doc_key: String,
}

/// Every type and member, top-level object, function and constant, and the event paths
/// under objects (`world.afterEvents.entityDie`).
fn entries(api: &Api) -> Vec<Entry> {
    let mut found = Vec::new();
    for (key, kind) in TYPE_LISTS {
        for value in api.list(key) {
            let Some(name) = value["name"].as_str() else {
                continue;
            };
            found.push(Entry {
                path: name.to_owned(),
                kind,
                doc_key: name.to_owned(),
            });
            for (member_kind, member) in Api::own_members(value, kind) {
                let Some(member_name) = member["name"].as_str() else {
                    continue;
                };
                let path = format!("{name}.{member_name}");
                found.push(Entry {
                    path: path.clone(),
                    kind: member_kind,
                    doc_key: path,
                });
            }
        }
    }
    for (key, kind) in [("functions", "function"), ("constants", "constant")] {
        for value in api.list(key) {
            let Some(name) = value["name"].as_str() else {
                continue;
            };
            found.push(Entry {
                path: name.to_owned(),
                kind,
                doc_key: name.to_owned(),
            });
        }
    }
    for object in api.list("objects") {
        let Some(name) = object["name"].as_str() else {
            continue;
        };
        found.push(Entry {
            path: name.to_owned(),
            kind: "object",
            doc_key: name.to_owned(),
        });
        object_paths(api, name, bound_type(&object["type"]), 2, &mut found);
    }
    found
}

/// The property paths below `path` (of type `ty`), `depth` levels deep.
fn object_paths(api: &Api, path: &str, ty: Option<&str>, depth: usize, found: &mut Vec<Entry>) {
    let Some((kind, value)) = ty.and_then(|ty| api.find_type(ty)) else {
        return;
    };
    if depth == 0 {
        return;
    }
    let owner = value["name"].as_str().unwrap_or_default();
    for (member_kind, member) in Api::own_members(value, kind) {
        let Some(name) = member["name"].as_str() else {
            continue;
        };
        let child = format!("{path}.{name}");
        found.push(Entry {
            path: child.clone(),
            kind: member_kind,
            doc_key: format!("{owner}.{name}"),
        });
        if member_kind == "property" {
            object_paths(api, &child, bound_type(&member["type"]), depth - 1, found);
        }
    }
}

/// How well `entry` matches `query` (lower case); lower is better, None is no match.
fn score(entry: &Entry, query: &str) -> Option<u8> {
    let path = entry.path.to_lowercase();
    let last = path.rsplit('.').next().unwrap_or_default();
    let wanted = query.rsplit('.').next().unwrap_or_default();
    if path == query {
        Some(0)
    } else if last == wanted {
        Some(1)
    } else if last.starts_with(wanted) {
        Some(2)
    } else if last.contains(wanted) {
        Some(3)
    } else if path.contains(query) {
        Some(4)
    } else if wanted.len() >= 4 && distance(last, wanted) <= 2 {
        Some(5)
    } else {
        None
    }
}

/// The symbols matching `query`, best first, and how many matched in all.
fn search(api: &Api, docs: Option<&Docs>, query: &str) -> (Vec<Value>, usize) {
    let query = query.trim().to_lowercase();
    let mut scored: Vec<(u8, Entry)> = entries(api)
        .into_iter()
        .filter_map(|entry| Some((score(&entry, &query)?, entry)))
        .collect();
    scored.sort_by(|(a, x), (b, y)| (a, x.path.len(), &x.path).cmp(&(b, y.path.len(), &y.path)));
    let total = scored.len();
    let matches = scored
        .into_iter()
        .take(MAX_MATCHES)
        .map(|(_, entry)| {
            let mut found = json!({ "symbol": entry.path, "kind": entry.kind });
            if let Some(summary) = docs
                .and_then(|docs| docs.get(&entry.doc_key))
                .map(Doc::summary)
                .filter(|summary| !summary.is_empty())
            {
                found["summary"] = summary.into();
            }
            found
        })
        .collect();
    (matches, total)
}

/// The names a module version declares, read without keeping its details.
#[derive(Debug, Default, Deserialize)]
struct Names {
    #[serde(default, rename = "parentModule")]
    parent: Option<Value>,
    #[serde(default)]
    classes: Vec<TypeNames>,
    #[serde(default)]
    interfaces: Vec<TypeNames>,
    #[serde(default)]
    enums: Vec<TypeNames>,
    #[serde(default)]
    errors: Vec<TypeNames>,
    #[serde(default)]
    type_aliases: Vec<Named>,
    #[serde(default)]
    objects: Vec<Named>,
    #[serde(default)]
    functions: Vec<Named>,
    #[serde(default)]
    constants: Vec<Named>,
}

#[derive(Debug, Deserialize)]
struct TypeNames {
    name: String,
    #[serde(default)]
    functions: Vec<Named>,
    #[serde(default)]
    properties: Vec<Named>,
    #[serde(default)]
    constants: Vec<Named>,
}

#[derive(Debug, Deserialize)]
struct Named {
    name: String,
}

impl Names {
    fn read(dir: &Path, path: &Path) -> Option<Self> {
        let names: Self = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
        match &names.parent {
            Some(_) => {
                let api = api_path(dir, path, names.parent.as_ref()).ok()?;
                serde_json::from_slice(&fs::read(api).ok()?).ok()
            }
            None => Some(names),
        }
    }

    /// Whether this version has the symbol the docs list as `doc_key`.
    fn has(&self, doc_key: &str) -> bool {
        let (owner, member) = match doc_key.split_once('.') {
            Some((owner, member)) => (owner, Some(member)),
            None => (doc_key, None),
        };
        let types = self
            .classes
            .iter()
            .chain(&self.interfaces)
            .chain(&self.enums)
            .chain(&self.errors);
        let named = |list: &[Named], name: &str| list.iter().any(|item| item.name == name);
        if let Some(member) = member {
            return types.filter(|ty| ty.name == owner).any(|ty| {
                named(&ty.functions, member)
                    || named(&ty.properties, member)
                    || named(&ty.constants, member)
            });
        }
        types.into_iter().any(|ty| ty.name == owner)
            || [
                &self.type_aliases,
                &self.objects,
                &self.functions,
                &self.constants,
            ]
            .into_iter()
            .any(|list| named(list, owner))
    }
}

/// The oldest stable version of the chosen module's major version that has `doc_key`, and
/// whether only beta versions have it. Versions of one major only add API, so a binary
/// search finds the first that has it.
fn since(
    files: &[ModuleFile],
    dir: &Path,
    chosen: &ModuleFile,
    doc_key: &str,
) -> (Option<String>, bool) {
    let (chosen_numbers, _) = version_key(&chosen.version);
    let major = chosen_numbers.first().copied();
    let older: Vec<&ModuleFile> = files
        .iter()
        .filter(|file| file.name == chosen.name && !file.is_beta())
        .filter(|file| version_key(&file.version).0.first().copied() == major)
        .filter(|file| version_key(&file.version) <= version_key(&chosen.version))
        .collect();
    let has =
        |file: &ModuleFile| Names::read(dir, &file.path).is_some_and(|names| names.has(doc_key));
    let first = older.partition_point(|file| !has(file));
    match older.get(first) {
        Some(file) => (Some(file.version.clone()), false),
        None if chosen.is_beta() => (Some(chosen.version.clone()), true),
        None => (None, false),
    }
}

/// The docs fields of a symbol.
fn doc_fields(result: &mut Value, doc: &Doc) {
    if !doc.description.is_empty() {
        result["description"] = doc.description.clone().into();
    }
    if let Some(deprecated) = &doc.deprecated {
        result["deprecated"] = deprecated.clone().into();
    }
    if !doc.examples.is_empty() {
        result["examples"] = doc
            .examples
            .iter()
            .map(|(name, code)| json!({ "name": name, "code": code }))
            .collect();
    }
}

fn parameters(member: &Value, doc: Option<&Doc>) -> Vec<Value> {
    member["arguments"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|argument| {
            let name = argument["name"].as_str().unwrap_or_default();
            let ty = &argument["type"];
            let optional = ty["name"] == "optional";
            let shown = if optional { &ty["optional_type"] } else { ty };
            let mut parameter =
                json!({ "name": name, "type": render(shown), "optional": optional });
            let details = &argument["details"];
            if let Some(default) = details
                .get("default_value")
                .filter(|value| !matches!(value.as_str(), Some("null")) && !value.is_null())
            {
                parameter["default"] = default.clone();
            }
            for (key, field) in [("min_value", "min"), ("max_value", "max")] {
                if let Some(limit) = details.get(key) {
                    parameter[field] = limit.clone();
                }
            }
            if let Some(text) = doc
                .and_then(|doc| doc.param(name))
                .filter(|text| !text.is_empty())
            {
                parameter["description"] = text.into();
            }
            parameter
        })
        .collect()
}

/// The members of a type, each with its signature and summary.
fn members(kind: &str, value: &Value, docs: Option<&Docs>) -> (Vec<Value>, usize) {
    let owner = value["name"].as_str().unwrap_or_default();
    let all: Vec<(&str, &Value)> = Api::own_members(value, kind).collect();
    let total = all.len();
    let listed = all
        .into_iter()
        .take(MAX_MEMBERS)
        .map(|(member_kind, member)| {
            let name = member["name"].as_str().unwrap_or_default();
            let mut entry = json!({ "name": name, "kind": member_kind, "signature": member_signature(member_kind, member) });
            if let Some(summary) = docs
                .and_then(|docs| docs.get(&format!("{owner}.{name}")))
                .map(Doc::summary)
                .filter(|summary| !summary.is_empty())
            {
                entry["summary"] = summary.into();
            }
            entry
        })
        .collect();
    (listed, total)
}

/// The full reply fields for a found symbol.
fn detail(api: &Api, hit: &Hit, docs: Option<&Docs>) -> Value {
    let doc = docs.and_then(|docs| docs.get(&hit.doc_key));
    let value = hit.value;
    let mut result = json!({ "symbol": hit.path, "kind": hit.kind });
    match hit.kind {
        "method" | "constructor" | "function" => {
            let returns = &value["return_type"];
            result["signature"] = member_signature(hit.kind, value).into();
            result["parameters"] = parameters(value, doc).into();
            let mut returned = json!({ "type": render(returns) });
            if let Some(text) = doc.and_then(|doc| doc.returns.clone()) {
                returned["description"] = text.into();
            }
            result["returns"] = returned;
            result["throws"] = errors(returns).into();
            result["privileges"] = privileges(value, "call_privilege").into();
            result["static"] = (value["is_static"] == true).into();
        }
        "property" | "object" => {
            result["signature"] = member_signature("property", value).into();
            result["type"] = render(&value["type"]).into();
            result["read_only"] = (value["is_read_only"] == true).into();
            result["throws"] = errors(&value["type"]).into();
            result["privileges"] = privileges(value, "get_privilege").into();
            let set = privileges(value, "set_privilege");
            if !set.is_empty() {
                result["set_privileges"] = set.into();
            }
            if hit.kind == "object"
                && let Some((kind, ty)) =
                    bound_type(&value["type"]).and_then(|name| api.find_type(name))
            {
                let (listed, total) = members(kind, ty, docs);
                result["members"] = listed.into();
                result["member_count"] = total.into();
            }
        }
        "constant" | "enum_value" => {
            result["signature"] = member_signature(hit.kind, value).into();
            result["type"] = render(&value["type"]).into();
            result["value"] = value["value"].clone();
        }
        "type_alias" => {
            result["alias_type"] = value["alias_type"].clone();
            result["mappings"] = value["mappings"].clone();
        }
        kind => {
            let bases: Vec<&str> = value["base_types"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|base| base["name"].as_str())
                .collect();
            let extends = if bases.is_empty() {
                String::new()
            } else {
                format!(" extends {}", bases.join(", "))
            };
            result["signature"] = format!("{kind} {}{extends}", hit.path).into();
            result["base_types"] = bases.into();
            let (listed, total) = members(kind, value, docs);
            result[if kind == "enum" { "values" } else { "members" }] = listed.into();
            result["member_count"] = total.into();
        }
    }
    if let Some(doc) = doc {
        if !doc.declaration.is_empty()
            && !matches!(hit.kind, "class" | "interface" | "enum" | "error")
        {
            result["signature"] = doc.declaration.clone().into();
        }
        doc_fields(&mut result, doc);
    }
    result
}

/// What the reply is about: the release, the module file and the docs package.
struct Context<'a> {
    samples: &'a Samples,
    files: Vec<ModuleFile>,
    dir: PathBuf,
    chosen: ModuleFile,
    notes: Vec<String>,
}

/// The reply for `request` from `samples`, with docs from `load_docs(package, version)`.
fn answer(
    samples: &Samples,
    request: &Lookup,
    load_docs: impl Fn(&str, &str) -> Result<Arc<Docs>>,
) -> Result<Value> {
    let text = match (&request.symbol, &request.query) {
        (Some(symbol), None) => symbol,
        (None, Some(query)) => query,
        _ => bail!("Give symbol or query, not both."),
    };
    let dir = samples.metadata().join("script_modules").join("@minecraft");
    let files = module_files(&dir);
    let name = module_name(request.module.as_deref());
    let (chosen, note) = choose(&files, &name, request.version.as_deref())?;
    let mut context = Context {
        samples,
        files,
        dir,
        chosen,
        notes: samples.notes.clone(),
    };
    context.notes.extend(note);
    let module = parsed(&context.chosen.path)?;
    let api_file = api_path(
        &context.dir,
        &context.chosen.path,
        module.get("parentModule"),
    )?;
    let data = parsed(&api_file)?;
    let api = Api(&data);
    let npm = npm_version(&context.chosen.version, &samples.release);
    let docs = if name.ends_with("-bindings") {
        context
            .notes
            .push(format!("{name} has no npm package, so no descriptions."));
        None
    } else {
        match load_docs(&name, &npm) {
            Ok(docs) => Some(docs),
            Err(error) => {
                context.notes.push(format!(
                    "No descriptions: fetching {name}@{npm} from npm failed: {error}"
                ));
                None
            }
        }
    };
    let mut result = labelled(&context, docs.is_some().then(|| format!("{name}@{npm}")));
    let docs = docs.as_deref();
    if request.query.is_some() {
        let (matches, total) = search(&api, docs, text);
        result["matches"] = matches.into();
        result["match_count"] = total.into();
        return Ok(result);
    }
    let Some(hit) = resolve(&api, text) else {
        let (matches, total) = search(&api, docs, text);
        result["found"] = false.into();
        result["matches"] = matches.into();
        result["match_count"] = total.into();
        return Ok(result);
    };
    let (first, beta_only) = since(&context.files, &context.dir, &context.chosen, &hit.doc_key);
    result["found"] = true.into();
    if let Value::Object(fields) = detail(&api, &hit, docs) {
        result
            .as_object_mut()
            .expect("labelled is an object")
            .extend(fields);
    }
    result["since"] = first.into();
    if beta_only {
        result["beta_only"] = true.into();
    }
    Ok(result)
}

fn labelled(context: &Context, docs: Option<String>) -> Value {
    let mut source = context.samples.source();
    source["docs"] = docs.map_or(Value::Null, |package| format!("npm {package}").into());
    json!({
        "source": source,
        "module": context.chosen.name,
        "module_version": context.chosen.version,
        "channel": if context.chosen.is_beta() { "beta" } else { "stable" },
        "notes": context.notes,
    })
}

/// The docs of npm package `name@version`, fetched on first use and kept for the process.
fn npm_docs(name: &str, version: &str) -> Result<Arc<Docs>> {
    static LOADED: Mutex<Vec<(String, Arc<Docs>)>> = Mutex::new(Vec::new());
    let key = format!("{name}@{version}");
    let mut loaded = LOADED.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some((_, docs)) = loaded.iter().find(|(cached, _)| *cached == key) {
        return Ok(docs.clone());
    }
    let index = ensure_package(name, version)?.join("index.d.ts");
    let text = fs::read_to_string(&index)
        .map_err(|error| Error::msg(format!("Cannot read {}: {error}", index.display())))?;
    let docs = Arc::new(dts::parse(&text));
    if loaded.len() >= MAX_PARSED {
        loaded.remove(0);
    }
    loaded.push((key, docs.clone()));
    Ok(docs)
}

pub fn lookup(install: &Install, request: &Lookup) -> Result<Value> {
    let samples = samples::open(install)?;
    answer(&samples, request, npm_docs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir::TestDir;

    fn ty(name: &str, bind: bool) -> Value {
        json!({ "name": name, "is_bind_type": bind, "is_errorable": false })
    }

    /// Bindings for version `version`: Entity (with applyDamage only from 1.1.0), World
    /// with afterEvents, WorldAfterEvents with entityDie, and the `world` object.
    fn bindings(with_damage: bool) -> Value {
        let mut entity_functions = vec![];
        if with_damage {
            entity_functions.push(json!({
                "name": "applyDamage",
                "is_constructor": false,
                "is_static": false,
                "call_privilege": [{ "name": "default" }],
                "arguments": [
                    { "name": "amount", "details": { "min_value": 0 }, "type": ty("float", false) },
                    { "name": "options", "details": { "default_value": "null" }, "type": { "name": "optional", "is_bind_type": false, "optional_type": ty("EntityApplyDamageOptions", true) } }
                ],
                "return_type": { "name": "boolean", "is_bind_type": false, "is_errorable": true, "error_types": [
                    { "name": "EngineError", "is_bind_type": true, "from_module": { "name": "@minecraft/common" } },
                    { "name": "InvalidEntityError", "is_bind_type": true }
                ] }
            }));
        }
        json!({
            "name": "@minecraft/server-bindings",
            "classes": [
                { "name": "Entity", "base_types": [], "functions": entity_functions, "properties": [
                    { "name": "id", "is_read_only": true, "type": ty("string", false) }
                ], "constants": [] },
                { "name": "Player", "base_types": [ty("Entity", true)], "functions": [], "properties": [], "constants": [] },
                { "name": "World", "base_types": [], "functions": [], "properties": [
                    { "name": "afterEvents", "is_read_only": true, "type": ty("WorldAfterEvents", true) }
                ], "constants": [] },
                { "name": "WorldAfterEvents", "base_types": [], "functions": [], "properties": [
                    { "name": "entityDie", "is_read_only": true, "type": ty("EntityDieAfterEventSignal", true) }
                ], "constants": [] },
                { "name": "EntityDieAfterEventSignal", "base_types": [], "functions": [], "properties": [], "constants": [] }
            ],
            "interfaces": [], "errors": [], "type_aliases": [], "functions": [], "constants": [],
            "enums": [{ "name": "DamageCause", "constants": [{ "name": "fall", "value": "fall", "type": ty("string", false) }] }],
            "objects": [{ "name": "world", "is_read_only": true, "is_static": true, "type": ty("World", true) }]
        })
    }

    fn fixture() -> (TestDir, Samples) {
        let temp = TestDir::new("script-api");
        let dir = temp.path().join("metadata/script_modules/@minecraft");
        fs::create_dir_all(&dir).unwrap();
        let write =
            |file: &str, value: &Value| fs::write(dir.join(file), value.to_string()).unwrap();
        for (version, with_damage) in [
            ("1.0.0", false),
            ("1.1.0", true),
            ("1.2.0", true),
            ("1.3.0-beta", true),
        ] {
            write(
                &format!("server-bindings_{version}.json"),
                &bindings(with_damage),
            );
            write(
                &format!("server_{version}.json"),
                &json!({ "name": "@minecraft/server", "parentModule": { "name": "@minecraft/server-bindings", "version": version } }),
            );
        }
        let samples = Samples {
            release: Release::parse("v1.0.0.1").unwrap(),
            dir: temp.path().to_owned(),
            game: None,
            notes: vec![],
        };
        (temp, samples)
    }

    fn docs(_: &str, _: &str) -> Result<Arc<Docs>> {
        Ok(Arc::new(dts::parse(dts::tests::INDEX)))
    }

    fn lookup(samples: &Samples, request: Value) -> Value {
        answer(samples, &serde_json::from_value(request).unwrap(), docs).unwrap()
    }

    #[test]
    fn a_method_joins_its_metadata_and_its_docs() {
        let (_temp, samples) = fixture();
        let reply = lookup(&samples, json!({ "symbol": "Entity.applyDamage" }));
        assert_eq!(reply["found"], true);
        assert_eq!(reply["module_version"], "1.2.0");
        assert_eq!(reply["source"]["docs"], "npm @minecraft/server@1.2.0");
        assert_eq!(reply["kind"], "method");
        assert_eq!(reply["since"], "1.1.0");
        assert_eq!(
            reply["parameters"][0],
            json!({ "name": "amount", "type": "number", "optional": false, "min": 0, "description": "Amount of damage to apply." })
        );
        assert_eq!(reply["parameters"][1]["type"], "EntityApplyDamageOptions");
        assert_eq!(reply["parameters"][1]["optional"], true);
        assert_eq!(
            reply["returns"],
            json!({ "type": "boolean", "description": "Whether the entity takes any damage." })
        );
        assert_eq!(
            reply["throws"],
            json!(["@minecraft/common.EngineError", "InvalidEntityError"])
        );
        assert_eq!(
            reply["description"],
            "Applies a set of damage to an entity."
        );
        assert!(
            reply["signature"]
                .as_str()
                .unwrap()
                .starts_with("applyDamage(amount: number")
        );
        // Inherited members resolve to the class that declares them.
        let inherited = lookup(&samples, json!({ "symbol": "Player.applyDamage" }));
        assert_eq!(inherited["symbol"], "Player.applyDamage");
        assert_eq!(
            inherited["description"],
            "Applies a set of damage to an entity."
        );
    }

    #[test]
    fn event_paths_resolve_through_property_types() {
        let (_temp, samples) = fixture();
        let reply = lookup(&samples, json!({ "symbol": "world.afterEvents.entityDie" }));
        assert_eq!(reply["kind"], "property");
        assert_eq!(reply["type"], "EntityDieAfterEventSignal");
        assert_eq!(reply["since"], "1.0.0");
        let found = lookup(&samples, json!({ "query": "entitydie" }));
        let symbols: Vec<&str> = found["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["symbol"].as_str().unwrap())
            .collect();
        assert_eq!(
            symbols[..2],
            ["WorldAfterEvents.entityDie", "world.afterEvents.entityDie"]
        );
        assert!(symbols.contains(&"EntityDieAfterEventSignal"));
    }

    #[test]
    fn unknown_symbols_list_close_matches() {
        let (_temp, samples) = fixture();
        let reply = lookup(&samples, json!({ "symbol": "Entity.applyDamge" }));
        assert_eq!(reply["found"], false);
        assert_eq!(reply["matches"][0]["symbol"], "Entity.applyDamage");
        assert_eq!(
            reply["matches"][0]["summary"],
            "Applies a set of damage to an entity."
        );
    }

    #[test]
    fn versions_pick_stable_beta_or_exact_files() {
        let (_temp, samples) = fixture();
        let beta = lookup(&samples, json!({ "symbol": "Entity", "version": "beta" }));
        assert_eq!(
            (beta["module_version"].as_str(), beta["channel"].as_str()),
            (Some("1.3.0-beta"), Some("beta"))
        );
        assert_eq!(
            beta["source"]["docs"],
            "npm @minecraft/server@1.3.0-beta.1.0.0-stable"
        );
        assert_eq!(beta["members"][0]["name"], "applyDamage");
        let old = lookup(
            &samples,
            json!({ "symbol": "Entity.applyDamage", "version": "1.0.0" }),
        );
        assert_eq!(old["found"], false);
        let npm = lookup(
            &samples,
            json!({ "symbol": "DamageCause.fall", "version": "1.3.0-beta.1.0.0-stable" }),
        );
        assert_eq!(
            (npm["kind"].as_str(), npm["value"].as_str()),
            (Some("enum_value"), Some("fall"))
        );
        let error = answer(
            &samples,
            &serde_json::from_value(json!({ "symbol": "x", "module": "server-ui" })).unwrap(),
            docs,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("modules: @minecraft/server"),
            "{error}"
        );
    }

    #[test]
    fn missing_docs_are_noted_not_fatal() {
        let (_temp, samples) = fixture();
        let request = serde_json::from_value(json!({ "symbol": "Entity.id" })).unwrap();
        let reply = answer(&samples, &request, |_, _| bail!("offline")).unwrap();
        assert_eq!(reply["signature"], "readonly id: string");
        assert_eq!(reply["source"]["docs"], Value::Null);
        assert!(reply["notes"][0].as_str().unwrap().contains("offline"));
    }
}
