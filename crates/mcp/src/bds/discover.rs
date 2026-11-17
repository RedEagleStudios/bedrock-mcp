//! Finds the GameTests a behavior pack registers by reading its scripts: the
//! `register("<class>", "<name>", ...)` and `registerAsync(...)` calls of
//! `@minecraft/server-gametest` with literal names, and the literal `.tag(...)` and `.batch(...)`
//! calls chained to them. It reads bundled and minified output too, where the import renames
//! `register`. BDS has no command that lists registered tests, so this is the only way to know
//! them before they run.

use std::{
    fs,
    path::{Path, PathBuf},
};

const MODULE: &str = "@minecraft/server-gametest";
const REGISTER: [&str; 2] = ["register", "registerAsync"];
/// Bounds on what one discovery reads.
const MAX_FILES: usize = 2_000;
const MAX_BYTES: u64 = 64 << 20;

/// A registered test.
#[derive(Clone, Debug, PartialEq)]
pub struct Registered {
    /// `<class>:<name>`, as `/gametest run` takes it.
    pub id: String,
    pub class: String,
    pub tags: Vec<String>,
    pub batch: Option<String>,
    /// A chained `.tag(...)` or `.batch(...)` whose argument is not a string literal.
    pub unknown_chain: bool,
}

impl Registered {
    /// Whether `/gametest runset <tag>` runs this test: `suite:all` runs every test, other tags
    /// match a test's class or one of its tags.
    pub fn matches(&self, tag: &str) -> bool {
        tag == "suite:all" || self.class == tag || self.tags.iter().any(|known| known == tag)
    }
}

/// The tests found in a set of packs, and why the list may be incomplete.
#[derive(Debug, Default)]
pub struct Discovery {
    pub tests: Vec<Registered>,
    /// Calls whose names are not literals, as `file: call`; the tests they register are unknown.
    pub unresolved: Vec<String>,
    pub files: usize,
    /// Set when the bounds stopped the scan.
    pub truncated: bool,
}

/// Scan the `scripts` folder of each behavior pack folder.
pub fn discover(packs: &[PathBuf]) -> Discovery {
    let mut discovery = Discovery::default();
    let mut budget = MAX_BYTES;
    for pack in packs {
        let mut files = Vec::new();
        collect_scripts(&pack.join("scripts"), &mut files, &mut discovery.truncated);
        files.sort();
        for file in files {
            if discovery.files >= MAX_FILES {
                discovery.truncated = true;
                return discovery;
            }
            let Ok(metadata) = fs::metadata(&file) else {
                continue;
            };
            if metadata.len() > budget {
                discovery.truncated = true;
                return discovery;
            }
            budget -= metadata.len();
            let Ok(text) = fs::read_to_string(&file) else {
                continue;
            };
            discovery.files += 1;
            let name = file
                .strip_prefix(pack)
                .unwrap_or(&file)
                .display()
                .to_string();
            scan(&text, &name, &mut discovery);
        }
    }
    discovery
}

fn collect_scripts(dir: &Path, files: &mut Vec<PathBuf>, truncated: &mut bool) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if files.len() >= MAX_FILES {
            *truncated = true;
            return;
        }
        let path = entry.path();
        if path.is_dir() {
            collect_scripts(&path, files, truncated);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "js" || extension == "mjs")
        {
            files.push(path);
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Ident(String),
    /// A string or a template without substitutions.
    Str(String),
    /// A template with substitutions.
    Template,
    Punct(char),
    Other,
}

/// Add the tests `text` registers to `discovery`.
pub fn scan(text: &str, file: &str, discovery: &mut Discovery) {
    let tokens = tokenize(text);
    let (functions, namespaces) = imports(&tokens);
    if functions.is_empty() && namespaces.is_empty() {
        return;
    }
    let mut at = 0;
    while at < tokens.len() {
        let Some(open) = register_call(&tokens, at, &functions, &namespaces) else {
            at += 1;
            continue;
        };
        let close = matching(&tokens, open);
        match literal_names(&tokens[open + 1..close]) {
            Some((class, name)) => {
                let mut test = Registered {
                    id: format!("{class}:{name}"),
                    class,
                    tags: Vec::new(),
                    batch: None,
                    unknown_chain: false,
                };
                chain(&tokens, close + 1, &mut test);
                if !discovery.tests.iter().any(|known| known.id == test.id) {
                    discovery.tests.push(test);
                }
            }
            None => discovery
                .unresolved
                .push(format!("{file}: a register call without literal names")),
        }
        at = open + 1;
    }
}

/// Local names bound to `register`/`registerAsync`, and namespace imports of the module.
fn imports(tokens: &[Token]) -> (Vec<String>, Vec<String>) {
    let mut functions = Vec::new();
    let mut namespaces = Vec::new();
    for (at, token) in tokens.iter().enumerate() {
        if *token != Token::Str(MODULE.into()) || at < 2 {
            continue;
        }
        if tokens[at - 1] != Token::Ident("from".into()) {
            continue;
        }
        match &tokens[at - 2] {
            Token::Punct('}') => {
                let Some(open) = tokens[..at - 2]
                    .iter()
                    .rposition(|token| *token == Token::Punct('{'))
                else {
                    continue;
                };
                for item in tokens[open + 1..at - 2].split(|token| *token == Token::Punct(',')) {
                    match item {
                        [Token::Ident(name)] if REGISTER.contains(&name.as_str()) => {
                            functions.push(name.clone())
                        }
                        [Token::Ident(name), Token::Ident(word), Token::Ident(local)]
                            if word == "as" && REGISTER.contains(&name.as_str()) =>
                        {
                            functions.push(local.clone())
                        }
                        _ => {}
                    }
                }
            }
            Token::Ident(local)
                if at >= 4
                    && tokens[at - 3] == Token::Ident("as".into())
                    && tokens[at - 4] == Token::Punct('*') =>
            {
                namespaces.push(local.clone())
            }
            _ => {}
        }
    }
    (functions, namespaces)
}

/// The index of the `(` when a register call starts at `at`.
fn register_call(
    tokens: &[Token],
    at: usize,
    functions: &[String],
    namespaces: &[String],
) -> Option<usize> {
    let Token::Ident(name) = &tokens[at] else {
        return None;
    };
    let after_dot = at > 0 && tokens[at - 1] == Token::Punct('.');
    if functions.contains(name) && !after_dot {
        return (tokens.get(at + 1) == Some(&Token::Punct('('))).then_some(at + 1);
    }
    if namespaces.contains(name)
        && !after_dot
        && tokens.get(at + 1) == Some(&Token::Punct('.'))
        && matches!(tokens.get(at + 2), Some(Token::Ident(call)) if REGISTER.contains(&call.as_str()))
        && tokens.get(at + 3) == Some(&Token::Punct('('))
    {
        return Some(at + 3);
    }
    None
}

/// The index of the bracket that closes the one at `open`, or the end.
fn matching(tokens: &[Token], open: usize) -> usize {
    let mut depth = 0usize;
    for (at, token) in tokens.iter().enumerate().skip(open) {
        match token {
            Token::Punct('(' | '[' | '{') => depth += 1,
            Token::Punct(')' | ']' | '}') => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return at;
                }
            }
            _ => {}
        }
    }
    tokens.len()
}

/// `"class", "name", ...` → the two names.
fn literal_names(arguments: &[Token]) -> Option<(String, String)> {
    match arguments {
        [
            Token::Str(class),
            Token::Punct(','),
            Token::Str(name),
            Token::Punct(','),
            ..,
        ] => Some((class.clone(), name.clone())),
        _ => None,
    }
}

/// Follow `.method(args)` calls after a registration.
fn chain(tokens: &[Token], mut at: usize, test: &mut Registered) {
    while tokens.get(at) == Some(&Token::Punct('.')) {
        let (Some(Token::Ident(method)), Some(Token::Punct('('))) =
            (tokens.get(at + 1), tokens.get(at + 2))
        else {
            return;
        };
        let close = matching(tokens, at + 2);
        let argument = match tokens.get(at + 3..close) {
            Some([Token::Str(value)]) => Some(value.clone()),
            _ => None,
        };
        match (method.as_str(), argument) {
            ("tag", Some(tag)) => test.tags.push(tag),
            ("batch", Some(batch)) => test.batch = Some(batch),
            ("tag" | "batch", None) => test.unknown_chain = true,
            _ => {}
        }
        at = close + 1;
    }
}

/// Split JavaScript into the tokens the scan needs; it skips comments and regex literals.
fn tokenize(text: &str) -> Vec<Token> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        let c = chars[at];
        if c.is_whitespace() {
            at += 1;
        } else if c == '/' && chars.get(at + 1) == Some(&'/') {
            while at < chars.len() && chars[at] != '\n' {
                at += 1;
            }
        } else if c == '/' && chars.get(at + 1) == Some(&'*') {
            at += 2;
            while at < chars.len() && !(chars[at] == '*' && chars.get(at + 1) == Some(&'/')) {
                at += 1;
            }
            at += 2;
        } else if c == '"' || c == '\'' {
            let (value, end) = quoted(&chars, at);
            tokens.push(Token::Str(value));
            at = end;
        } else if c == '`' {
            let (value, end) = template(&chars, at);
            tokens.push(value.map_or(Token::Template, Token::Str));
            at = end;
        } else if c == '/' && regex_allowed(tokens.last()) {
            at = regex_end(&chars, at);
            tokens.push(Token::Other);
        } else if c.is_alphabetic() || c == '_' || c == '$' {
            let start = at;
            while at < chars.len()
                && (chars[at].is_alphanumeric() || chars[at] == '_' || chars[at] == '$')
            {
                at += 1;
            }
            tokens.push(Token::Ident(chars[start..at].iter().collect()));
        } else if c.is_ascii_digit() {
            while at < chars.len() && (chars[at].is_alphanumeric() || chars[at] == '.') {
                at += 1;
            }
            tokens.push(Token::Other);
        } else {
            tokens.push(Token::Punct(c));
            at += 1;
        }
    }
    tokens
}

/// A string literal starting at `start`: its value and the index after it.
fn quoted(chars: &[char], start: usize) -> (String, usize) {
    let quote = chars[start];
    let mut value = String::new();
    let mut at = start + 1;
    while at < chars.len() && chars[at] != quote && chars[at] != '\n' {
        if chars[at] == '\\' && at + 1 < chars.len() {
            at += 1;
            value.push(match chars[at] {
                'n' => '\n',
                't' => '\t',
                other => other,
            });
        } else {
            value.push(chars[at]);
        }
        at += 1;
    }
    (value, at + 1)
}

/// A template literal: its text when it has no substitutions, and the index after it.
fn template(chars: &[char], start: usize) -> (Option<String>, usize) {
    let mut value = String::new();
    let mut plain = true;
    let mut at = start + 1;
    while at < chars.len() && chars[at] != '`' {
        if chars[at] == '\\' && at + 1 < chars.len() {
            value.push(chars[at + 1]);
            at += 2;
        } else if chars[at] == '$' && chars.get(at + 1) == Some(&'{') {
            plain = false;
            at = substitution_end(chars, at + 2);
        } else {
            value.push(chars[at]);
            at += 1;
        }
    }
    (plain.then_some(value), at + 1)
}

/// The index after the `}` that closes a template substitution whose body starts at `start`.
fn substitution_end(chars: &[char], start: usize) -> usize {
    let mut depth = 1;
    let mut at = start;
    while at < chars.len() {
        match chars[at] {
            '"' | '\'' => {
                at = quoted(chars, at).1;
                continue;
            }
            '`' => {
                at = template(chars, at).1;
                continue;
            }
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return at + 1;
                }
            }
            _ => {}
        }
        at += 1;
    }
    at
}

/// A `/` starts a regex literal unless it follows a value.
fn regex_allowed(previous: Option<&Token>) -> bool {
    match previous {
        None => true,
        Some(Token::Punct(c)) => !matches!(c, ')' | ']' | '}'),
        Some(Token::Ident(word)) => matches!(
            word.as_str(),
            "return" | "typeof" | "case" | "do" | "else" | "in" | "of" | "new" | "delete" | "void"
        ),
        Some(_) => false,
    }
}

fn regex_end(chars: &[char], start: usize) -> usize {
    let mut at = start + 1;
    let mut class = false;
    while at < chars.len() && chars[at] != '\n' {
        match chars[at] {
            '\\' => at += 1,
            '[' => class = true,
            ']' => class = false,
            '/' if !class => {
                at += 1;
                while at < chars.len() && chars[at].is_alphabetic() {
                    at += 1;
                }
                return at;
            }
            _ => {}
        }
        at += 1;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(text: &str) -> Discovery {
        let mut discovery = Discovery::default();
        scan(text, "scripts/main.js", &mut discovery);
        discovery
    }

    #[test]
    fn plain_imports_give_names_tags_and_batches() {
        let discovery = found(
            r#"
            import { register, registerAsync } from "@minecraft/server-gametest";
            // register("commented", "out", () => {});
            register("demo", "walk", (test) => { test.succeed(); }).maxTicks(200).tag("smoke").tag('slow');
            registerAsync('demo', `jump`, async (test) => { const r = /"(/g; test.succeed(); })
              .batch("night")
              .structureName("demo:empty");
            const label = "register(\"x\", \"y\")";
            "#,
        );
        let ids: Vec<&str> = discovery
            .tests
            .iter()
            .map(|test| test.id.as_str())
            .collect();
        assert_eq!(ids, ["demo:walk", "demo:jump"]);
        assert_eq!(discovery.tests[0].tags, ["smoke", "slow"]);
        assert_eq!(discovery.tests[1].batch.as_deref(), Some("night"));
        assert!(discovery.unresolved.is_empty());
        assert!(discovery.tests[0].matches("smoke") && discovery.tests[0].matches("demo"));
        assert!(discovery.tests[1].matches("suite:all") && !discovery.tests[1].matches("smoke"));
    }

    #[test]
    fn minified_bundles_rename_the_import() {
        let discovery = found(
            r#"import{world as w}from"@minecraft/server";import{register as Q,SimulatedPlayer as H}from"@minecraft/server-gametest";Q("demo","a",t=>{t.succeed()}).tag("x");Q("demo","b",t=>t.fail("no"));w.register("not","this",1);"#,
        );
        let ids: Vec<&str> = discovery
            .tests
            .iter()
            .map(|test| test.id.as_str())
            .collect();
        assert_eq!(ids, ["demo:a", "demo:b"]);
        assert_eq!(discovery.tests[0].tags, ["x"]);
    }

    #[test]
    fn namespace_imports_and_dynamic_names() {
        let discovery = found(
            r#"import * as gt from "@minecraft/server-gametest";
            gt.register("demo", "fixed", () => {}).tag(TAG);
            for (const n of names) gt.register("demo", `case_${n}`, () => {});
            gt.register("demo", "x" + n, () => {});"#,
        );
        assert_eq!(discovery.tests.len(), 1);
        assert!(discovery.tests[0].unknown_chain);
        assert_eq!(discovery.unresolved.len(), 2);
    }

    #[test]
    fn scripts_without_the_module_register_nothing() {
        let discovery = found(r#"function register(a, b) {} register("demo", "x", 1);"#);
        assert!(discovery.tests.is_empty() && discovery.unresolved.is_empty());
    }
}
