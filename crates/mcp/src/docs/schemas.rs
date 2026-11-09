//! Bedrock JSON schemas from npm `@minecraft/bedrock-schemas`, fetched per version into the
//! cache. The version follows the installed game when it can be read.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};

use serde::Deserialize;
use serde_json::{Value, json};

use super::{cache_dir, ensure_package, package_dir, package_versions};
use crate::{
    error::{Error, Result, bail},
    install::Install,
};

pub const PACKAGE: &str = "@minecraft/bedrock-schemas";
/// The game's package manifest; its `<Identity Version>` is the game build.
const GAME_CONFIGS: [&str; 2] = ["MicrosoftGame.Config", "MicrosoftGame.config"];
const SCHEMA_SUFFIX: &str = ".schema.json";
const MAX_COMPONENTS: usize = 200;

/// A game version `major.minor.patch`.
pub type Version = [u32; 3];

/// The game version in a MicrosoftGame.Config: `Version="1.26.6201.0"` is 1.26.62 build 1.
pub fn game_version_from_config(text: &str) -> Option<(Version, String)> {
    let identity = &text[text.find("<Identity")?..];
    let identity = &identity[..identity.find('>')?];
    let start = identity.find("Version=\"")? + "Version=\"".len();
    let value = &identity[start..];
    let value = &value[..value.find('"')?];
    let parts: Vec<u32> = value
        .split('.')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    let [major, minor, packed, ..] = parts[..] else {
        return None;
    };
    let (patch, build) = (packed / 100, packed % 100);
    Some((
        [major, minor, patch],
        format!("{major}.{minor}.{patch}.{build}"),
    ))
}

/// The installed game's version: from the running game's folder, else from the
/// installation's `content` folder (mcbe-gdk-linux keeps the game there).
pub fn game_version(install: &Install) -> Option<(Version, String)> {
    let running = install
        .game_pid()
        .and_then(|pid| install.game_exe(pid))
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    running
        .into_iter()
        .chain([install.profile.join("content")])
        .flat_map(|dir| GAME_CONFIGS.map(|name| dir.join(name)))
        .find_map(|path| game_version_from_config(&fs::read_to_string(path).ok()?))
}

/// A published package version: `1.26.60` or `1.26.60-beta.28`.
fn parse_package_version(text: &str) -> Option<(Version, Option<u32>)> {
    let (release, beta) = match text.split_once("-beta.") {
        Some((release, beta)) => (release, Some(beta.parse().ok()?)),
        None if text.contains('-') => return None,
        None => (text, None),
    };
    let parts: Vec<u32> = release
        .split('.')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    let [major, minor, patch] = parts[..] else {
        return None;
    };
    Some(([major, minor, patch], beta))
}

/// The package version for `game`, and a note when it does not match exactly. Releases
/// match releases; a game with no release schemas (a preview) takes that version's newest
/// beta; otherwise the newest release older than the game; unknown or older games take
/// the newest release.
pub fn pick_version(
    versions: &[String],
    game: Option<Version>,
) -> Option<(String, Option<String>)> {
    let parsed: Vec<(Version, Option<u32>, &String)> = versions
        .iter()
        .filter_map(|text| parse_package_version(text).map(|(version, beta)| (version, beta, text)))
        .collect();
    let newest_release = parsed
        .iter()
        .filter(|(_, beta, _)| beta.is_none())
        .max_by_key(|(version, _, _)| *version)
        .or_else(|| {
            parsed
                .iter()
                .max_by_key(|(version, beta, _)| (*version, *beta))
        });
    let Some(game) = game else {
        return newest_release.map(|(_, _, text)| ((*text).clone(), None));
    };
    let shown = |version: Version| format!("{}.{}.{}", version[0], version[1], version[2]);
    if let Some((_, _, text)) = parsed
        .iter()
        .find(|(version, beta, _)| *version == game && beta.is_none())
    {
        return Some(((*text).clone(), None));
    }
    if let Some((_, _, text)) = parsed
        .iter()
        .filter(|(version, beta, _)| *version == game && beta.is_some())
        .max_by_key(|(_, beta, _)| *beta)
    {
        return Some((
            (*text).clone(),
            Some(format!(
                "No release schemas exist for game {}; using beta schemas {text}.",
                shown(game)
            )),
        ));
    }
    let older = parsed
        .iter()
        .filter(|(version, beta, _)| *version < game && beta.is_none())
        .max_by_key(|(version, _, _)| *version);
    let (_, _, text) = older.or(newest_release)?;
    Some((
        (*text).clone(),
        Some(format!(
            "No schemas exist for game {}; using {text}, so components added or changed since may be missing or differ.",
            shown(game)
        )),
    ))
}

/// The schema versions already in the cache.
fn cached_versions(cache: &Path) -> Vec<String> {
    let prefix = format!("{}@", PACKAGE.replace('/', "+"));
    fs::read_dir(cache.join("packages"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            entry
                .file_name()
                .to_str()?
                .strip_prefix(&prefix)
                .map(str::to_owned)
        })
        .filter(|version| parse_package_version(version).is_some())
        .filter(|version| {
            package_dir(cache, PACKAGE, version)
                .join("package.json")
                .is_file()
        })
        .collect()
}

/// The schemas for this session and what was chosen.
pub struct Resolved {
    pub catalog: Catalog,
    pub version: String,
    pub game: Option<String>,
    pub notes: Vec<String>,
}

impl Resolved {
    fn labelled(&self, fields: Value) -> Value {
        let mut result = json!({
            "source": "schemas",
            "package_version": self.version,
            "game_version": self.game,
            "notes": self.notes,
        });
        if let (Some(result), Value::Object(fields)) = (result.as_object_mut(), fields) {
            result.extend(fields);
        }
        result
    }
}

/// The package version picked for a game version, and the note explaining a mismatch.
type Choice = (Option<Version>, String, Option<String>);

/// The schema package for the installed game, fetched on first use. The choice is kept
/// for the rest of the process unless the game version changes.
pub fn resolve(install: &Install) -> Result<Resolved> {
    static CHOSEN: LazyLock<Mutex<Option<Choice>>> = LazyLock::new(Mutex::default);
    let game = game_version(install);
    let game_version = game.as_ref().map(|(version, _)| *version);
    let mut notes = Vec::new();
    let mut chosen = CHOSEN.lock().unwrap_or_else(|poison| poison.into_inner());
    let (version, note) = match chosen.as_ref() {
        Some((seen, version, note)) if *seen == game_version => (version.clone(), note.clone()),
        _ => {
            let cache = cache_dir()?;
            let cached = cached_versions(&cache);
            let exact = game_version.and_then(|game| {
                pick_version(&cached, Some(game)).filter(|(_, note)| note.is_none())
            });
            let picked = match exact {
                Some(picked) => picked,
                None => match package_versions(PACKAGE) {
                    Ok(published) => pick_version(&published, game_version).ok_or_else(|| {
                        Error::msg(format!("npm lists no versions of {PACKAGE}."))
                    })?,
                    Err(error) => {
                        let picked = pick_version(&cached, game_version).ok_or_else(|| {
                            Error::msg(format!(
                                "{error} No schemas are cached yet, so none can be read."
                            ))
                        })?;
                        notes.push(format!("{error} Using cached schemas."));
                        picked
                    }
                },
            };
            *chosen = Some((game_version, picked.0.clone(), picked.1.clone()));
            picked
        }
    };
    drop(chosen);
    if game.is_none() {
        notes.push("The installed game version is unknown; using the newest schemas.".to_owned());
    }
    notes.extend(note);
    let root = ensure_package(PACKAGE, &version)?;
    Ok(Resolved {
        catalog: Catalog::load(&root)?,
        version,
        game: game.map(|(_, full)| full),
        notes,
    })
}

/// One document type in `schemas/catalog.json`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Document {
    folder: String,
    schema_file: String,
    #[serde(default)]
    pack_type: String,
    #[serde(default)]
    pack_folder: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    component_count: u64,
}

impl Document {
    /// `bp/entities` for an index schema, else `rp/textures/terrain_texture`.
    fn id(&self) -> String {
        match self.schema_file.strip_suffix(SCHEMA_SUFFIX) {
            Some("index") => self.folder.clone(),
            Some(stem) => format!("{}/{stem}", self.folder),
            None => format!("{}/{}", self.folder, self.schema_file),
        }
    }
}

#[derive(Deserialize)]
struct DocumentList {
    documents: Vec<Document>,
}

#[derive(Deserialize)]
struct FileMatches {
    #[serde(default)]
    schemas: Vec<FileMatch>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileMatch {
    url: String,
    #[serde(default)]
    file_match: Vec<String>,
}

/// The schemas of one package version: `root/schemas/catalog.json` lists the document
/// types, `root/catalog.json` the file globs each applies to.
pub struct Catalog {
    schemas: PathBuf,
    documents: Vec<(Document, Vec<String>)>,
}

impl Catalog {
    pub fn load(root: &Path) -> Result<Self> {
        let schemas = root.join("schemas");
        let read = |path: PathBuf| -> Result<Vec<u8>> {
            fs::read(&path)
                .map_err(|error| Error::msg(format!("Cannot read {}: {error}", path.display())))
        };
        let list: DocumentList = serde_json::from_slice(&read(schemas.join("catalog.json"))?)?;
        let matches: FileMatches = fs::read(root.join("catalog.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or(FileMatches {
                schemas: Vec::new(),
            });
        let documents = list
            .documents
            .into_iter()
            .map(|document| {
                let url = format!("./schemas/{}/{}", document.folder, document.schema_file);
                let globs = matches
                    .schemas
                    .iter()
                    .find(|entry| entry.url == url)
                    .map(|entry| entry.file_match.clone())
                    .unwrap_or_default();
                (document, globs)
            })
            .collect();
        Ok(Self { schemas, documents })
    }

    /// Document types whose id, title or description contains `query`, and the components
    /// whose name contains it.
    pub fn list(&self, query: Option<&str>) -> Value {
        let query = query
            .map(str::to_lowercase)
            .filter(|query| !query.is_empty());
        let documents: Vec<Value> = self
            .documents
            .iter()
            .filter(|(document, _)| {
                query.as_ref().is_none_or(|query| {
                    [&document.id(), &document.title, &document.description]
                        .iter()
                        .any(|text| text.to_lowercase().contains(query.as_str()))
                })
            })
            .map(|(document, globs)| {
                json!({
                    "id": document.id(),
                    "title": document.title,
                    "pack": document.pack_type,
                    "file_match": globs,
                    "component_count": document.component_count,
                    "description": document.description,
                })
            })
            .collect();
        let mut result = json!({ "documents": documents });
        if let Some(query) = query {
            let mut components = Vec::new();
            let mut folders = BTreeSet::new();
            for (document, _) in &self.documents {
                if !folders.insert(document.folder.clone()) {
                    continue;
                }
                for name in self.component_names(&document.folder) {
                    if name.contains(query.as_str()) {
                        components.push(json!({ "kind": document.id(), "component": name }));
                    }
                }
            }
            if components.len() > MAX_COMPONENTS {
                components.truncate(MAX_COMPONENTS);
                result["truncated"] = true.into();
            }
            result["components"] = components.into();
        }
        result
    }

    /// The document type `kind` names: its id, its folder, its pack folder (`entities`,
    /// `particles`) or its title.
    fn document(&self, kind: &str) -> Result<&Document> {
        let lower = kind.trim().to_lowercase();
        let exact: Vec<&Document> = self
            .documents
            .iter()
            .map(|(document, _)| document)
            .filter(|document| document.id() == lower || document.title.to_lowercase() == lower)
            .collect();
        let found = if exact.is_empty() {
            self.documents
                .iter()
                .map(|(document, _)| document)
                .filter(|document| {
                    document.folder == lower
                        || (!document.pack_folder.is_empty() && document.pack_folder == lower)
                })
                .collect()
        } else {
            exact
        };
        match found[..] {
            [one] => Ok(one),
            [] => bail!("No schema kind {kind:?}; schema_list shows the kinds."),
            _ => {
                let ids: Vec<String> = found.iter().map(|document| document.id()).collect();
                bail!(
                    "Schema kind {kind:?} is ambiguous; use one of: {}",
                    ids.join(", ")
                )
            }
        }
    }

    /// The schema files under `schemas/folder`, sorted.
    fn files(&self, folder: &str) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = fs::read_dir(self.schemas.join(folder))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.to_string_lossy().ends_with(SCHEMA_SUFFIX))
            .collect();
        files.sort();
        files
    }

    /// Every namespaced property key (`minecraft:health`) in the folder's schemas.
    fn component_names(&self, folder: &str) -> BTreeSet<String> {
        fn collect(node: &Value, names: &mut BTreeSet<String>) {
            match node {
                Value::Object(map) => {
                    if let Some(Value::Object(properties)) = map.get("properties") {
                        names.extend(properties.keys().filter(|key| key.contains(':')).cloned());
                    }
                    map.values().for_each(|value| collect(value, names));
                }
                Value::Array(items) => items.iter().for_each(|value| collect(value, names)),
                _ => {}
            }
        }
        let mut names = BTreeSet::new();
        for file in self.files(folder) {
            if let Some(schema) = read_json(&file) {
                collect(&schema, &mut names);
            }
        }
        names
    }

    /// The id of a schema file: its path under `schemas/` without `.schema.json`.
    fn file_id(&self, path: &Path) -> Option<String> {
        let relative = path.strip_prefix(&self.schemas).ok()?;
        let parts: Vec<_> = relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect();
        Some(parts.join("/").strip_suffix(SCHEMA_SUFFIX)?.to_owned())
    }

    /// The schema file of an id: a document id or a schema file id.
    fn path_of(&self, id: &str) -> Result<PathBuf> {
        let id = id
            .trim()
            .trim_start_matches("./")
            .trim_end_matches(SCHEMA_SUFFIX);
        if id.is_empty()
            || id
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
            || id.contains('\\')
        {
            bail!("{id:?} is not a schema id; schema_list shows the ids.");
        }
        if let Some((document, _)) = self
            .documents
            .iter()
            .find(|(document, _)| document.id() == id)
        {
            return Ok(self
                .schemas
                .join(&document.folder)
                .join(&document.schema_file));
        }
        let path = self.schemas.join(format!("{id}{SCHEMA_SUFFIX}"));
        if path.is_file() {
            Ok(path)
        } else {
            bail!(
                "No schema {id:?}; schema_list shows the document ids, and each schema lists the ids it refers to in refs."
            )
        }
    }

    /// A schema with the ids of the schema files it refers to.
    fn schema_at(&self, path: &Path) -> Result<Value> {
        let schema = read_json(path)
            .ok_or_else(|| Error::msg(format!("{} is not valid JSON.", path.display())))?;
        let dir = path.parent().unwrap_or(&self.schemas);
        let mut refs = BTreeSet::new();
        collect_refs(&schema, &mut refs);
        let refs: Vec<String> = refs
            .into_iter()
            .filter_map(|reference| {
                let file = reference.split('#').next()?;
                (!file.is_empty()).then(|| self.file_id(&normalize(&dir.join(file))))?
            })
            .collect();
        Ok(json!({ "id": self.file_id(path), "schema": schema, "refs": refs }))
    }

    pub fn get(&self, id: &str) -> Result<Value> {
        self.schema_at(&self.path_of(id)?)
    }

    /// The schema of component `name` (`minecraft:` is assumed when no namespace is given)
    /// in document type `kind`: the file a component property refers to, else the inline
    /// definition.
    pub fn component(&self, kind: &str, name: &str) -> Result<Value> {
        let document = self.document(kind)?;
        let name = name.trim().to_lowercase();
        let name = if name.contains(':') {
            name
        } else {
            format!("minecraft:{name}")
        };
        let mut inline = None;
        for file in self.files(&document.folder) {
            let Some(schema) = read_json(&file) else {
                continue;
            };
            let Some(found) = find_property(&schema, &name) else {
                continue;
            };
            if let Some(reference) = found["$ref"].as_str()
                && let Some(target) = reference
                    .split('#')
                    .next()
                    .filter(|target| !target.is_empty())
            {
                let target = normalize(&file.parent().unwrap_or(&self.schemas).join(target));
                if target.is_file() {
                    let mut result = self.schema_at(&target)?;
                    result["component"] = name.into();
                    result["kind"] = document.id().into();
                    return Ok(result);
                }
            }
            inline.get_or_insert((file, found.clone()));
        }
        let Some((file, definition)) = inline else {
            bail!(
                "No component {name} in {}; schema_list with a query lists component names.",
                document.id()
            );
        };
        let mut refs = BTreeSet::new();
        collect_refs(&definition, &mut refs);
        let dir = file.parent().unwrap_or(&self.schemas);
        let refs: Vec<String> = refs
            .into_iter()
            .filter_map(|reference| {
                let target = reference.split('#').next()?;
                (!target.is_empty()).then(|| self.file_id(&normalize(&dir.join(target))))?
            })
            .collect();
        Ok(json!({
            "id": self.file_id(&file),
            "component": name,
            "kind": document.id(),
            "schema": definition,
            "refs": refs,
        }))
    }
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// The first `properties[name]` in a schema, searching depth first.
fn find_property<'a>(node: &'a Value, name: &str) -> Option<&'a Value> {
    match node {
        Value::Object(map) => map
            .get("properties")
            .and_then(|properties| properties.get(name))
            .or_else(|| map.values().find_map(|value| find_property(value, name))),
        Value::Array(items) => items.iter().find_map(|value| find_property(value, name)),
        _ => None,
    }
}

fn collect_refs(node: &Value, refs: &mut BTreeSet<String>) {
    match node {
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get("$ref") {
                refs.insert(reference.clone());
            }
            map.values().for_each(|value| collect_refs(value, refs));
        }
        Value::Array(items) => items.iter().for_each(|value| collect_refs(value, refs)),
        _ => {}
    }
}

/// `a/b/../c/./d` as `a/c/d`, without touching the file system.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
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
}

pub fn list(install: &Install, request: &List) -> Result<Value> {
    let resolved = resolve(install)?;
    Ok(resolved.labelled(resolved.catalog.list(request.query.as_deref())))
}

pub fn get(install: &Install, request: &Get) -> Result<Value> {
    let found = |catalog: &Catalog| match (&request.id, &request.kind, &request.component) {
        (Some(id), None, None) => catalog.get(id),
        (None, Some(kind), Some(component)) => catalog.component(kind, component),
        (None, Some(kind), None) => {
            let document = catalog.document(kind)?;
            catalog.get(&document.id())
        }
        _ => bail!("Give id, or kind with an optional component."),
    };
    let resolved = resolve(install)?;
    Ok(resolved.labelled(found(&resolved.catalog)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir::TestDir;

    #[test]
    fn the_game_version_comes_from_the_identity() {
        let config = r#"<Game><Identity Name="Microsoft.MinecraftUWP" Publisher="CN=Microsoft" Version="1.26.6201.0" /></Game>"#;
        assert_eq!(
            game_version_from_config(config),
            Some(([1, 26, 62], "1.26.62.1".to_owned()))
        );
        let config = r#"<Identity Version="1.2.34501.0"/>"#;
        assert_eq!(game_version_from_config(config).unwrap().0, [1, 2, 345]);
        assert_eq!(game_version_from_config("<Game/>"), None);
    }

    #[test]
    fn schema_versions_follow_the_game() {
        let published: Vec<String> = [
            "1.26.30-beta.28",
            "1.26.40-beta.26",
            "1.26.50",
            "1.26.60-beta.24",
            "1.26.60-beta.28",
            "1.26.60",
        ]
        .map(str::to_owned)
        .to_vec();
        assert_eq!(
            pick_version(&published, Some([1, 26, 60])),
            Some(("1.26.60".to_owned(), None))
        );
        assert_eq!(
            pick_version(&published, None),
            Some(("1.26.60".to_owned(), None))
        );
        let (version, note) = pick_version(&published, Some([1, 26, 40])).unwrap();
        assert_eq!(version, "1.26.40-beta.26");
        assert!(note.unwrap().contains("beta"));
        let (version, note) = pick_version(&published, Some([9, 9, 9])).unwrap();
        assert_eq!(version, "1.26.60");
        assert!(note.unwrap().contains("game 9.9.9"));
        let (version, note) = pick_version(&published, Some([1, 20, 0])).unwrap();
        assert_eq!(version, "1.26.60");
        assert!(note.is_some());
        assert_eq!(pick_version(&[], None), None);
    }

    /// A package laid out like @minecraft/bedrock-schemas 1.26.60.
    fn fixture() -> (TestDir, Catalog) {
        let dir = TestDir::new("schemas");
        let root = dir.path();
        let write = |path: &str, value: Value| {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
        };
        write(
            "catalog.json",
            json!({ "schemas": [
            { "name": "Entity Behavior", "fileMatch": ["**/entities/**/*.json"], "url": "./schemas/bp/entities/index.schema.json" }
        ] }),
        );
        write(
            "schemas/catalog.json",
            json!({ "documents": [
            { "folder": "bp/entities", "schemaFile": "index.schema.json", "packType": "behavior", "packFolder": "entities", "title": "Entity Behavior", "description": "An entity file.", "componentCount": 2 },
            { "folder": "bp/blocks", "schemaFile": "index.schema.json", "packType": "behavior", "packFolder": "blocks", "title": "Block Behavior", "description": "A block file.", "componentCount": 1 },
            { "folder": "rp/textures", "schemaFile": "terrain_texture.schema.json", "packType": "resource", "packFolder": "textures", "title": "Terrain Texture", "componentCount": 0 },
            { "folder": "rp/textures", "schemaFile": "item_texture.schema.json", "packType": "resource", "packFolder": "textures", "title": "Item Texture", "componentCount": 0 }
        ] }),
        );
        write(
            "schemas/bp/entities/index.schema.json",
            json!({
                "properties": { "minecraft:entity": { "$ref": "./entity_actor_document.schema.json" } }
            }),
        );
        write(
            "schemas/bp/entities/entity_actor_document.schema.json",
            json!({
                "properties": { "components": { "$ref": "./entity_components.schema.json" } }
            }),
        );
        write(
            "schemas/bp/entities/entity_components.schema.json",
            json!({
                "properties": {
                    "minecraft:health": { "$ref": "./entity_minecraft_health.schema.json" },
                    "minecraft:scale": { "type": "object", "properties": { "value": { "$ref": "../../common/floatrange.schema.json#/x" } } }
                }
            }),
        );
        write(
            "schemas/bp/entities/entity_minecraft_health.schema.json",
            json!({
                "title": "Health", "properties": { "value": { "type": "integer" } }
            }),
        );
        write(
            "schemas/bp/blocks/index.schema.json",
            json!({
                "properties": { "minecraft:block": { "properties": { "components": { "properties": {
                    "minecraft:light_emission": { "type": "integer" }
                } } } } }
            }),
        );
        write(
            "schemas/common/floatrange.schema.json",
            json!({ "type": "number" }),
        );
        write(
            "schemas/rp/textures/terrain_texture.schema.json",
            json!({ "type": "object" }),
        );
        write(
            "schemas/rp/textures/item_texture.schema.json",
            json!({ "type": "object" }),
        );
        let catalog = Catalog::load(root).unwrap();
        (dir, catalog)
    }

    #[test]
    fn the_catalog_lists_documents_and_matching_components() {
        let (_dir, catalog) = fixture();
        let all = catalog.list(None);
        let ids: Vec<&str> = all["documents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            [
                "bp/entities",
                "bp/blocks",
                "rp/textures/terrain_texture",
                "rp/textures/item_texture"
            ]
        );
        assert_eq!(
            all["documents"][0]["file_match"],
            json!(["**/entities/**/*.json"])
        );
        assert!(all.get("components").is_none());

        let found = catalog.list(Some("Health"));
        assert_eq!(found["documents"], json!([]));
        assert_eq!(
            found["components"],
            json!([{ "kind": "bp/entities", "component": "minecraft:health" }])
        );
        let found = catalog.list(Some("texture"));
        assert_eq!(found["documents"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn schemas_are_found_by_id_kind_and_component() {
        let (_dir, catalog) = fixture();
        let index = catalog.get("bp/entities").unwrap();
        assert_eq!(index["id"], "bp/entities/index");
        assert_eq!(index["refs"], json!(["bp/entities/entity_actor_document"]));
        assert_eq!(
            catalog.get("bp/entities/entity_actor_document").unwrap()["refs"],
            json!(["bp/entities/entity_components"])
        );
        assert!(catalog.get("../etc/passwd").is_err());
        assert!(
            catalog
                .get("bp/entities/missing")
                .unwrap_err()
                .to_string()
                .contains("schema_list")
        );

        let health = catalog.component("entities", "health").unwrap();
        assert_eq!(health["id"], "bp/entities/entity_minecraft_health");
        assert_eq!(health["schema"]["title"], "Health");
        assert_eq!(health["component"], "minecraft:health");
        let scale = catalog
            .component("Entity Behavior", "minecraft:scale")
            .unwrap();
        assert_eq!(scale["id"], "bp/entities/entity_components");
        assert_eq!(scale["schema"]["type"], "object");
        assert_eq!(scale["refs"], json!(["common/floatrange"]));
        let light = catalog.component("bp/blocks", "light_emission").unwrap();
        assert_eq!(light["schema"], json!({ "type": "integer" }));

        assert!(
            catalog
                .component("entities", "nope")
                .unwrap_err()
                .to_string()
                .contains("No component minecraft:nope")
        );
        let error = catalog.document("textures").unwrap_err().to_string();
        assert!(
            error.contains("ambiguous") && error.contains("rp/textures/item_texture"),
            "{error}"
        );
        assert_eq!(
            catalog.document("rp/textures/item_texture").unwrap().title,
            "Item Texture"
        );
        assert!(catalog.document("worlds").is_err());
    }
}
