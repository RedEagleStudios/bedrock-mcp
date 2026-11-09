//! Bedrock JSON schemas from rockide/schemas (https://github.com/rockide/schemas): the source
//! archive of one pinned release, downloaded once with curl into the cache. Its
//! `scripts/compile.ts` maps pack file globs to the entry schemas under `schema/`.

use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    fs, io,
    path::{Component, Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use serde::Deserialize;
use serde_json::{Value, json};

use super::{cache_dir, download};
use crate::error::{Error, Result, bail};

pub const REPOSITORY: &str = "rockide/schemas";
/// The rockide/schemas release in use; bump it when a new game release needs newer schemas.
pub const PINNED: &str = "0.1.4";
/// The file in the release that lists the document types and their file globs.
const ENTRIES: &str = "scripts/compile.ts";
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);
/// More than the release unpacks to (about 6 MB); stops a broken archive filling the disk.
const MAX_EXTRACTED: u64 = 256 << 20;
const MAX_COMPONENTS: usize = 200;
/// Schema nodes one search follows through `$ref`s at most.
const MAX_VISITS: usize = 5000;

/// The source archive of a release.
pub fn archive_url(version: &str) -> String {
    format!("https://github.com/{REPOSITORY}/archive/refs/tags/{version}.zip")
}

/// An extracted release holds the document list and the schema tree.
fn is_complete(dir: &Path) -> bool {
    dir.join(ENTRIES).is_file() && dir.join("schema").join("bedrock").is_dir()
}

/// The release `version` under `home`, fetched with `fetch` (which writes the source archive
/// to the path it is given) when it is missing or incomplete. It is unpacked in a staging
/// folder and renamed into place, so an interrupted download is never taken as cached. The
/// note says when an incomplete copy was replaced.
fn ensure_in(
    home: &Path,
    version: &str,
    fetch: impl FnOnce(&Path) -> Result<()>,
) -> Result<(PathBuf, Option<String>)> {
    let dir = home.join(version);
    if is_complete(&dir) {
        return Ok((dir, None));
    }
    let note = dir.exists().then(|| {
        let _ = fs::remove_dir_all(&dir);
        format!("The cached {REPOSITORY} {version} was incomplete, so it was downloaded again.")
    });
    let staging = home.join(format!("{version}.staging-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;
    let archive = staging.join("source.zip");
    let unpacked = fetch(&archive).and_then(|()| extract(&archive, &staging));
    let _ = fs::remove_file(&archive);
    if let Err(error) = unpacked {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    if !is_complete(&staging) {
        let _ = fs::remove_dir_all(&staging);
        bail!(
            "The {REPOSITORY} {version} archive ({}) lacks {ENTRIES} or schema/bedrock.",
            archive_url(version)
        );
    }
    // Another server may have finished the same download first; either copy will do.
    if fs::rename(&staging, &dir).is_err() {
        let _ = fs::remove_dir_all(&staging);
        if !is_complete(&dir) {
            bail!("Cannot move the downloaded schemas into {}.", dir.display());
        }
    }
    Ok((dir, note))
}

/// Unpack `schema/**` and the document list from a GitHub source archive, whose members all
/// sit under one top folder (`schemas-0.1.4/`), into `to`.
fn extract(archive: &Path, to: &Path) -> Result<()> {
    let zip_error =
        |error: zip::result::ZipError| Error::msg(format!("{}: {error}", archive.display()));
    let mut zip = zip::ZipArchive::new(fs::File::open(archive)?).map_err(zip_error)?;
    let mut written = 0;
    for index in 0..zip.len() {
        let mut member = zip.by_index(index).map_err(zip_error)?;
        let name = member.name().map_err(zip_error)?.into_owned();
        let Some((_, relative)) = name.split_once('/') else {
            continue;
        };
        if member.is_dir() || !(relative.starts_with("schema/") || relative == ENTRIES) {
            continue;
        }
        let path = Path::new(relative);
        if !path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        {
            bail!("{} contains an unsafe path: {name}", archive.display());
        }
        let target = to.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut limited = io::Read::take(&mut member, MAX_EXTRACTED - written);
        written += io::copy(&mut limited, &mut fs::File::create(&target)?)?;
        if written >= MAX_EXTRACTED {
            bail!(
                "{} unpacks to more than {} MB.",
                archive.display(),
                MAX_EXTRACTED >> 20
            );
        }
    }
    Ok(())
}

/// One entry of the `*_ENTRIES` lists in `scripts/compile.ts`.
#[derive(Debug, PartialEq)]
struct Entry {
    pack: &'static str,
    globs: Vec<String>,
    url: String,
}

/// The entries of `BP_ENTRIES`, `RP_ENTRIES` and `SP_ENTRIES`, which are written one key per
/// line: `fileMatch: "glob"` or `fileMatch: [` with one glob per line, then `url: "./schema/..."`.
fn parse_entries(text: &str) -> Vec<Entry> {
    let strings = |line: &str| -> Vec<String> {
        line.split('"')
            .skip(1)
            .step_by(2)
            .map(str::to_owned)
            .collect()
    };
    let mut entries = Vec::new();
    let mut pack = None;
    let mut globs = Vec::new();
    let mut in_globs = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("const ") || line.starts_with("function ") {
            pack = match line.split_whitespace().nth(1).unwrap_or_default() {
                "BP_ENTRIES:" | "BP_ENTRIES" => Some("behavior"),
                "RP_ENTRIES:" | "RP_ENTRIES" => Some("resource"),
                "SP_ENTRIES:" | "SP_ENTRIES" => Some("skin"),
                _ => None,
            };
            continue;
        }
        let Some(pack) = pack else { continue };
        if in_globs {
            if line.starts_with(']') {
                in_globs = false;
            } else {
                globs.extend(strings(line));
            }
        } else if let Some(rest) = line.strip_prefix("fileMatch:") {
            globs = strings(rest);
            in_globs = rest.trim_end().ends_with('[');
        } else if let Some(rest) = line.strip_prefix("url:")
            && let Some(url) = strings(rest).into_iter().next()
        {
            entries.push(Entry {
                pack,
                globs: std::mem::take(&mut globs),
                url,
            });
        }
    }
    entries
}

/// `bp/entities` for `entities/**/*.{json,jsonc,json5}`, `rp/textures/terrain_texture` for
/// `textures/terrain_texture.{json,jsonc,json5}`: the pack, then the glob's literal parts.
fn document_id(pack: &str, globs: &[String]) -> String {
    let prefix = match pack {
        "behavior" => "bp",
        "resource" => "rp",
        _ => "sp",
    };
    let glob = globs
        .iter()
        .find(|glob| !glob.starts_with('!'))
        .map_or("", String::as_str);
    let mut parts = vec![prefix.to_owned()];
    for part in glob.split('/') {
        let part = match part.find(".{") {
            Some(end) => &part[..end],
            None => part,
        };
        let part = part
            .strip_prefix("*.")
            .unwrap_or(part)
            .trim_start_matches('*');
        if !part.is_empty() {
            parts.push(part.to_owned());
        }
    }
    parts.join("/")
}

/// A schema file id (`bedrock/server/entity/_index`, its path under `schema/` without
/// `.json`) and a JSON pointer into it (empty for the whole file).
type Node = (String, String);

/// `file` with `#pointer` when the pointer is not empty.
fn node_id((file, pointer): &Node) -> String {
    if pointer.is_empty() {
        file.clone()
    } else {
        format!("{file}#{pointer}")
    }
}

/// The node a `$ref` in `from` points to; None when it leaves the schema tree or is not a
/// JSON file.
fn resolve_ref(from: &str, reference: &str) -> Option<Node> {
    let (file, pointer) = reference.split_once('#').unwrap_or((reference, ""));
    if !pointer.is_empty() && !pointer.starts_with('/') {
        return None;
    }
    if file.is_empty() {
        return Some((from.to_owned(), pointer.to_owned()));
    }
    let mut parts: Vec<&str> = from.split('/').collect();
    parts.pop();
    for part in file.strip_suffix(".json")?.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    Some((parts.join("/"), pointer.to_owned()))
}

/// A schema id as given: `file` or `file#/pointer`, with or without `.json`.
fn parse_id(id: &str) -> Result<Node> {
    let (file, pointer) = id.trim().split_once('#').unwrap_or((id.trim(), ""));
    let file = file.trim_start_matches("./");
    let file = file.strip_suffix(".json").unwrap_or(file);
    if file.is_empty()
        || file.contains('\\')
        || file
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || !(pointer.is_empty() || pointer.starts_with('/'))
    {
        bail!("{id:?} is not a schema id; schema_list shows the document ids.");
    }
    Ok((file.to_owned(), pointer.to_owned()))
}

/// JSON with `//` and `/* */` comments removed, outside strings.
fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                '\\' => out.extend(chars.next()),
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut last = ' ';
                for c in chars.by_ref() {
                    if last == '*' && c == '/' {
                        break;
                    }
                    last = c;
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// A format_version with its own schema: `then` of an `if format_version ...` in the entry.
#[derive(Clone, Debug)]
struct Variant {
    /// Its folder, e.g. `1.26.60`.
    name: String,
    /// The format_version values the entry routes to it.
    accepts: Vec<String>,
    file: String,
}

/// `1.21.130` as numbers, for ordering.
fn version_key(text: &str) -> Vec<u32> {
    text.split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

/// The variants an entry schema chooses between by format_version, oldest first.
fn variants(entry: &str, schema: &Value) -> Vec<Variant> {
    let folder = entry.rsplit_once('/').map_or("", |(folder, _)| folder);
    let mut found: Vec<Variant> = Vec::new();
    for item in schema["allOf"].as_array().into_iter().flatten() {
        let Some(condition) = item.get("if") else {
            continue;
        };
        let wanted = &condition["properties"]["format_version"];
        let accepts: Vec<String> = wanted
            .get("const")
            .into_iter()
            .chain(wanted["enum"].as_array().into_iter().flatten())
            .map(|value| match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            })
            .collect();
        for (branch, accepts) in [("then", accepts), ("else", Vec::new())] {
            let Some((file, pointer)) = item[branch]["$ref"]
                .as_str()
                .and_then(|reference| resolve_ref(entry, reference))
            else {
                continue;
            };
            let Some(name) = file
                .strip_prefix(folder)
                .and_then(|rest| rest.strip_prefix('/'))
                .and_then(|rest| rest.strip_suffix("/_index"))
                .filter(|name| pointer.is_empty() && !name.contains('/'))
            else {
                continue;
            };
            match found.iter_mut().find(|variant| variant.name == name) {
                Some(variant) => variant.accepts.extend(accepts),
                None => found.push(Variant {
                    name: name.to_owned(),
                    accepts,
                    file,
                }),
            }
        }
    }
    found.sort_by_key(|variant| version_key(&variant.name));
    found
}

/// One pack file type.
#[derive(Debug)]
struct Document {
    id: String,
    pack: &'static str,
    globs: Vec<String>,
    /// The entry schema's file id.
    entry: String,
    title: String,
    description: String,
    variants: Vec<Variant>,
}

impl Document {
    /// The schema for `format_version`, else the newest variant, else the entry itself.
    fn root(&self, format_version: Option<&str>) -> Result<(String, Option<&str>)> {
        let Some(wanted) = format_version.map(str::trim) else {
            return Ok(match self.variants.last() {
                Some(newest) => (newest.file.clone(), Some(newest.name.as_str())),
                None => (self.entry.clone(), None),
            });
        };
        if self.variants.is_empty() {
            bail!(
                "{} has one schema for every format_version; leave format_version out.",
                self.id
            );
        }
        match self
            .variants
            .iter()
            .find(|variant| variant.name == wanted)
            .or_else(|| {
                self.variants
                    .iter()
                    .find(|variant| variant.accepts.iter().any(|value| value == wanted))
            }) {
            Some(variant) => Ok((variant.file.clone(), Some(variant.name.as_str()))),
            None => bail!(
                "{} has no schema for format_version {wanted:?}; it has: {}.",
                self.id,
                self.variants
                    .iter()
                    .map(|variant| variant.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

/// Schema files read during one request, each parsed once.
struct Files<'a> {
    root: &'a Path,
    loaded: HashMap<String, Option<Rc<Value>>>,
}

impl<'a> Files<'a> {
    fn new(root: &'a Path) -> Self {
        Self {
            root,
            loaded: HashMap::new(),
        }
    }

    /// The file `id`, parsed as JSON (with comments allowed); None when missing or invalid.
    fn file(&mut self, id: &str) -> Option<Rc<Value>> {
        if let Some(found) = self.loaded.get(id) {
            return found.clone();
        }
        let parsed = fs::read_to_string(self.root.join(format!("{id}.json")))
            .ok()
            .and_then(|text| {
                serde_json::from_str(&text)
                    .or_else(|_| serde_json::from_str(&strip_comments(&text)))
                    .ok()
            })
            .map(Rc::new);
        self.loaded.insert(id.to_owned(), parsed.clone());
        parsed
    }

    fn node(&mut self, (file, pointer): &Node) -> Option<Value> {
        self.file(file)?.pointer(pointer).cloned()
    }

    /// The node `id` names, or an error saying why it cannot be read.
    fn read(&mut self, node: &Node) -> Result<Value> {
        let Some(schema) = self.file(&node.0) else {
            bail!(
                "No schema {:?}; schema_list shows the document ids, and each schema lists the ids it refers to in refs.",
                node.0
            );
        };
        schema
            .pointer(&node.1)
            .cloned()
            .ok_or_else(|| Error::msg(format!("Schema {} has no {:?}.", node.0, node.1)))
    }

    /// Breadth first from `start` through `$ref`s: `visit` sees each node once and stops the
    /// walk by returning Some.
    fn walk<T>(
        &mut self,
        start: Node,
        mut visit: impl FnMut(&Node, &Value) -> Option<T>,
    ) -> Option<T> {
        let mut queue = VecDeque::from([start.clone()]);
        let mut seen = HashSet::from([start]);
        while let Some(node) = queue.pop_front() {
            let Some(value) = self.node(&node) else {
                continue;
            };
            if let Some(found) = visit(&node, &value) {
                return Some(found);
            }
            for target in refs_in(&node.0, &value) {
                if seen.len() < MAX_VISITS && seen.insert(target.clone()) {
                    queue.push_back(target);
                }
            }
        }
        None
    }
}

/// Every `$ref` in `value`, resolved from `file`, in document order.
fn refs_in(file: &str, value: &Value) -> Vec<Node> {
    fn collect(node: &Value, refs: &mut Vec<String>) {
        match node {
            Value::Object(map) => {
                if let Some(Value::String(reference)) = map.get("$ref") {
                    refs.push(reference.clone());
                }
                map.values().for_each(|value| collect(value, refs));
            }
            Value::Array(items) => items.iter().for_each(|value| collect(value, refs)),
            _ => {}
        }
    }
    let mut refs = Vec::new();
    collect(value, &mut refs);
    refs.iter()
        .filter_map(|reference| resolve_ref(file, reference))
        .collect()
}

/// `~` and `/` escaped for a JSON pointer.
fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// The pointer (relative to `node`) of the first `properties[name]`, depth first, without
/// following `$ref`s.
fn find_property(node: &Value, name: &str, path: &str) -> Option<String> {
    match node {
        Value::Object(map) => {
            if map
                .get("properties")
                .is_some_and(|properties| properties.get(name).is_some())
            {
                return Some(format!("{path}/properties/{}", escape(name)));
            }
            map.iter().find_map(|(key, value)| {
                find_property(value, name, &format!("{path}/{}", escape(key)))
            })
        }
        Value::Array(items) => items
            .iter()
            .enumerate()
            .find_map(|(index, value)| find_property(value, name, &format!("{path}/{index}"))),
        _ => None,
    }
}

/// Every namespaced key (`minecraft:health`) under a `properties` in `node`, without following
/// `$ref`s; `skip_top` leaves out `node`'s own properties (the file's top-level keys).
fn namespaced_keys(node: &Value, skip_top: bool, names: &mut BTreeSet<String>) {
    match node {
        Value::Object(map) => {
            if !skip_top && let Some(Value::Object(properties)) = map.get("properties") {
                names.extend(properties.keys().filter(|key| key.contains(':')).cloned());
            }
            map.values()
                .for_each(|value| namespaced_keys(value, false, names));
        }
        Value::Array(items) => items
            .iter()
            .for_each(|value| namespaced_keys(value, false, names)),
        _ => {}
    }
}

/// The schema ids `value` (read from `node`) refers to; references back into `node` itself
/// are left out.
fn ref_ids(node: &Node, value: &Value) -> Vec<String> {
    let inside = |target: &Node| {
        target.0 == node.0 && (target.1 == node.1 || target.1.starts_with(&format!("{}/", node.1)))
    };
    refs_in(&node.0, value)
        .into_iter()
        .filter(|target| !inside(target))
        .map(|target| node_id(&target))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The schemas of one release: `root` is its `schema` folder.
pub struct Catalog {
    root: PathBuf,
    documents: Vec<Document>,
    /// The namespaced keys each document's newest schema reaches, computed on first use.
    components: OnceLock<Vec<BTreeSet<String>>>,
}

impl Catalog {
    /// Read the document list of the release extracted at `dir`.
    pub fn load(dir: &Path) -> Result<Self> {
        let list = dir.join(ENTRIES);
        let text = fs::read_to_string(&list)
            .map_err(|error| Error::msg(format!("Cannot read {}: {error}", list.display())))?;
        let root = dir.join("schema");
        let mut files = Files::new(&root);
        let mut documents: Vec<Document> = Vec::new();
        for entry in parse_entries(&text) {
            let Some(url) = entry
                .url
                .strip_prefix("./schema/")
                .and_then(|url| url.strip_suffix(".json"))
            else {
                continue;
            };
            let id = document_id(entry.pack, &entry.globs);
            if documents.iter().any(|document| document.id == id) {
                continue;
            }
            let schema = files.file(url).ok_or_else(|| {
                Error::msg(format!(
                    "{} names {}, which is missing or not valid JSON.",
                    list.display(),
                    entry.url
                ))
            })?;
            let text = |key: &str| schema[key].as_str().unwrap_or_default().to_owned();
            documents.push(Document {
                id,
                pack: entry.pack,
                variants: variants(url, &schema),
                title: text("title"),
                description: text("description"),
                globs: entry.globs,
                entry: url.to_owned(),
            });
        }
        if documents.is_empty() {
            bail!("{} lists no schemas.", list.display());
        }
        Ok(Self {
            root,
            documents,
            components: OnceLock::new(),
        })
    }

    /// The namespaced keys each document's newest schema reaches through its `$ref`s.
    fn components(&self) -> &[BTreeSet<String>] {
        self.components.get_or_init(|| {
            let mut files = Files::new(&self.root);
            self.documents
                .iter()
                .map(|document| {
                    let mut names = BTreeSet::new();
                    let Ok((start, _)) = document.root(None) else {
                        return names;
                    };
                    let start = (start, String::new());
                    files.walk(start.clone(), |node, value| {
                        namespaced_keys(value, *node == start, &mut names);
                        None::<()>
                    });
                    names
                })
                .collect()
        })
    }

    /// Document types whose id, title or description contains `query`, and the components
    /// whose name contains it.
    pub fn list(&self, query: Option<&str>) -> Value {
        let query = query
            .map(|query| query.trim().to_lowercase())
            .filter(|query| !query.is_empty());
        let components = self.components();
        let documents: Vec<Value> = self
            .documents
            .iter()
            .zip(components)
            .filter(|(document, _)| {
                query.as_ref().is_none_or(|query| {
                    [&document.id, &document.title, &document.description]
                        .iter()
                        .any(|text| text.to_lowercase().contains(query.as_str()))
                })
            })
            .map(|(document, names)| {
                json!({
                    "id": document.id,
                    "title": document.title,
                    "pack": document.pack,
                    "file_match": document.globs,
                    "format_versions": document.variants.iter().map(|variant| &variant.name).collect::<Vec<_>>(),
                    "component_count": names.len(),
                    "description": document.description,
                })
            })
            .collect();
        let mut result = json!({ "documents": documents });
        if let Some(query) = query {
            let mut found: Vec<Value> = self
                .documents
                .iter()
                .zip(components)
                .flat_map(|(document, names)| {
                    names
                        .iter()
                        .filter(|name| name.contains(query.as_str()))
                        .map(|name| json!({ "kind": document.id, "component": name }))
                })
                .collect();
            if found.len() > MAX_COMPONENTS {
                found.truncate(MAX_COMPONENTS);
                result["truncated"] = true.into();
            }
            result["components"] = found.into();
        }
        result
    }

    /// The document type `kind` names: its id; else its id without the pack (`entities`) or
    /// its title; else its schema folder (`server/entity`, `entity`).
    fn document(&self, kind: &str) -> Result<&Document> {
        let lower = kind.trim().to_lowercase();
        let short =
            |document: &Document| document.id.split_once('/').map(|(_, rest)| rest.to_owned());
        let folder = |document: &Document| {
            let folder = document
                .entry
                .rsplit_once('/')
                .map_or("", |(folder, _)| folder)
                .to_owned();
            folder == format!("bedrock/{lower}") || folder.rsplit('/').next() == Some(&lower)
        };
        let tests: [&dyn Fn(&Document) -> bool; 3] = [
            &|document| document.id == lower,
            &|document| {
                short(document).as_deref() == Some(&lower) || document.title.to_lowercase() == lower
            },
            &folder,
        ];
        for test in tests {
            let found: Vec<&Document> = self
                .documents
                .iter()
                .filter(|document| test(document))
                .collect();
            match found[..] {
                [] => continue,
                [one] => return Ok(one),
                _ => {
                    let ids: Vec<&str> =
                        found.iter().map(|document| document.id.as_str()).collect();
                    bail!(
                        "Schema kind {kind:?} is ambiguous; use one of: {}",
                        ids.join(", ")
                    )
                }
            }
        }
        bail!("No schema kind {kind:?}; schema_list shows the kinds.")
    }

    /// A schema node with the ids of the schemas it refers to.
    fn schema(&self, files: &mut Files, node: &Node) -> Result<Value> {
        let schema = files.read(node)?;
        let refs = ref_ids(node, &schema);
        Ok(json!({ "id": node_id(node), "schema": schema, "refs": refs }))
    }

    /// The schema `id` names: a document id (its schema for `format_version`, else the newest)
    /// or a schema id from refs.
    pub fn get(&self, id: &str, format_version: Option<&str>) -> Result<Value> {
        let mut files = Files::new(&self.root);
        if let Some(document) = self
            .documents
            .iter()
            .find(|document| document.id == id.trim())
        {
            return self.document_schema(&mut files, document, format_version);
        }
        if format_version.is_some() {
            bail!("format_version goes with kind or a document id, not a schema id.");
        }
        self.schema(&mut files, &parse_id(id)?)
    }

    fn document_schema(
        &self,
        files: &mut Files,
        document: &Document,
        format_version: Option<&str>,
    ) -> Result<Value> {
        let (file, chosen) = document.root(format_version)?;
        let mut result = self.schema(files, &(file, String::new()))?;
        result["kind"] = document.id.clone().into();
        result["format_version"] = chosen.into();
        Ok(result)
    }

    /// The schema of document type `kind`.
    pub fn kind(&self, kind: &str, format_version: Option<&str>) -> Result<Value> {
        let document = self.document(kind)?;
        self.document_schema(&mut Files::new(&self.root), document, format_version)
    }

    /// The schema of component `name` (`minecraft:` is assumed when no namespace is given) in
    /// document type `kind`: the first `properties[name]` the document's schema reaches,
    /// nearest first, and the schema its `$ref` points to when it has one.
    pub fn component(&self, kind: &str, name: &str, format_version: Option<&str>) -> Result<Value> {
        let document = self.document(kind)?;
        let (file, chosen) = document.root(format_version)?;
        let name = name.trim().to_lowercase();
        let name = if name.contains(':') {
            name
        } else {
            format!("minecraft:{name}")
        };
        let mut files = Files::new(&self.root);
        let found = files.walk((file, String::new()), |(file, pointer), value| {
            find_property(value, &name, "")
                .map(|relative| (file.clone(), format!("{pointer}{relative}")))
        });
        let Some(found) = found else {
            bail!(
                "No component {name} in {}; schema_list with a query lists component names.",
                document.id
            );
        };
        let value = files.read(&found)?;
        let target = value["$ref"]
            .as_str()
            .and_then(|reference| resolve_ref(&found.0, reference))
            .filter(|target| files.node(target).is_some())
            .unwrap_or(found);
        let mut result = self.schema(&mut files, &target)?;
        result["component"] = name.into();
        result["kind"] = document.id.clone().into();
        result["format_version"] = chosen.into();
        Ok(result)
    }
}

/// The pinned release's folder, downloaded on first use, and a note when an incomplete
/// cached copy was replaced.
pub fn release_dir() -> Result<(PathBuf, Option<String>)> {
    ensure_in(&cache_dir()?.join("rockide-schemas"), PINNED, |archive| {
        download(&archive_url(PINNED), archive, DOWNLOAD_TIMEOUT)
    })
}

/// The pinned release's catalog, downloaded on first use and kept for the process, and a
/// note when an incomplete cached copy was replaced.
fn catalog() -> Result<(Arc<Catalog>, Option<String>)> {
    static LOADED: Mutex<Option<Arc<Catalog>>> = Mutex::new(None);
    let home = cache_dir()?.join("rockide-schemas");
    let mut loaded = LOADED.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some(catalog) = loaded.as_ref()
        && is_complete(&home.join(PINNED))
    {
        return Ok((catalog.clone(), None));
    }
    let (dir, note) = release_dir()?;
    let catalog = Arc::new(Catalog::load(&dir)?);
    *loaded = Some(catalog.clone());
    Ok((catalog, note))
}

/// `fields` after where the schemas came from.
fn labelled(fields: Value, note: Option<String>) -> Value {
    let mut result = json!({
        "source": "rockide",
        "version": PINNED,
        "notes": note.into_iter().collect::<Vec<_>>(),
    });
    if let (Some(result), Value::Object(fields)) = (result.as_object_mut(), fields) {
        result.extend(fields);
    }
    result
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct List {
    #[serde(default)]
    pub query: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Get {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub component: Option<String>,
    #[serde(default)]
    pub format_version: Option<String>,
}

pub fn list(request: &List) -> Result<Value> {
    let (catalog, note) = catalog()?;
    Ok(labelled(catalog.list(request.query.as_deref()), note))
}

pub fn get(request: &Get) -> Result<Value> {
    let format_version = request.format_version.as_deref();
    let found = |catalog: &Catalog| match (&request.id, &request.kind, &request.component) {
        (Some(id), None, None) => catalog.get(id, format_version),
        (None, Some(kind), Some(component)) => catalog.component(kind, component, format_version),
        (None, Some(kind), None) => catalog.kind(kind, format_version),
        _ => bail!("Give id, or kind with an optional component."),
    };
    let (catalog, note) = catalog()?;
    Ok(labelled(found(&catalog)?, note))
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, io::Write};

    use super::*;
    use crate::testdir::TestDir;

    const COMPILE_TS: &str = r#"import path from "path/posix";

const PACK_PREFIXES = {
	bp: ["behavior_pack/", "*BP/"],
	rp: ["resource_pack/", "*RP/"],
};

const BP_ENTRIES: JsonValidationEntry[] = [
	{
		fileMatch: "manifest.{json,jsonc,json5}",
		url: "./schema/bedrock/shared/manifest/_index.json",
	},
	{
		fileMatch: "entities/**/*.{json,jsonc,json5}",
		url: "./schema/bedrock/server/entity/_index.json",
	},
];

const RP_ENTRIES: JsonValidationEntry[] = [
	{
		fileMatch: "attachables/**/*.{json,jsonc,json5}",
		url: "./schema/bedrock/client/attachable/_index.json",
	},
	{
		fileMatch: [
			"ui/**/*.{json,jsonc,json5}",
			"!ui/_ui_defs.{json,jsonc,json5}",
		],
		url: "./schema/bedrock/client/ui/_index.json",
	},
	{
		fileMatch: "textures/**/*.texture_set.{json,jsonc,json5}",
		url: "./schema/bedrock/client/texture_set/_index.json",
	},
];

function mapFileMatch(type: string, entry: JsonValidationEntry) {
	const url = "./schema/ignored.json";
}
"#;

    /// A release laid out like rockide/schemas 0.1.4, as `(path, contents)`.
    fn release() -> Vec<(String, String)> {
        let entity_events = json!({ "type": "object", "title": "Events" });
        let files = [
            ("scripts/compile.ts", COMPILE_TS.to_owned()),
            (
                "schema/bedrock/server/entity/_index.json",
                json!({
                    "title": "Minecraft Bedrock Entity File",
                    "properties": { "format_version": { "enum": ["1.8.0", "1.20.0", "1.21.0"] } },
                    "allOf": [
                        { "if": { "properties": { "format_version": { "const": "1.8.0" } } }, "then": { "$ref": "./1.8.0/_index.json" } },
                        { "if": { "properties": { "format_version": { "enum": ["1.20.0", "1.21.0"] } } }, "then": { "$ref": "./1.21.0/_index.json" } }
                    ]
                })
                .to_string(),
            ),
            (
                "schema/bedrock/server/entity/1.8.0/_index.json",
                json!({
                    "properties": { "minecraft:entity": { "properties": {
                        "components": { "properties": { "minecraft:health": { "type": "object", "title": "Old Health" } } },
                        "events": entity_events
                    } } }
                })
                .to_string(),
            ),
            (
                "schema/bedrock/server/entity/1.21.0/_index.json",
                json!({
                    "properties": { "format_version": { "const": "1.21.0" }, "minecraft:entity": { "properties": {
                        "components": { "allOf": [{ "$ref": "./components/_index.json" }] },
                        "events": { "$ref": "../1.8.0/_index.json#/properties/minecraft:entity/properties/events" }
                    } } }
                })
                .to_string(),
            ),
            (
                "schema/bedrock/server/entity/1.21.0/components/_index.json",
                json!({
                    "properties": {
                        "minecraft:health": { "$ref": "../../legacy/components/health.json" },
                        "minecraft:scale": { "type": "object", "properties": { "value": { "$ref": "../../legacy/general.json#/$defs/range" } } }
                    }
                })
                .to_string(),
            ),
            (
                "schema/bedrock/server/entity/legacy/components/health.json",
                json!({
                    "title": "Health",
                    "properties": { "value": { "$ref": "#/$defs/int" }, "max": { "$ref": "../general.json#/$defs/range" } },
                    "$defs": { "int": { "type": "integer" } }
                })
                .to_string(),
            ),
            (
                "schema/bedrock/server/entity/legacy/general.json",
                json!({ "$defs": { "range": { "type": "number" } } }).to_string(),
            ),
            (
                "schema/bedrock/shared/manifest/_index.json",
                json!({
                    "title": "Minecraft Bedrock Pack Manifest",
                    "description": "The pack's manifest.",
                    "allOf": [
                        { "properties": { "format_version": { "enum": [1, 2] } } },
                        { "if": { "properties": { "format_version": { "enum": [2] } } }, "then": { "$ref": "./2/_index.json" } },
                        { "if": { "properties": { "format_version": { "enum": [1] } } }, "then": { "$ref": "./1/_index.json" } }
                    ]
                })
                .to_string(),
            ),
            (
                "schema/bedrock/shared/manifest/1/_index.json",
                json!({ "properties": { "header": { "type": "object" } } }).to_string(),
            ),
            (
                "schema/bedrock/shared/manifest/2/_index.json",
                json!({ "properties": { "header": { "type": "object" }, "modules": { "type": "array" } } }).to_string(),
            ),
            (
                "schema/bedrock/client/attachable/_index.json",
                concat!(
                    "{\n",
                    "  // Attachables have one shape here.\n",
                    "  \"title\": \"Minecraft Bedrock Client Attachable\",\n",
                    "  \"properties\": { \"minecraft:attachable\": { \"properties\": { \"description\": {\n",
                    "    \"properties\": { \"identifier\": { \"type\": \"string\", \"pattern\": \"^[a-z]+:// not a comment\" } }\n",
                    "  } } } }\n",
                    "}\n"
                )
                .to_owned(),
            ),
            (
                "schema/bedrock/client/ui/_index.json",
                json!({ "$ref": "./ui_defs.json" }).to_string(),
            ),
            (
                "schema/bedrock/client/ui/ui_defs.json",
                json!({ "type": "object" }).to_string(),
            ),
            (
                "schema/bedrock/client/texture_set/_index.json",
                json!({ "title": "Minecraft Bedrock Texture Set" }).to_string(),
            ),
        ];
        files
            .into_iter()
            .map(|(path, text)| (path.to_owned(), text))
            .collect()
    }

    /// The release extracted into a folder, and its catalog.
    fn fixture() -> (TestDir, Catalog) {
        let dir = TestDir::new("schemas");
        for (path, text) in release() {
            let path = dir.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        let catalog = Catalog::load(dir.path()).unwrap();
        (dir, catalog)
    }

    /// The release as GitHub's source archive: everything under `schemas-<version>/`.
    fn write_archive(path: &Path, files: &[(String, String)]) {
        let mut writer = zip::ZipWriter::new(fs::File::create(path).unwrap());
        writer
            .add_directory("schemas-0.1.4/", zip::write::SimpleFileOptions::default())
            .unwrap();
        for (name, text) in files {
            writer
                .start_file(
                    format!("schemas-0.1.4/{name}"),
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            writer.write_all(text.as_bytes()).unwrap();
        }
        writer
            .start_file(
                "schemas-0.1.4/README.md",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        writer.finish().unwrap();
    }

    #[test]
    fn entries_and_document_ids_come_from_the_compile_script() {
        let entries = parse_entries(COMPILE_TS);
        assert_eq!(entries.len(), 5);
        assert_eq!(
            entries[3],
            Entry {
                pack: "resource",
                globs: vec![
                    "ui/**/*.{json,jsonc,json5}".to_owned(),
                    "!ui/_ui_defs.{json,jsonc,json5}".to_owned()
                ],
                url: "./schema/bedrock/client/ui/_index.json".to_owned(),
            }
        );
        let ids: Vec<String> = entries
            .iter()
            .map(|entry| document_id(entry.pack, &entry.globs))
            .collect();
        assert_eq!(
            ids,
            [
                "bp/manifest",
                "bp/entities",
                "rp/attachables",
                "rp/ui",
                "rp/textures/texture_set"
            ]
        );
        assert_eq!(
            document_id(
                "resource",
                &["textures/terrain_texture.{json,jsonc,json5}".to_owned()]
            ),
            "rp/textures/terrain_texture"
        );
        assert!(parse_entries("const OTHER = [];\n").is_empty());
    }

    #[test]
    fn refs_resolve_inside_the_schema_tree() {
        let from = "bedrock/server/entity/1.21.0/components/_index";
        assert_eq!(
            resolve_ref(from, "../../legacy/components/health.json"),
            Some((
                "bedrock/server/entity/legacy/components/health".to_owned(),
                String::new()
            ))
        );
        assert_eq!(
            resolve_ref(from, "#/$defs/x"),
            Some((from.to_owned(), "/$defs/x".to_owned()))
        );
        assert_eq!(resolve_ref(from, "../../../../../../etc/passwd.json"), None);
        assert_eq!(resolve_ref(from, "./notes.txt"), None);
        assert!(parse_id("../etc/passwd").is_err());
        assert_eq!(
            parse_id("bedrock/a/b.json#/$defs/x").unwrap(),
            ("bedrock/a/b".to_owned(), "/$defs/x".to_owned())
        );
        assert_eq!(
            strip_comments("{\"a\": \"//x\", // y\n\"b\": 1 /* z */}"),
            "{\"a\": \"//x\", \n\"b\": 1 }"
        );
    }

    #[test]
    fn the_catalog_lists_documents_and_matching_components() {
        let (_dir, catalog) = fixture();
        let all = catalog.list(None);
        let documents = all["documents"].as_array().unwrap();
        let ids: Vec<&str> = documents
            .iter()
            .map(|d| d["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            [
                "bp/manifest",
                "bp/entities",
                "rp/attachables",
                "rp/ui",
                "rp/textures/texture_set"
            ]
        );
        assert_eq!(
            documents[1],
            json!({
                "id": "bp/entities",
                "title": "Minecraft Bedrock Entity File",
                "pack": "behavior",
                "file_match": ["entities/**/*.{json,jsonc,json5}"],
                "format_versions": ["1.8.0", "1.21.0"],
                "component_count": 2,
                "description": "",
            })
        );
        assert_eq!(documents[0]["format_versions"], json!(["1", "2"]));
        assert_eq!(documents[2]["component_count"], 0);
        assert_eq!(documents[2]["format_versions"], json!([]));
        assert!(all.get("components").is_none());

        let found = catalog.list(Some(" Health "));
        assert_eq!(found["documents"], json!([]));
        assert_eq!(
            found["components"],
            json!([{ "kind": "bp/entities", "component": "minecraft:health" }])
        );
        let found = catalog.list(Some("manifest"));
        assert_eq!(found["documents"].as_array().unwrap().len(), 1);
        assert_eq!(found["components"], json!([]));
    }

    #[test]
    fn schemas_are_found_by_id_kind_and_component() {
        let (_dir, catalog) = fixture();
        let entities = catalog.get("bp/entities", None).unwrap();
        assert_eq!(entities["id"], "bedrock/server/entity/1.21.0/_index");
        assert_eq!(entities["format_version"], "1.21.0");
        assert_eq!(
            entities["refs"],
            json!([
                "bedrock/server/entity/1.21.0/components/_index",
                "bedrock/server/entity/1.8.0/_index#/properties/minecraft:entity/properties/events"
            ])
        );
        let events = catalog
            .get(entities["refs"][1].as_str().unwrap(), None)
            .unwrap();
        assert_eq!(events["schema"]["title"], "Events");
        assert!(catalog.get("../etc/passwd", None).is_err());
        let error = catalog
            .get("bedrock/missing", None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("schema_list"), "{error}");
        assert!(
            catalog
                .get("bedrock/server/entity/legacy/general#/nope", None)
                .is_err()
        );
        assert!(
            catalog
                .get("bedrock/server/entity/legacy/general", Some("1.21.0"))
                .is_err()
        );

        let health = catalog.component("entities", "health", None).unwrap();
        assert_eq!(
            health["id"],
            "bedrock/server/entity/legacy/components/health"
        );
        assert_eq!(health["schema"]["title"], "Health");
        assert_eq!(health["component"], "minecraft:health");
        assert_eq!(health["kind"], "bp/entities");
        assert_eq!(
            health["refs"],
            json!(["bedrock/server/entity/legacy/general#/$defs/range"])
        );
        let range = catalog
            .get(health["refs"][0].as_str().unwrap(), None)
            .unwrap();
        assert_eq!(range["schema"], json!({ "type": "number" }));

        let scale = catalog
            .component("Minecraft Bedrock Entity File", "minecraft:scale", None)
            .unwrap();
        assert_eq!(
            scale["id"],
            "bedrock/server/entity/1.21.0/components/_index#/properties/minecraft:scale"
        );
        assert_eq!(scale["schema"]["type"], "object");
        assert_eq!(
            scale["refs"],
            json!(["bedrock/server/entity/legacy/general#/$defs/range"])
        );
        let error = catalog
            .component("entities", "nope", None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("No component minecraft:nope"), "{error}");

        let attachable = catalog.kind("attachables", None).unwrap();
        assert_eq!(attachable["id"], "bedrock/client/attachable/_index");
        assert_eq!(attachable["format_version"], Value::Null);
        assert_eq!(
            attachable["schema"]["properties"]["minecraft:attachable"]["properties"]["description"]
                ["properties"]["identifier"]["pattern"],
            "^[a-z]+:// not a comment"
        );
        assert_eq!(
            catalog.kind("rp/ui", None).unwrap()["refs"],
            json!(["bedrock/client/ui/ui_defs"])
        );
        assert_eq!(catalog.document("server/entity").unwrap().id, "bp/entities");
        assert_eq!(catalog.document("attachable").unwrap().id, "rp/attachables");
        assert!(catalog.document("worlds").is_err());
    }

    #[test]
    fn format_version_picks_a_variant() {
        let (_dir, catalog) = fixture();
        let old = catalog
            .component("bp/entities", "health", Some("1.8.0"))
            .unwrap();
        assert_eq!(
            old["id"],
            "bedrock/server/entity/1.8.0/_index#/properties/minecraft:entity/properties/components/properties/minecraft:health"
        );
        assert_eq!(old["schema"]["title"], "Old Health");
        assert_eq!(old["format_version"], "1.8.0");
        let routed = catalog.kind("entities", Some("1.20.0")).unwrap();
        assert_eq!(routed["format_version"], "1.21.0");
        let manifest = catalog.get("bp/manifest", None).unwrap();
        assert_eq!(manifest["id"], "bedrock/shared/manifest/2/_index");
        assert_eq!(
            catalog.get("bp/manifest", Some("1")).unwrap()["format_version"],
            "1"
        );
        let error = catalog
            .kind("entities", Some("1.99.0"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("1.8.0, 1.21.0"), "{error}");
        let error = catalog
            .kind("attachables", Some("1.10.0"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("leave format_version out"), "{error}");
    }

    #[test]
    fn the_release_is_downloaded_once_and_replaced_when_incomplete() {
        let home = TestDir::new("rockide-cache");
        let fetches = Cell::new(0);
        let fetch = |archive: &Path| {
            fetches.set(fetches.get() + 1);
            write_archive(archive, &release());
            Ok(())
        };
        let (dir, note) = ensure_in(home.path(), "0.1.4", fetch).unwrap();
        assert_eq!((dir.clone(), note), (home.path().join("0.1.4"), None));
        assert!(dir.join("scripts/compile.ts").is_file());
        assert!(!dir.join("README.md").exists());
        assert_eq!(Catalog::load(&dir).unwrap().documents.len(), 5);
        let staged: Vec<_> = fs::read_dir(home.path()).unwrap().flatten().collect();
        assert_eq!(staged.len(), 1, "no staging folder is left behind");

        ensure_in(home.path(), "0.1.4", fetch).unwrap();
        assert_eq!(fetches.get(), 1, "a complete cache is reused");

        fs::remove_file(dir.join("scripts/compile.ts")).unwrap();
        let (_, note) = ensure_in(home.path(), "0.1.4", fetch).unwrap();
        assert_eq!(fetches.get(), 2);
        assert!(note.unwrap().contains("incomplete"));
        assert!(is_complete(&dir));
    }

    #[test]
    fn a_failed_or_partial_download_is_not_cached() {
        let home = TestDir::new("rockide-partial");
        let error = ensure_in(home.path(), "0.1.4", |_| bail!("curl failed")).unwrap_err();
        assert_eq!(error.to_string(), "curl failed");
        assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);

        let error = ensure_in(home.path(), "0.1.4", |archive| {
            fs::write(archive, b"PK\x03\x04 truncated")?;
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("source.zip"), "{error}");
        assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);

        let error = ensure_in(home.path(), "0.1.4", |archive| {
            let files: Vec<_> = release()
                .into_iter()
                .filter(|(path, _)| path != ENTRIES)
                .collect();
            write_archive(archive, &files);
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains(ENTRIES), "{error}");
        assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);
    }
}
