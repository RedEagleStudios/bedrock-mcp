//! The names slash commands refer to: `function`, `summon`, `give`, `setblock`, `fill`,
//! `playsound` and `particle`, also after `execute ... run` and the older
//! `execute <target> <x y z>` form. Other commands are not read.

use super::{Kind, Reference};
use crate::docs::jsonpos::{Node, Pos};

/// The names in a .mcfunction file, each at its own line and column.
pub fn function_references(text: &str, found: &mut Vec<Reference>) {
    for (index, line) in text.lines().enumerate() {
        let command = line.trim_start();
        if command.is_empty() || command.starts_with('#') {
            continue;
        }
        for (kind, name, offset) in names(line) {
            found.push(Reference {
                kind,
                name,
                pos: Pos {
                    line: u32::try_from(index + 1).unwrap_or(u32::MAX),
                    column: u32::try_from(line[..offset].chars().count() + 1).unwrap_or(u32::MAX),
                },
            });
        }
    }
}

/// The names in a command written as a JSON string, at the string's position (escapes
/// make columns inside the string unreliable).
pub fn json_references((node, text): (&Node, &str)) -> Vec<Reference> {
    names(text)
        .into_iter()
        .map(|(kind, name, _)| Reference {
            kind,
            name,
            pos: node.pos,
        })
        .collect()
}

/// Each name in `command` with its kind and byte offset.
fn names(command: &str) -> Vec<(Kind, String, usize)> {
    let mut found = Vec::new();
    names_in(&tokens(command), &mut found);
    found
}

/// Words split at spaces outside quotes and brackets, so `@a[tag=a, r=5]` and `"a b"` stay
/// whole, each with its byte offset.
fn tokens(command: &str) -> Vec<(usize, &str)> {
    let mut tokens = Vec::new();
    let mut start = None;
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for (offset, character) in command.char_indices() {
        if character.is_whitespace() && depth == 0 && !quoted {
            if let Some(begin) = start.take() {
                tokens.push((begin, &command[begin..offset]));
            }
            continue;
        }
        start.get_or_insert(offset);
        match character {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            '[' | '{' if !quoted => depth += 1,
            ']' | '}' if !quoted => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    if let Some(begin) = start {
        tokens.push((begin, &command[begin..]));
    }
    tokens
}

fn is_coordinate(token: &str) -> bool {
    token.starts_with(['~', '^']) || token.parse::<f64>().is_ok()
}

fn names_in(tokens: &[(usize, &str)], found: &mut Vec<(Kind, String, usize)>) {
    let Some(&(_, command)) = tokens.first() else {
        return;
    };
    let (kind, index) = match command.trim_start_matches('/') {
        "execute" => {
            if let Some(run) = tokens.iter().position(|&(_, token)| token == "run") {
                names_in(&tokens[run + 1..], found);
            } else if tokens.len() > 5
                && tokens[2..5].iter().all(|&(_, token)| is_coordinate(token))
            {
                // `execute <target> <x y z> [detect <x y z> <block> <data>] <command>`
                let start = if tokens[5].1 == "detect" { 11 } else { 5 };
                names_in(tokens.get(start..).unwrap_or_default(), found);
            }
            return;
        }
        "function" => (Kind::Function, 1),
        "summon" => (Kind::Entity, 1),
        "give" => (Kind::Item, 2),
        "setblock" => (Kind::Block, 4),
        "fill" => (Kind::Block, 7),
        "playsound" => (Kind::Sound, 1),
        "particle" => (Kind::Particle, 1),
        _ => return,
    };
    let Some(&(offset, token)) = tokens.get(index) else {
        return;
    };
    // A block's states may follow it without a space: `stone["stone_type"="granite"]`.
    let name = token.split('[').next().unwrap_or_default();
    let unquoted = name.trim_matches('"');
    let offset = offset + usize::from(unquoted.len() != name.len());
    if !unquoted.is_empty() {
        found.push((kind, unquoted.to_owned(), offset));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(command: &str) -> Vec<(Kind, String, usize)> {
        names(command)
    }

    #[test]
    fn reads_the_name_of_each_command() {
        assert_eq!(
            found("function demo/start"),
            [(Kind::Function, "demo/start".into(), 9)]
        );
        assert_eq!(
            found("/summon demo:bob ~ ~ ~"),
            [(Kind::Entity, "demo:bob".into(), 8)]
        );
        assert_eq!(
            found("give @a[tag=a, r=5] demo:gem 2"),
            [(Kind::Item, "demo:gem".into(), 20)]
        );
        assert_eq!(
            found("setblock ~ ~1 ~ stone[\"stone_type\"=\"granite\"]"),
            [(Kind::Block, "stone".into(), 16)]
        );
        assert_eq!(
            found("fill 0 0 0 1 1 1 demo:ore replace"),
            [(Kind::Block, "demo:ore".into(), 17)]
        );
        assert_eq!(
            found("playsound random.pop @s"),
            [(Kind::Sound, "random.pop".into(), 10)]
        );
        assert_eq!(
            found("function \"demo/a b\""),
            [(Kind::Function, "demo/a b".into(), 10)]
        );
        assert!(found("say function demo/x").is_empty());
        assert!(found("summon").is_empty());
    }

    #[test]
    fn follows_execute_run_and_the_older_form() {
        assert_eq!(
            found("execute as @a[name=\"a b\"] at @s run summon demo:bob"),
            [(Kind::Entity, "demo:bob".into(), 43)]
        );
        assert_eq!(
            found("execute @s ~ ~ ~ function demo/x"),
            [(Kind::Function, "demo/x".into(), 26)]
        );
        assert_eq!(
            found("execute @s ~ ~ ~ detect ~ ~-1 ~ stone 0 give @s demo:gem"),
            [(Kind::Item, "demo:gem".into(), 48)]
        );
    }

    #[test]
    fn function_files_give_line_and_column() {
        let mut references = Vec::new();
        function_references(
            "# comment\n  function demo/x\nsay é function demo/y\nsummon demo:bob",
            &mut references,
        );
        let at: Vec<_> = references
            .iter()
            .map(|reference| {
                (
                    reference.name.as_str(),
                    reference.pos.line,
                    reference.pos.column,
                )
            })
            .collect();
        assert_eq!(at, [("demo/x", 2, 12), ("demo:bob", 4, 8)]);
    }
}
