//! `pack_check`: static reference checks over an add-on's behavior and resource packs. Every
//! texture, function, loot table, item, block, entity, model, animation, sound or pack a file
//! names must exist in the add-on or in vanilla; each one that does not becomes a diagnostic
//! with its file, line and column.

mod commands;
mod rules;
#[cfg(test)]
mod tests;
mod tree;
mod vanilla;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    ffi::OsString,
    fmt,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    jsonpos::{self, Node, Pos},
    schemas,
};
use crate::error::{Result, bail};
use tree::Tree;
use vanilla::Vanilla;

/// The `source` of every diagnostic this module reports.
const SOURCE: &str = "references";
/// Diagnostics a reply lists at most; counts still cover all of them.
const MAX_DIAGNOSTICS: usize = 1000;

/// What a reference names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    Item,
    Block,
    Entity,
    LootTable,
    TradeTable,
    Function,
    BehaviorAnimation,
    TerrainTexture,
    ItemTexture,
    /// An image file by its path without extension.
    Texture,
    Geometry,
    RenderController,
    Animation,
    /// A sound definition (`mob.zombie.say`).
    Sound,
    /// A sound file by its path without extension.
    SoundFile,
    Particle,
}

impl Kind {
    pub fn rule(self) -> &'static str {
        match self {
            Self::Item => "item-missing",
            Self::Block => "block-missing",
            Self::Entity => "entity-missing",
            Self::LootTable => "loot-table-missing",
            Self::TradeTable => "trade-table-missing",
            Self::Function => "function-missing",
            Self::BehaviorAnimation | Self::Animation => "animation-missing",
            Self::TerrainTexture | Self::ItemTexture | Self::Texture => "texture-missing",
            Self::Geometry => "geometry-missing",
            Self::RenderController => "render-controller-missing",
            Self::Sound | Self::SoundFile => "sound-missing",
            Self::Particle => "particle-missing",
        }
    }

    fn noun(self) -> &'static str {
        match self {
            Self::Item => "item",
            Self::Block => "block",
            Self::Entity => "entity",
            Self::LootTable => "loot table",
            Self::TradeTable => "trade table",
            Self::Function => "function",
            Self::BehaviorAnimation | Self::Animation => "animation",
            Self::TerrainTexture => "terrain texture",
            Self::ItemTexture => "item texture",
            Self::Texture => "texture file",
            Self::Geometry => "geometry",
            Self::RenderController => "render controller",
            Self::Sound => "sound",
            Self::SoundFile => "sound file",
            Self::Particle => "particle",
        }
    }

    /// Where the add-on defines this kind, for messages.
    fn home(self) -> &'static str {
        match self {
            Self::Item => "BP items/, blocks/",
            Self::Block => "BP blocks/",
            Self::Entity => "BP entities/",
            Self::LootTable => "BP loot_tables/",
            Self::TradeTable => "BP trading/",
            Self::Function => "BP functions/",
            Self::BehaviorAnimation => "BP animations/, animation_controllers/",
            Self::TerrainTexture => "RP textures/terrain_texture.json",
            Self::ItemTexture => "RP textures/item_texture.json",
            Self::Texture => "RP image files (.png, .tga, .jpg)",
            Self::Geometry => "RP models/",
            Self::RenderController => "RP render_controllers/",
            Self::Animation => "RP animations/, animation_controllers/",
            Self::Sound => "RP sounds/sound_definitions.json",
            Self::SoundFile => "RP sound files (.ogg, .wav, .fsb)",
            Self::Particle => "RP particles/",
        }
    }

    /// The pack type that defines this kind.
    fn pack(self) -> PackKind {
        match self {
            Self::Item
            | Self::Block
            | Self::Entity
            | Self::LootTable
            | Self::TradeTable
            | Self::Function
            | Self::BehaviorAnimation => PackKind::Behavior,
            _ => PackKind::Resource,
        }
    }

    /// The name as definitions and references are compared: identifiers get the default
    /// `minecraft:` namespace and lose an item's data value (`minecraft:dye:4`) or an entity's
    /// spawn event (`minecraft:zombie<minecraft:as_baby>`); paths use `/`, lose a leading `/`
    /// and file extensions the game adds itself.
    pub fn normalize(self, name: &str) -> String {
        let name = name.trim();
        match self {
            Self::Item | Self::Block | Self::Entity | Self::Particle => {
                let mut name = name.split('<').next().unwrap_or_default();
                if self == Self::Item
                    && let Some((base, data)) = name.rsplit_once(':')
                    && !data.is_empty()
                    && data.bytes().all(|byte| byte.is_ascii_digit())
                {
                    name = base;
                }
                if name.contains(':') || name.is_empty() {
                    name.to_owned()
                } else {
                    format!("minecraft:{name}")
                }
            }
            Self::LootTable | Self::TradeTable => {
                let path = name.replace('\\', "/");
                let path = path.trim_start_matches('/');
                if path.ends_with(".json") || path.is_empty() {
                    path.to_owned()
                } else {
                    format!("{path}.json")
                }
            }
            Self::Function => name.trim_matches('"').trim_start_matches('/').to_owned(),
            Self::Texture => strip_extension(name, &["png", "tga", "jpg", "jpeg"]),
            Self::SoundFile => strip_extension(name, &["ogg", "wav", "fsb", "mp3"]),
            _ => name.to_owned(),
        }
    }
}

fn strip_extension(path: &str, extensions: &[&str]) -> String {
    let path = path.replace('\\', "/");
    let path = path.trim_start_matches('/');
    match path.rsplit_once('.') {
        Some((stem, extension))
            if extensions
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known)) =>
        {
            stem.to_owned()
        }
        _ => path.to_owned(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PackKind {
    Behavior,
    Resource,
}

impl PackKind {
    fn label(self) -> &'static str {
        match self {
            Self::Behavior => "BP",
            Self::Resource => "RP",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Behavior => "behavior",
            Self::Resource => "resource",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

impl Severity {
    pub fn name(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Info => "info",
        }
    }
}

/// One problem, at a place in a pack file.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Diagnostic {
    /// Relative to the checked folder, starting with the pack's label (`BP/items/gem.json`).
    pub file: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub severity: Severity,
    pub rule: &'static str,
    pub message: String,
}

impl Diagnostic {
    fn to_json(&self) -> Value {
        let mut value = json!({
            "file": self.file,
            "severity": self.severity.name(),
            "rule": self.rule,
            "message": self.message,
            "source": SOURCE,
        });
        if let (Some(line), Some(column)) = (self.line, self.column) {
            value["line"] = json!(line);
            value["column"] = json!(column);
        }
        value
    }
}

/// `BP/file.json:12:5: error rule: message`, the form compilers print.
impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.file)?;
        if let (Some(line), Some(column)) = (self.line, self.column) {
            write!(f, ":{line}:{column}")?;
        }
        write!(
            f,
            ": {} {}: {}",
            self.severity.name(),
            self.rule,
            self.message
        )
    }
}

/// A pack found in the tree.
#[derive(Debug)]
pub struct Pack {
    pub kind: PackKind,
    /// The pack's folder in the tree: empty, or ending with `/`.
    pub folder: String,
    /// What diagnostics put before the pack-relative path: `BP`, or `BP/<folder>` when there
    /// are several behavior packs.
    pub label: String,
    pub name: String,
    uuid: Option<String>,
    version: Option<String>,
}

/// A parsed pack file.
pub struct Doc<'a> {
    pub pack: usize,
    /// Relative to the pack folder.
    pub path: &'a str,
    pub content: Content<'a>,
}

pub enum Content<'a> {
    Json(Node),
    Function(&'a str),
}

/// A name a file refers to, before it is resolved.
#[derive(Debug, PartialEq)]
pub struct Reference {
    pub kind: Kind,
    pub name: String,
    pub pos: Pos,
}

/// The result of one check.
pub struct Report {
    pub packs: Vec<Pack>,
    pub diagnostics: Vec<Diagnostic>,
    pub files_scanned: usize,
    /// The vanilla data release the names were checked against.
    pub vanilla: Option<String>,
    pub notes: Vec<String>,
}

impl Report {
    pub fn count(&self, severity: Severity) -> usize {
        self.diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == severity)
            .count()
    }
}

/// Check every pack in `tree`, resolving names against the add-on and `vanilla`.
pub fn check(tree: &Tree, vanilla: Option<&Vanilla>) -> Report {
    let mut diagnostics = Vec::new();
    let mut notes = Vec::new();
    let packs = tree::packs(tree, &mut diagnostics, &mut notes);
    let mut docs = Vec::new();
    for (index, pack) in packs.iter().enumerate() {
        for (path, text) in tree.texts_under(&pack.folder) {
            let content = if path.ends_with(".mcfunction") {
                Content::Function(text)
            } else {
                match jsonpos::parse(text) {
                    Ok(node) => Content::Json(node),
                    Err(error) => {
                        diagnostics.push(Diagnostic {
                            file: format!("{}/{path}", pack.label),
                            line: Some(error.pos.line),
                            column: Some(error.pos.column),
                            severity: Severity::Error,
                            rule: "json-syntax",
                            message: format!("{}; the game cannot read this file", error.message),
                        });
                        continue;
                    }
                }
            };
            docs.push(Doc {
                pack: index,
                path,
                content,
            });
        }
    }
    let defined = define(tree, &packs, &docs);
    let present: BTreeSet<PackKind> = packs.iter().map(|pack| pack.kind).collect();
    let mut skipped = BTreeSet::new();
    for doc in &docs {
        let pack = &packs[doc.pack];
        let file = format!("{}/{}", pack.label, doc.path);
        for reference in rules::references(pack.kind, doc) {
            if !present.contains(&reference.kind.pack()) {
                skipped.insert(reference.kind.pack());
                continue;
            }
            if let Some(diagnostic) = resolve(&reference, &file, &defined, vanilla) {
                diagnostics.push(diagnostic);
            }
        }
        if let Content::Json(node) = &doc.content
            && doc.path == "manifest.json"
        {
            diagnostics.extend(dependencies(node, &file, &packs));
        }
    }
    for kind in skipped {
        notes.push(format!(
            "No {} pack was found, so references to its files were not checked.",
            kind.name()
        ));
    }
    if vanilla.is_none() {
        notes.push(
            "Vanilla data was unavailable: names that are not in the add-on and may be vanilla are warnings, not errors."
                .into(),
        );
    }
    diagnostics.sort();
    diagnostics.dedup();
    Report {
        packs,
        files_scanned: docs.len(),
        vanilla: vanilla.map(|vanilla| vanilla.version.clone()),
        diagnostics,
        notes,
    }
}

/// Everything the add-on defines, by kind, normalized.
type Defined = HashMap<Kind, HashSet<String>>;

fn define(tree: &Tree, packs: &[Pack], docs: &[Doc]) -> Defined {
    let mut defined = Defined::new();
    for (index, pack) in packs.iter().enumerate() {
        for path in tree.files_under(&pack.folder) {
            if let Some((kind, name)) = rules::file_definition(pack.kind, path) {
                defined
                    .entry(kind)
                    .or_default()
                    .insert(kind.normalize(&name));
            }
        }
        for doc in docs.iter().filter(|doc| doc.pack == index) {
            if let Content::Json(node) = &doc.content {
                for (kind, name) in rules::definitions(pack.kind, doc.path, node) {
                    defined
                        .entry(kind)
                        .or_default()
                        .insert(kind.normalize(&name));
                }
            }
        }
    }
    defined
}

/// The diagnostic for `reference` when it names nothing in the add-on or vanilla. Without
/// vanilla data, a name that may be vanilla is only a warning.
fn resolve(
    reference: &Reference,
    file: &str,
    defined: &Defined,
    vanilla: Option<&Vanilla>,
) -> Option<Diagnostic> {
    let kind = reference.kind;
    let name = kind.normalize(&reference.name);
    if name.is_empty()
        || defined.get(&kind).is_some_and(|set| set.contains(&name))
        || vanilla.is_some_and(|vanilla| vanilla.contains(kind, &name))
    {
        return None;
    }
    let (severity, searched) = match vanilla {
        Some(_) if vanilla_incomplete(kind, &name) => (
            Severity::Warning,
            " and vanilla; vanilla data lacks legacy names, built-ins and sounds the game ships apart, so this may still work",
        ),
        Some(_) => (Severity::Error, " and vanilla"),
        None if may_be_vanilla(kind, &name) => {
            (Severity::Warning, "; vanilla data was unavailable")
        }
        None => (Severity::Error, ""),
    };
    Some(Diagnostic {
        file: file.to_owned(),
        line: Some(reference.pos.line),
        column: Some(reference.pos.column),
        severity,
        rule: kind.rule(),
        message: format!(
            "{} \"{}\" is not defined (looked in {}{searched})",
            kind.noun(),
            reference.name,
            kind.home()
        ),
    })
}

/// Whether the vanilla lists may miss a real vanilla `name`: they hold current item, block
/// and entity identifiers but not the legacy ones the game still accepts (`minecraft:dye`)
/// or built-ins (`minecraft:leash_knot`), and only the sound files the public vanilla
/// pack ships, not music and other sounds the game ships apart.
fn vanilla_incomplete(kind: Kind, name: &str) -> bool {
    match kind {
        Kind::Item | Kind::Block | Kind::Entity => name.starts_with("minecraft:"),
        Kind::SoundFile => true,
        _ => false,
    }
}

/// Whether vanilla could define `name`: never a function, and never an identifier in a
/// namespace other than `minecraft`.
fn may_be_vanilla(kind: Kind, name: &str) -> bool {
    match kind {
        Kind::Function => false,
        Kind::Item | Kind::Block | Kind::Entity | Kind::Particle | Kind::Geometry => {
            name.starts_with("minecraft:") || name.starts_with("geometry.")
        }
        _ => true,
    }
}

/// `[1, 2, 0]` or `"1.2.0"` as `1.2.0`.
fn version_text(node: &Node) -> Option<String> {
    match &node.value {
        jsonpos::Value::String(text) => Some(text.clone()),
        jsonpos::Value::Array(parts) => parts
            .iter()
            .map(|part| match &part.value {
                jsonpos::Value::Number(number) => Some(number.clone()),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()
            .map(|parts| parts.join(".")),
        _ => None,
    }
}

/// `manifest-dependency-missing` for each pack dependency that is not among `packs` (a
/// warning: it may be installed on its own) or is there with another version (an error).
/// Script module dependencies (`module_name`) are the game's and are not checked.
fn dependencies(manifest: &Node, file: &str, packs: &[Pack]) -> Vec<Diagnostic> {
    let mut found = Vec::new();
    for dependency in manifest.get("dependencies").map_or(&[][..], Node::items) {
        let Some(uuid_node) = dependency.get("uuid") else {
            continue;
        };
        let Some(uuid) = uuid_node.as_str() else {
            continue;
        };
        let wanted = dependency.get("version").and_then(version_text);
        let pack = packs.iter().find(|pack| {
            pack.uuid
                .as_deref()
                .is_some_and(|own| own.eq_ignore_ascii_case(uuid))
        });
        let (severity, message) = match (pack, &wanted) {
            (None, _) => (
                Severity::Warning,
                format!(
                    "depends on pack {uuid}{}, which is not among the checked packs",
                    wanted
                        .as_deref()
                        .map(|v| format!(" {v}"))
                        .unwrap_or_default()
                ),
            ),
            (Some(pack), Some(wanted)) if pack.version.as_ref() != Some(wanted) => (
                Severity::Error,
                format!(
                    "depends on \"{}\" {wanted}, but that pack's header version is {}",
                    pack.name,
                    pack.version.as_deref().unwrap_or("missing")
                ),
            ),
            _ => continue,
        };
        found.push(Diagnostic {
            file: file.to_owned(),
            line: Some(uuid_node.pos.line),
            column: Some(uuid_node.pos.column),
            severity,
            rule: "manifest-dependency-missing",
            message,
        });
    }
    found
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub path: PathBuf,
}

/// The vanilla names of the pinned rockide/schemas release, downloaded on first use, or
/// None with the reason in `notes`.
fn load_vanilla(notes: &mut Vec<String>) -> Option<Vanilla> {
    let loaded = schemas::release_dir().and_then(|(dir, note)| {
        notes.extend(note);
        Vanilla::load(&dir, schemas::PINNED.into())
    });
    match loaded {
        Ok(vanilla) => Some(vanilla),
        Err(error) => {
            notes.push(format!("Vanilla identifiers could not be loaded: {error}"));
            None
        }
    }
}

/// Check the packs at `path` against the pinned vanilla data; fails when there are none.
fn check_path(path: &Path) -> Result<Report> {
    let tree = Tree::load(path)?;
    let mut notes = tree.notes.clone();
    let vanilla = load_vanilla(&mut notes);
    let mut report = check(&tree, vanilla.as_ref());
    if report.packs.is_empty() {
        bail!(
            "{} holds no behavior or resource pack (a folder with a manifest.json whose modules are data, script or resources).",
            path.display()
        );
    }
    notes.append(&mut report.notes);
    report.notes = notes;
    Ok(report)
}

/// The `pack_check` tool.
pub fn run(request: &Check) -> Result<Value> {
    let report = check_path(&request.path)?;
    let counts: BTreeMap<&str, usize> = [Severity::Error, Severity::Warning, Severity::Info]
        .into_iter()
        .map(|severity| (severity.name(), report.count(severity)))
        .collect();
    let mut result = json!({
        "source": SOURCE,
        "ok": report.count(Severity::Error) == 0,
        "diagnostics": report.diagnostics.iter().take(MAX_DIAGNOSTICS).map(Diagnostic::to_json).collect::<Vec<_>>(),
        "counts": counts,
        "files_scanned": report.files_scanned,
        "packs": report.packs.iter().map(|pack| json!({
            "label": pack.label,
            "kind": pack.kind.name(),
            "name": pack.name,
            "folder": pack.folder.trim_end_matches('/'),
        })).collect::<Vec<_>>(),
        "vanilla": report.vanilla.as_ref().map(|version| json!({ "source": schemas::REPOSITORY, "version": version })),
        "notes": report.notes,
    });
    if report.diagnostics.len() > MAX_DIAGNOSTICS {
        result["truncated"] = json!(true);
    }
    Ok(result)
}

/// `bedrock-mcp check <path>`: each diagnostic on stdout as `file:line:column: severity
/// rule: message`, notes and a summary on stderr. The exit status is 1 when there are
/// errors and 2 when the check could not run.
pub fn cli(path: Option<OsString>) -> i32 {
    let Some(path) = path else {
        eprintln!(
            "usage: bedrock-mcp check <pack, project folder or .mcpack/.mcaddon/.mcworld/.zip>"
        );
        return 2;
    };
    let report = match check_path(Path::new(&path)) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("bedrock-mcp: {error}");
            return 2;
        }
    };
    for diagnostic in &report.diagnostics {
        println!("{diagnostic}");
    }
    for note in &report.notes {
        eprintln!("note: {note}");
    }
    let errors = report.count(Severity::Error);
    eprintln!(
        "{errors} errors, {} warnings in {} files",
        report.count(Severity::Warning),
        report.files_scanned
    );
    i32::from(errors > 0)
}
