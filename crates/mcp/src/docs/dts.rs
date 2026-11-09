//! The JSDoc in a Script API package's `index.d.ts` (npm `@minecraft/server` and its
//! siblings), by declaration: `Entity`, `Entity.applyDamage`, `world`. The packages are
//! formatted by the same generator, so declarations are found by layout: top-level
//! `export ...` lines, members indented four spaces, each after its `/** ... */` comment.

use std::collections::HashMap;

/// At most this many lines make one declaration; stops a malformed file running on.
const MAX_DECLARATION_LINES: usize = 200;
const MAX_EXAMPLES: usize = 2;
/// Lines of example code kept per example.
const MAX_EXAMPLE_LINES: usize = 60;

/// The documentation of one declaration.
#[derive(Debug, Default, PartialEq)]
pub struct Doc {
    /// The TypeScript declaration with its whitespace collapsed.
    pub declaration: String,
    pub description: String,
    pub params: Vec<(String, String)>,
    pub returns: Option<String>,
    pub throws: Option<String>,
    pub deprecated: Option<String>,
    /// `(name, code)` pairs.
    pub examples: Vec<(String, String)>,
}

impl Doc {
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(param, _)| param == name)
            .map(|(_, text)| text.as_str())
    }

    /// The first sentence of the description.
    pub fn summary(&self) -> &str {
        let text = self.description.lines().next().unwrap_or_default();
        match text.find(". ") {
            Some(end) => &text[..=end],
            None => text,
        }
    }
}

/// Every documented declaration in `text`, keyed `Type`, `Type.member` or `name` for
/// top-level constants and functions. Overloads keep the first.
pub fn parse(text: &str) -> HashMap<String, Doc> {
    let lines: Vec<&str> = text.lines().collect();
    let mut docs = HashMap::new();
    let mut owner: Option<String> = None;
    let mut comment: Option<String> = None;
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim();
        if trimmed.starts_with("/**") {
            let (body, next) = read_comment(&lines, index);
            comment = Some(body);
            index = next;
            continue;
        }
        if line == "}" || line == "};" {
            owner = None;
            comment = None;
        } else if let Some(rest) = line.strip_prefix("export ") {
            let (declaration, next) = read_declaration(&lines, index);
            let (kind, name) = top_level(rest);
            if !name.is_empty() {
                if matches!(kind, "class" | "interface" | "enum") && trimmed.ends_with('{') {
                    owner = Some(name.clone());
                    insert(
                        &mut docs,
                        name,
                        trimmed.trim_end_matches('{').trim(),
                        comment.take(),
                    );
                    index += 1;
                    continue;
                }
                insert(&mut docs, name, &declaration, comment.take());
            }
            index = next;
            continue;
        } else if let Some(owner) = &owner
            && line.starts_with("    ")
            && !line.starts_with("     ")
        {
            let (declaration, next) = read_declaration(&lines, index);
            let name = member_name(trimmed);
            if !name.is_empty() {
                insert(
                    &mut docs,
                    format!("{owner}.{name}"),
                    &declaration,
                    comment.take(),
                );
            }
            comment = None;
            index = next;
            continue;
        }
        if !trimmed.is_empty() {
            comment = None;
        }
        index += 1;
    }
    docs
}

fn insert(
    docs: &mut HashMap<String, Doc>,
    key: String,
    declaration: &str,
    comment: Option<String>,
) {
    docs.entry(key).or_insert_with(|| {
        let mut doc = comment.as_deref().map(parse_comment).unwrap_or_default();
        doc.declaration = declaration.trim_end_matches([';', ',']).to_owned();
        doc
    });
}

/// The kind word and name after `export `: `class Entity`, `const world: World`.
fn top_level(rest: &str) -> (&str, String) {
    let mut words = rest
        .split_whitespace()
        .skip_while(|word| matches!(*word, "declare" | "abstract"));
    let kind = words.next().unwrap_or_default();
    let name = words.next().map(identifier).unwrap_or_default();
    (kind, name)
}

/// The leading identifier of `text`.
fn identifier(text: &str) -> String {
    text.chars()
        .take_while(|char| char.is_alphanumeric() || *char == '_' || *char == '$')
        .collect()
}

/// A member's name, after its modifiers: `readonly id: string` is `id`.
fn member_name(declaration: &str) -> String {
    let mut rest = declaration;
    while let Some(next) = [
        "readonly ",
        "static ",
        "get ",
        "set ",
        "private ",
        "protected ",
        "abstract ",
    ]
    .iter()
    .find_map(|modifier| rest.strip_prefix(modifier))
    {
        rest = next;
    }
    identifier(rest)
}

/// The comment starting at line `start` without its `*` margins, and the line after it.
fn read_comment(lines: &[&str], start: usize) -> (String, usize) {
    let mut body = Vec::new();
    let mut index = start;
    while index < lines.len() {
        let line = lines[index].trim();
        let done = line.ends_with("*/");
        let line = line.trim_start_matches("/**").trim_end_matches("*/");
        let line = line.strip_prefix('*').unwrap_or(line);
        body.push(line.strip_prefix(' ').unwrap_or(line).trim_end());
        index += 1;
        if done {
            break;
        }
    }
    (body.join("\n"), index)
}

/// The declaration starting at line `start`, ended by `;` or `,` outside brackets (or the
/// `{` that opens a type), with its whitespace collapsed, and the line after it.
fn read_declaration(lines: &[&str], start: usize) -> (String, usize) {
    let mut text = String::new();
    let mut depth = 0i32;
    let end = lines.len().min(start + MAX_DECLARATION_LINES);
    for (index, line) in lines.iter().enumerate().take(end).skip(start) {
        for char in line.chars() {
            match char {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth -= 1,
                _ => {}
            }
        }
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(line.trim());
        let finished = depth <= 0 && (text.ends_with(';') || text.ends_with(','))
            || (index == start && depth == 1 && text.ends_with('{'));
        if finished {
            return (text, index + 1);
        }
    }
    (text, end.max(start + 1))
}

/// Which part of a comment a line belongs to.
enum Section {
    Description,
    Param(String),
    Returns,
    Throws,
    Deprecated,
    Example(String),
    Other,
}

/// The parts of a JSDoc comment body.
fn parse_comment(body: &str) -> Doc {
    let mut doc = Doc::default();
    let mut section = Section::Description;
    let mut parts: Vec<(Section, Vec<&str>)> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in body.lines() {
        let Some(tag) = line.strip_prefix('@') else {
            current.push(line);
            continue;
        };
        parts.push((section, std::mem::take(&mut current)));
        let (name, rest) = tag.split_once(' ').unwrap_or((tag, ""));
        section = match name {
            "remarks" => Section::Description,
            "param" => {
                let (param, text) = rest.split_once(' ').unwrap_or((rest, ""));
                current.push(text);
                Section::Param(param.to_owned())
            }
            "returns" => Section::Returns,
            "throws" => Section::Throws,
            "deprecated" => Section::Deprecated,
            "example" => Section::Example(rest.trim().to_owned()),
            _ => Section::Other,
        };
        if !matches!(
            section,
            Section::Param(_) | Section::Example(_) | Section::Other
        ) {
            current.push(rest);
        }
    }
    parts.push((section, current));
    for (section, lines) in parts {
        match section {
            Section::Description => append(&mut doc.description, &prose(&lines)),
            Section::Param(name) => doc.params.push((name, prose(&lines))),
            Section::Returns => doc.returns = Some(prose(&lines)).filter(|text| !text.is_empty()),
            Section::Throws => doc.throws = Some(prose(&lines)).filter(|text| !text.is_empty()),
            Section::Deprecated => doc.deprecated = Some(prose(&lines)),
            Section::Example(name) if doc.examples.len() < MAX_EXAMPLES => {
                let code: Vec<&str> = lines
                    .iter()
                    .filter(|line| !line.starts_with("```"))
                    .take(MAX_EXAMPLE_LINES)
                    .copied()
                    .collect();
                doc.examples.push((name, code.join("\n").trim().to_owned()));
            }
            Section::Example(_) | Section::Other => {}
        }
    }
    doc
}

fn append(text: &mut String, more: &str) {
    if more.is_empty() {
        return;
    }
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(more);
}

/// Comment lines as prose: lines joined into paragraphs, paragraphs by newlines, and
/// `{@link X}` as `X`.
fn prose(lines: &[&str]) -> String {
    let mut paragraphs: Vec<String> = vec![String::new()];
    for line in lines {
        let line = line.trim();
        let last = paragraphs.last_mut().expect("starts with one paragraph");
        if line.is_empty() {
            if !last.is_empty() {
                paragraphs.push(String::new());
            }
            continue;
        }
        if !last.is_empty() {
            last.push(' ');
        }
        last.push_str(&unlink(line));
    }
    paragraphs.retain(|paragraph| !paragraph.is_empty());
    paragraphs.join("\n")
}

/// `{@link minecraftcommon.EngineError}` as `minecraftcommon.EngineError`.
fn unlink(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(start) = rest.find("{@link ") {
        out.push_str(&rest[..start]);
        let after = &rest[start + "{@link ".len()..];
        let Some(end) = after.find('}') else {
            rest = after;
            break;
        };
        out.push_str(after[..end].trim());
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A tiny `index.d.ts` laid out like the published packages.
    pub(crate) const INDEX: &str = r"import * as minecraftcommon from '@minecraft/common';
/**
 * Kinds of damage.
 */
export enum DamageCause {
    /**
     * @remarks
     * Fell too far.
     *
     */
    fall = 'fall',
}
/**
 * Represents the state of an entity.
 */
export class Entity {
    private constructor();
    /**
     * @remarks
     * Unique identifier of the entity.
     *
     */
    readonly id: string;
    /**
     * @remarks
     * Applies a set of damage to an entity.
     *
     * @param amount
     * Amount of damage to apply.
     * @param options
     * Additional options about the source of damage.
     * @returns
     * Whether the entity takes any damage.
     * @throws This function can throw errors.
     *
     * {@link minecraftcommon.EngineError}
     * @example hurt.ts
     * ```typescript
     * entity.applyDamage(19);
     * ```
     */
    applyDamage(amount: number, options?: EntityApplyDamageOptions): boolean;
    /**
     * @remarks
     * Adds a callback.
     *
     */
    subscribe(
        callback: (arg0: Entity) => void,
    ): (arg0: Entity) => void;
}
/**
 * A class that wraps the state of a world.
 */
export class World {
    private constructor();
    readonly afterEvents: WorldAfterEvents;
}
/**
 * @remarks
 * A class that wraps the state of a world.
 *
 */
export const world: World;
";

    #[test]
    fn declarations_are_found_with_their_comments() {
        let docs = parse(INDEX);
        let damage = &docs["Entity.applyDamage"];
        assert_eq!(
            damage.declaration,
            "applyDamage(amount: number, options?: EntityApplyDamageOptions): boolean"
        );
        assert_eq!(damage.description, "Applies a set of damage to an entity.");
        assert_eq!(damage.param("amount"), Some("Amount of damage to apply."));
        assert_eq!(
            damage.returns.as_deref(),
            Some("Whether the entity takes any damage.")
        );
        assert_eq!(
            damage.throws.as_deref(),
            Some("This function can throw errors.\nminecraftcommon.EngineError")
        );
        assert_eq!(
            damage.examples,
            [("hurt.ts".to_owned(), "entity.applyDamage(19);".to_owned())]
        );
        assert_eq!(docs["Entity.id"].declaration, "readonly id: string");
        assert_eq!(
            docs["Entity.subscribe"].declaration,
            "subscribe( callback: (arg0: Entity) => void, ): (arg0: Entity) => void"
        );
        assert_eq!(
            docs["Entity"].summary(),
            "Represents the state of an entity."
        );
        assert_eq!(docs["DamageCause.fall"].description, "Fell too far.");
        assert_eq!(docs["world"].declaration, "export const world: World");
        // An undocumented member is still known by its declaration.
        assert_eq!(docs["World.afterEvents"].description, "");
        assert_eq!(
            docs["Entity.constructor"].declaration,
            "private constructor()"
        );
    }
}
