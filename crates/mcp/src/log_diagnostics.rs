//! Content-log entries from the game or a dedicated server as diagnostics that point at a pack
//! file, and at a line when the entry gives one or names a key or identifier the file holds.
//!
//! The message shapes handled here are what the public game and Bedrock Dedicated Server print:
//! `recipes/a.json | demo:a | text` (a file, then an identifier), `JSON: recipes/a.json has an
//! error: * Line 5, Column 5 ...` (a parse error), `Error Parsing Item 'demo:a':` followed by
//! `Failed to parse field ' -> components -> key: text'`, `Function name failed to load ...`
//! followed by `Error on line 2: ...`, and script stacks `at name (main.js:7)`. A line is
//! reported only when the entry states it or the named key occurs once in the file; otherwise
//! it stays null.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    rc::Rc,
};

use serde::Serialize;
use serde_json::{Value, json};

use crate::packs::Kind;

/// Files a pack walk looks at, so a huge folder cannot stall a call.
const MAX_FILES: usize = 20_000;
/// Larger files are not searched for keys.
const MAX_FILE_BYTES: u64 = 4 << 20;
/// Longest message kept per diagnostic.
const MAX_MESSAGE: usize = 400;
const PATH_EXTENSIONS: [&str; 10] = [
    "json",
    "js",
    "mcfunction",
    "lang",
    "png",
    "tga",
    "material",
    "mcstructure",
    "ts",
    "fsb",
];

/// A pack folder diagnostics resolve file paths against.
#[derive(Clone, Debug)]
pub struct PackRoot {
    pub kind: Kind,
    pub path: PathBuf,
    /// The manifest name, which script errors print as `[name]`.
    pub name: String,
}

impl PackRoot {
    fn label(&self) -> &'static str {
        match self.kind {
            Kind::Behavior => "BP",
            Kind::Resource => "RP",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

impl Severity {
    /// A level name or number as the content log, the live layer or a server console gives it.
    pub fn from_level(level: &str) -> Self {
        match level.to_ascii_lowercase().as_str() {
            "error" | "3" | "fatal" => Self::Error,
            "warning" | "warn" | "2" => Self::Warning,
            _ => Self::Info,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Info => "info",
        }
    }
}

/// One problem: `file` is relative to its pack root with a `BP/` or `RP/` prefix, or the path
/// as the entry wrote it when no given pack holds it, or null when the entry names no file.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct Diagnostic {
    pub file: Option<String>,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub severity: Severity,
    pub rule: String,
    pub message: String,
    pub source: &'static str,
    /// Whether `file` is in one of the given packs.
    #[serde(skip)]
    pub in_pack: bool,
}

/// One content-log entry: its area (`Recipes`, `Scripting`, ... or empty), level and text.
pub struct LogEntry<'a> {
    pub area: &'a str,
    pub level: &'a str,
    pub message: &'a str,
}

/// `{error, warning, info}` counts.
pub fn counts(diagnostics: &[Diagnostic]) -> Value {
    let count = |severity| {
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == severity)
            .count()
    };
    json!({
        "error": count(Severity::Error),
        "warning": count(Severity::Warning),
        "info": count(Severity::Info),
    })
}

/// Map entries to diagnostics, dropping repeats of the same problem at the same place.
pub fn map(entries: &[LogEntry], roots: &[PackRoot], source: &'static str) -> Vec<Diagnostic> {
    let mut mapper = Mapper {
        roots,
        walks: HashMap::new(),
        definitions: HashMap::new(),
        texts: HashMap::new(),
    };
    let mut context: Option<(String, Located)> = None;
    let mut seen = HashSet::new();
    let mut found: Vec<Diagnostic> = Vec::new();
    for entry in entries {
        let diagnostic = mapper.diagnostic(entry, &mut context, source);
        if seen.insert(diagnostic.clone()) {
            found.push(diagnostic);
        }
    }
    found
}

/// A file an entry points at.
#[derive(Clone, Debug)]
struct Located {
    /// `BP/...`, `RP/...`, or the path as written when no root holds it.
    label: String,
    /// The file on disk, when a root holds it.
    path: Option<PathBuf>,
}

/// What one `map` call has read, so a flood of entries about the same files reads each once.
struct Mapper<'a> {
    roots: &'a [PackRoot],
    /// Each root's files, relative with `/` separators.
    walks: HashMap<usize, Vec<String>>,
    /// The file defining each identifier looked up.
    definitions: HashMap<String, Option<Located>>,
    /// File texts, None when unreadable or too large.
    texts: HashMap<PathBuf, Option<Rc<str>>>,
}

impl Mapper<'_> {
    fn diagnostic(
        &mut self,
        entry: &LogEntry,
        context: &mut Option<(String, Located)>,
        source: &'static str,
    ) -> Diagnostic {
        let text = entry.message;
        let script = entry.area.eq_ignore_ascii_case("Scripting");
        let mut position = None;
        let mut located = None;
        if script {
            (located, position) = self.script_frame(text);
        }
        if located.is_none()
            && let Some(reference) = path_references(text).into_iter().next()
        {
            position = reference.line.map(|line| (line, reference.column));
            located = Some(self.resolve(&reference.path, pack_prefix(text)));
        }
        if located.is_none() {
            located = self.subject(entry);
        }
        match &located {
            Some(file) if is_header(text) => *context = Some((entry.area.to_owned(), file.clone())),
            Some(_) => *context = None,
            None => {
                located = context
                    .as_ref()
                    .filter(|(area, _)| area == entry.area)
                    .map(|(_, file)| file.clone());
                if located.is_none() {
                    *context = None;
                }
            }
        }
        let position = position.or_else(|| stated_position(text)).or_else(|| {
            let path = located.as_ref()?.path.clone()?;
            key_position(&self.text(&path)?, text)
        });
        Diagnostic {
            in_pack: located.as_ref().is_some_and(|file| file.path.is_some()),
            file: located.map(|file| file.label),
            line: position.map(|(line, _)| line),
            column: position.and_then(|(_, column)| column),
            severity: Severity::from_level(entry.level),
            rule: rule(entry.area, text),
            message: message(text, script),
            source,
        }
    }

    /// The first stack frame `(file:line[:column])` that a root holds.
    fn script_frame(&mut self, text: &str) -> (Option<Located>, Option<(usize, Option<usize>)>) {
        let prefix = pack_prefix(text);
        for reference in path_references(text) {
            let Some(line) = reference.line else {
                continue;
            };
            let located = self.resolve(&reference.path, prefix);
            if located.path.is_some() {
                return (Some(located), Some((line, reference.column)));
            }
        }
        (None, None)
    }

    /// A file named by what the entry says failed: `Error Parsing Item 'demo:a'` (the JSON
    /// file defining that identifier) or `Function name failed to load` (its .mcfunction).
    fn subject(&mut self, entry: &LogEntry) -> Option<Located> {
        let text = entry.message.trim();
        if let Some(rest) = text.strip_prefix("Function ")
            && let Some((name, _)) = rest.split_once(" failed to load")
        {
            return Some(self.resolve(&format!("functions/{name}.mcfunction"), None));
        }
        let id = text
            .find("Error Parsing ")
            .and_then(|at| quoted(&text[at..]).into_iter().next())?;
        self.defining_file(&id)
    }

    /// The one JSON file whose `"identifier"` is `id`.
    fn defining_file(&mut self, id: &str) -> Option<Located> {
        if let Some(known) = self.definitions.get(id) {
            return known.clone();
        }
        let needle = format!("\"{id}\"");
        let mut matches = Vec::new();
        for index in 0..self.roots.len() {
            let root = self.roots[index].path.clone();
            for relative in self.files(index).to_vec() {
                if !relative.ends_with(".json") {
                    continue;
                }
                let Some(text) = self.text(&root.join(&relative)) else {
                    continue;
                };
                if text.contains(&needle) && text.contains("\"identifier\"") {
                    matches.push((index, relative));
                }
            }
        }
        let found = match matches.as_slice() {
            [(index, relative)] => Some(self.located(*index, relative)),
            _ => None,
        };
        self.definitions.insert(id.to_owned(), found.clone());
        found
    }

    fn text(&mut self, path: &Path) -> Option<Rc<str>> {
        self.texts
            .entry(path.to_path_buf())
            .or_insert_with(|| {
                let size = fs::metadata(path).ok()?.len();
                (size <= MAX_FILE_BYTES)
                    .then(|| fs::read_to_string(path).ok().map(Rc::from))
                    .flatten()
            })
            .clone()
    }

    fn located(&self, index: usize, relative: &str) -> Located {
        let root = &self.roots[index];
        Located {
            label: format!("{}/{relative}", root.label()),
            path: Some(root.path.join(relative)),
        }
    }

    /// Find `path` (as a log entry wrote it) in the roots: the longest suffix of it that
    /// exists under exactly one root, directly or under `scripts/`, else the one file whose
    /// path ends with it. `pack` (a script error's `[name]`) picks between roots.
    fn resolve(&mut self, path: &str, pack: Option<&str>) -> Located {
        let normalized = path.replace('\\', "/");
        let parts: Vec<&str> = normalized
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .collect();
        let roots = self.roots;
        let candidates: Vec<usize> = (0..roots.len())
            .filter(|&index| pack.is_none_or(|name| roots[index].name == name))
            .collect();
        let candidates = if candidates.is_empty() {
            (0..roots.len()).collect()
        } else {
            candidates
        };
        for start in 0..parts.len() {
            let suffix = parts[start..].join("/");
            let hits: Vec<(usize, String)> = candidates
                .iter()
                .flat_map(|&index| {
                    [suffix.clone(), format!("scripts/{suffix}")]
                        .into_iter()
                        .filter(move |relative| roots[index].path.join(relative).is_file())
                        .map(move |relative| (index, relative))
                })
                .collect();
            if let [(index, relative)] = hits.as_slice() {
                return self.located(*index, relative);
            }
            if hits.len() > 1 {
                break;
            }
        }
        let ending = format!("/{}", parts.join("/"));
        let mut hits = Vec::new();
        for &index in &candidates {
            for relative in self.files(index) {
                if format!("/{relative}").ends_with(&ending) {
                    hits.push((index, relative.clone()));
                }
            }
        }
        match hits.as_slice() {
            [(index, relative)] => self.located(*index, relative),
            _ => Located {
                label: normalized,
                path: None,
            },
        }
    }

    fn files(&mut self, index: usize) -> &[String] {
        let root = &self.roots[index].path;
        self.walks.entry(index).or_insert_with(|| {
            let mut files = Vec::new();
            walk(root, "", &mut files);
            files
        })
    }
}

/// Relative paths of the files under `dir`, skipping hidden folders and `node_modules`,
/// at most [`MAX_FILES`].
fn walk(dir: &Path, prefix: &str, files: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if files.len() >= MAX_FILES {
            return;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let relative = format!("{prefix}{name}");
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if !name.starts_with('.') && name != "node_modules" {
                walk(&entry.path(), &format!("{relative}/"), files);
            }
        } else {
            files.push(relative);
        }
    }
}

/// A file path in a message, with the line and column written after it (`main.js:7:3`).
#[derive(Debug, PartialEq)]
struct PathReference {
    path: String,
    line: Option<usize>,
    column: Option<usize>,
}

fn path_references(text: &str) -> Vec<PathReference> {
    text.split(|c: char| c.is_whitespace() || "|()[]'\",".contains(c))
        .filter_map(|token| {
            let mut parts = token.trim_end_matches([':', '.', ';']).split(':');
            let path = parts.next()?;
            let numbers: Vec<usize> = parts.map_while(|part| part.parse().ok()).collect();
            let (_, extension) = path.rsplit_once('.')?;
            PATH_EXTENSIONS
                .contains(&extension.to_ascii_lowercase().as_str())
                .then(|| PathReference {
                    path: path.to_owned(),
                    line: numbers.first().copied(),
                    column: numbers.get(1).copied(),
                })
        })
        .collect()
}

/// A script error's leading `[Pack name]`.
fn pack_prefix(text: &str) -> Option<&str> {
    let rest = text.trim_start().strip_prefix('[')?;
    rest.split_once(']').map(|(name, _)| name)
}

/// Whether the entry introduces details that the next entries of its area give.
fn is_header(text: &str) -> bool {
    let text = text.trim_end();
    text.contains("Error Parsing ") || text.contains(" failed to load ") || text.ends_with(':')
}

/// Text between matching single or double quotes.
fn quoted(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(['\'', '"']) {
        let quote = rest[start..].chars().next().unwrap_or('"');
        let after = &rest[start + 1..];
        let Some(end) = after.find(quote) else {
            break;
        };
        found.push(after[..end].to_owned());
        rest = &after[end + 1..];
    }
    found
}

/// `Line 5, Column 3` (JSON parse errors) or `Error on line 2` (functions).
fn stated_position(text: &str) -> Option<(usize, Option<usize>)> {
    let lower = text.to_ascii_lowercase();
    lower.match_indices("line ").find_map(|(at, _)| {
        let word_start = lower[..at]
            .chars()
            .next_back()
            .is_none_or(|before| !before.is_alphanumeric());
        let rest = &lower[at + 5..];
        let line = leading_number(rest).filter(|_| word_start)?;
        let after = rest.trim_start_matches(|c: char| c.is_ascii_digit());
        let column = after
            .trim_start_matches([',', ' '])
            .strip_prefix("column ")
            .and_then(leading_number);
        Some((line, column))
    })
}

fn leading_number(text: &str) -> Option<usize> {
    let digits: String = text.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Where the key path (` -> a -> b: ...`) or an identifier the message quotes sits in the
/// file: the path's last key found key by key, else the first candidate that occurs exactly
/// once as a JSON string. Candidates in the reason (after the last ` | `) come first, since
/// the parts before it name the file and the definition as a whole.
fn key_position(file: &str, text: &str) -> Option<(usize, Option<usize>)> {
    if let Some(keys) = key_path(text) {
        let mut from = 0;
        let mut last = None;
        for key in keys {
            let at = find_key(file, &key, from)?;
            last = Some(at);
            from = at + key.len() + 2;
        }
        return last.map(|at| line_column(file, at));
    }
    let reason = text.rsplit_once(" | ").map_or(text, |(_, reason)| reason);
    candidates(reason)
        .into_iter()
        .chain(candidates(text))
        .find_map(|candidate| {
            let needle = format!("\"{candidate}\"");
            let first = file.find(&needle)?;
            file[first + 1..]
                .find(&needle)
                .is_none()
                .then(|| line_column(file, first))
        })
}

/// `Failed to parse field ' -> components -> minecraft:x: invalid value'` → the keys.
fn key_path(text: &str) -> Option<Vec<String>> {
    let start = text.find(" -> ")?;
    let mut keys: Vec<String> = text[start..]
        .split(" -> ")
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
        .collect();
    let last = keys.pop()?;
    // The last segment runs on into the reason: `key: reason'`.
    let last = last.split(": ").next().unwrap_or(&last);
    keys.push(last.trim_end_matches(['\'', '"']).to_owned());
    keys.retain(|key| !(key.starts_with('[') && key.ends_with(']')));
    (!keys.is_empty()).then_some(keys)
}

/// The byte offset of `"key"` used as an object key at or after `from`.
fn find_key(file: &str, key: &str, from: usize) -> Option<usize> {
    let needle = format!("\"{key}\"");
    let mut start = from;
    while let Some(offset) = file.get(start..)?.find(&needle) {
        let at = start + offset;
        if file[at + needle.len()..].trim_start().starts_with(':') {
            return Some(at);
        }
        start = at + needle.len();
    }
    None
}

/// Quoted text and namespaced identifiers in the message, most specific first.
fn candidates(text: &str) -> Vec<String> {
    let mut found: Vec<String> = quoted(text)
        .into_iter()
        .filter(|value| !value.is_empty() && !value.contains(char::is_whitespace))
        .collect();
    for word in text.split(|c: char| c.is_whitespace() || "|()[]'\",".contains(c)) {
        let word = word.trim_end_matches([':', '.', ';']);
        let namespaced = word
            .split_once(':')
            .is_some_and(|(space, name)| !space.is_empty() && !name.is_empty());
        if namespaced && !found.iter().any(|known| known == word) {
            found.push(word.to_owned());
        }
    }
    found.retain(|value| path_references(value).is_empty());
    found
}

/// 1-based line and character column of a byte offset.
fn line_column(text: &str, offset: usize) -> (usize, Option<usize>) {
    let before = &text[..offset];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |at| at + 1);
    (line, Some(before[line_start..].chars().count() + 1))
}

/// A short kebab-case code from the entry's area, `json-syntax` for parse errors.
fn rule(area: &str, text: &str) -> String {
    if stated_position(text).is_some() && text.contains("JSON") {
        return "json-syntax".into();
    }
    match area {
        "" => "content-log".into(),
        "Scripting" => "script".into(),
        "Recipes" => "recipe".into(),
        "Blocks" => "block".into(),
        "Actor" | "Entity" => "entity".into(),
        "Commands" => "command".into(),
        _ => kebab(area),
    }
}

fn kebab(name: &str) -> String {
    let mut out = String::new();
    for (index, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() && index > 0 {
            out.push('-');
        }
        if c.is_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_owned()
}

/// The message on one line; for script errors the error itself, without the stack or the
/// `Plugin ... ran with error: [...]` wrapper, so both reports of one error read the same.
fn message(text: &str, script: bool) -> String {
    let mut text = text.trim();
    if script && let Some(at) = error_start(text) {
        text = &text[at..];
        if let Some(end) = text.find(" at ").or_else(|| text.find('\n')) {
            text = &text[..end];
        }
        text = text.trim_end_matches([']', ' ']);
    }
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let flat = flat.trim_start_matches(['|', ' ']).to_owned();
    match flat.char_indices().nth(MAX_MESSAGE) {
        Some((cut, _)) => format!("{}…", &flat[..cut]),
        None => flat,
    }
}

/// Where `SomeError:` or `Error:` starts.
fn error_start(text: &str) -> Option<usize> {
    let at = text.find("Error: ")?;
    let start = text[..at]
        .rfind(|c: char| !c.is_ascii_alphanumeric())
        .map_or(0, |space| space + 1);
    Some(start)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir::TestDir;

    fn pack(dir: &Path, files: &[(&str, &str)]) -> PathBuf {
        for (path, text) in files {
            let path = dir.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        dir.to_path_buf()
    }

    const RECIPE: &str = "{\n  \"format_version\": \"1.20.10\",\n  \"minecraft:recipe_shaped\": {\n    \"description\": { \"identifier\": \"demo:test_item\" },\n    \"pattern\": [\"#X\"]\n  }\n}\n";
    const ITEM: &str = "{\n  \"minecraft:item\": {\n    \"description\": { \"identifier\": \"demo:thing\" },\n    \"components\": {\n      \"minecraft:icon\": \"x\",\n      \"minecraft:max_stack_size\": \"lots\"\n    }\n  }\n}\n";

    fn roots(dir: &TestDir) -> Vec<PackRoot> {
        let bp = pack(
            &dir.path().join("bp"),
            &[
                ("recipes/test_item.json", RECIPE),
                ("items/thing.json", ITEM),
                ("scripts/main.js", "import x from \"y\";\n\nboom();\n"),
                ("functions/test.mcfunction", "say hi\nnotacommand foo\n"),
            ],
        );
        let rp = pack(&dir.path().join("rp"), &[("textures/a.png", "")]);
        vec![
            PackRoot {
                kind: Kind::Behavior,
                path: bp,
                name: "Demo BP".into(),
            },
            PackRoot {
                kind: Kind::Resource,
                path: rp,
                name: "Demo RP".into(),
            },
        ]
    }

    fn one(roots: &[PackRoot], area: &str, level: &str, message: &str) -> Diagnostic {
        let mapped = map(
            &[LogEntry {
                area,
                level,
                message,
            }],
            roots,
            "content-log",
        );
        assert_eq!(mapped.len(), 1);
        mapped.into_iter().next().unwrap()
    }

    #[test]
    fn recipe_errors_point_at_the_identifier_they_name() {
        let dir = TestDir::new("diag-recipe");
        let roots = roots(&dir);
        let found = one(
            &roots,
            "Recipes",
            "error",
            "recipes/test_item.json | demo:test_item | 1.20+ Recipes require unlock data",
        );
        assert_eq!(found.file.as_deref(), Some("BP/recipes/test_item.json"));
        assert_eq!((found.line, found.column), (Some(4), Some(36)));
        assert_eq!(found.severity, Severity::Error);
        assert_eq!(found.rule, "recipe");
        assert_eq!(found.source, "content-log");
        assert!(found.in_pack);
    }

    #[test]
    fn script_errors_take_the_first_frame_and_collapse_the_plugin_wrapper() {
        let dir = TestDir::new("diag-script");
        let roots = roots(&dir);
        let entries = [
            LogEntry {
                area: "Scripting",
                level: "error",
                message: "[Demo BP] TypeError: cannot read property 'length' of undefined    at boom (main.js:3)\n    at <anonymous> (main.js:8)",
            },
            LogEntry {
                area: "Scripting",
                level: "error",
                message: "Plugin [Demo BP - 1.0.0] - [main.js] ran with error: [TypeError: cannot read property 'length' of undefined    at boom (main.js:3)\n    at <anonymous> (main.js:8)\n]",
            },
        ];
        let found = map(&entries, &roots, "bds");
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].file.as_deref(), Some("BP/scripts/main.js"));
        assert_eq!(found[0].line, Some(3));
        assert_eq!(found[0].column, None);
        assert_eq!(
            found[0].message,
            "TypeError: cannot read property 'length' of undefined"
        );
        assert_eq!(found[0].rule, "script");
    }

    #[test]
    fn parse_errors_keep_the_stated_line_and_column() {
        let dir = TestDir::new("diag-json");
        let roots = roots(&dir);
        let found = one(
            &roots,
            "Recipes",
            "ERROR",
            " | JSON: recipes/test_item.json has an error: \n * Line 5, Column 5\n  Missing ',' or '}' in object declaration",
        );
        assert_eq!(found.file.as_deref(), Some("BP/recipes/test_item.json"));
        assert_eq!((found.line, found.column), (Some(5), Some(5)));
        assert_eq!(found.rule, "json-syntax");
        assert!(found.message.starts_with("JSON: recipes/test_item.json"));
    }

    #[test]
    fn field_errors_follow_their_header_and_the_key_path() {
        let dir = TestDir::new("diag-item");
        let roots = roots(&dir);
        let entries = [
            LogEntry {
                area: "Item",
                level: "ERROR",
                message: " Error Parsing Item 'demo:thing':",
            },
            LogEntry {
                area: "Item",
                level: "ERROR",
                message: " Failed to parse field ' -> components -> minecraft:max_stack_size: invalid numeric value'",
            },
        ];
        let found = map(&entries, &roots, "bds");
        assert_eq!(found[0].file.as_deref(), Some("BP/items/thing.json"));
        assert_eq!(found[0].line, Some(3));
        assert_eq!(found[1].file.as_deref(), Some("BP/items/thing.json"));
        assert_eq!((found[1].line, found[1].column), (Some(6), Some(7)));
        assert_eq!(found[1].rule, "item");
    }

    #[test]
    fn function_errors_find_the_function_file_and_line() {
        let dir = TestDir::new("diag-function");
        let roots = roots(&dir);
        let entries = [
            LogEntry {
                area: "Commands",
                level: "WARN",
                message: "Function test failed to load correctly with error(s):",
            },
            LogEntry {
                area: "Commands",
                level: "WARN",
                message: "Error on line 2: unknown or invalid command 'notacommand'",
            },
        ];
        let found = map(&entries, &roots, "bds");
        assert_eq!(
            found[1].file.as_deref(),
            Some("BP/functions/test.mcfunction")
        );
        assert_eq!(found[1].line, Some(2));
        assert_eq!(found[1].severity, Severity::Warning);
        assert_eq!(found[1].rule, "command");
    }

    #[test]
    fn identifiers_in_the_reason_win_over_the_definition() {
        let dir = TestDir::new("diag-reason");
        let bp = pack(
            dir.path(),
            &[(
                "recipes/a.json",
                "{\n  \"description\": { \"identifier\": \"demo:a\" },\n  \"key\": { \"#\": { \"item\": \"demo:not_real\" } }\n}\n",
            )],
        );
        let roots = [PackRoot {
            kind: Kind::Behavior,
            path: bp,
            name: "Demo BP".into(),
        }];
        let found = one(
            &roots,
            "Recipes",
            "error",
            "recipes/a.json | demo:a | The Item: demo:not_real is missing or invalid, can't make the recipe",
        );
        assert_eq!((found.line, found.column), (Some(3), Some(27)));
    }

    #[test]
    fn unknown_places_stay_null_instead_of_guessed() {
        let dir = TestDir::new("diag-unknown");
        let roots = roots(&dir);
        let elsewhere = one(
            &roots,
            "Texture",
            "warning",
            "textures/missing.png | could not be found",
        );
        assert_eq!(elsewhere.file.as_deref(), Some("textures/missing.png"));
        assert_eq!(elsewhere.line, None);
        assert!(!elsewhere.in_pack);
        let absent = one(
            &roots,
            "Recipes",
            "error",
            "recipes/test_item.json | demo:other | unknown",
        );
        assert_eq!(absent.file.as_deref(), Some("BP/recipes/test_item.json"));
        assert_eq!(absent.line, None);
        let nothing = one(&roots, "", "info", "Server started.");
        assert_eq!((nothing.file, nothing.line), (None, None));
        assert_eq!(nothing.rule, "content-log");
        assert_eq!(nothing.severity, Severity::Info);
    }

    #[test]
    fn rules_are_kebab_case_areas() {
        assert_eq!(rule("LootTable", "x"), "loot-table");
        assert_eq!(rule("Molang", "x"), "molang");
        assert_eq!(rule("Json", "x"), "json");
    }

    #[test]
    fn counts_split_by_severity() {
        let diagnostic = |severity| Diagnostic {
            file: None,
            line: None,
            column: None,
            severity,
            rule: "x".into(),
            message: String::new(),
            source: "bds",
            in_pack: false,
        };
        let all = [
            diagnostic(Severity::Error),
            diagnostic(Severity::Error),
            diagnostic(Severity::Warning),
        ];
        assert_eq!(counts(&all), json!({ "error": 2, "warning": 1, "info": 0 }));
    }
}
