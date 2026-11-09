//! Reads the content log: pack and script errors and warnings
//! (https://learn.microsoft.com/en-us/minecraft/creator/documents/contenterrorlog). That page
//! does not describe the file, so the format here is what the code parses: one file per game
//! session (`logs/ContentLog<start>_<n>.txt`), entries `HH:MM:SS[Area][level]-message`
//! separated by a blank line. The code expects the game to write in buffered chunks, so a
//! read can end mid-entry; a cursor only advances past entries whose separator has arrived.

use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

use serde::Serialize;
use serde_json::{Value, json};

use crate::error::{Result, bail};

/// Longest read per call; later entries come with the next cursor.
const MAX_READ: u64 = 4 << 20;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Entry {
    pub time: String,
    pub area: String,
    pub level: String,
    pub message: String,
}

impl Entry {
    fn parse(text: &str) -> Self {
        let parsed = (|| {
            let (time, rest) = text.split_once('[')?;
            let (area, rest) = rest.split_once(']')?;
            let rest = rest.strip_prefix('[')?;
            let (level, rest) = rest.split_once(']')?;
            let message = rest.strip_prefix('-').unwrap_or(rest);
            Some(Self {
                time: time.trim().to_owned(),
                area: area.to_owned(),
                level: level.to_lowercase(),
                message: message.to_owned(),
            })
        })();
        parsed.unwrap_or_else(|| Self {
            time: String::new(),
            area: String::new(),
            level: "unknown".into(),
            message: text.to_owned(),
        })
    }
}

/// A position in one content log: `file name` + byte offset, encoded as `name@offset`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub file: String,
    pub offset: u64,
}

impl Cursor {
    pub fn parse(text: &str) -> Result<Self> {
        match text
            .rsplit_once('@')
            .and_then(|(file, offset)| Some((file, offset.parse().ok()?)))
        {
            Some((file, offset)) if !file.is_empty() => Ok(Self {
                file: file.to_owned(),
                offset,
            }),
            _ => bail!(
                "Invalid content log cursor {text:?}; pass back the cursor a previous call returned."
            ),
        }
    }

    pub fn encode(&self) -> String {
        format!("{}@{}", self.file, self.offset)
    }
}

/// Complete entries in `data` and how many bytes they span (through their separator).
pub fn parse_entries(data: &[u8]) -> (Vec<Entry>, usize) {
    let mut entries = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut consumed = 0;
    let mut position = 0;
    while let Some(length) = data[position..].iter().position(|&byte| byte == b'\n') {
        let line = String::from_utf8_lossy(&data[position..position + length]);
        let line = line.trim_end_matches('\r');
        position += length + 1;
        if line.trim().is_empty() {
            if !current.is_empty() {
                entries.push(Entry::parse(&current.join("\n")));
                current.clear();
            }
            consumed = position;
        } else {
            current.push(line.to_owned());
        }
    }
    (entries, consumed)
}

/// Entries after `cursor` in the newest content log (`logs` is newest first). A cursor for
/// another file (an older session) reads the newest file from its start.
pub fn read(
    logs: &[impl AsRef<Path>],
    cursor: Option<&Cursor>,
) -> Result<(Vec<Entry>, Option<Cursor>)> {
    let Some(newest) = logs.first().map(AsRef::as_ref) else {
        return Ok((Vec::new(), cursor.cloned()));
    };
    let name = newest
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let mut offset = cursor
        .filter(|cursor| cursor.file == name)
        .map_or(0, |cursor| cursor.offset);
    let mut file = fs::File::open(newest)?;
    if file.metadata()?.len() < offset {
        offset = 0;
    }
    let mut data = Vec::new();
    file.seek(SeekFrom::Start(offset))?;
    file.take(MAX_READ).read_to_end(&mut data)?;
    let (entries, consumed) = parse_entries(&data);
    Ok((
        entries,
        Some(Cursor {
            file: name,
            offset: offset + consumed as u64,
        }),
    ))
}

/// Distinct values kept per placeholder in a summary group.
const SUMMARY_SAMPLES: usize = 5;

/// A message's first line with its variable parts (quoted text, paths, ids, numbers) replaced
/// by `*`, and those parts in order.
fn template(message: &str) -> (String, Vec<String>) {
    let mut pattern = String::new();
    let mut values = Vec::new();
    let mut rest = message.lines().next().unwrap_or_default();
    while !rest.is_empty() {
        let space = rest.len() - rest.trim_start().len();
        pattern.push_str(&rest[..space]);
        rest = &rest[space..];
        let Some(first) = rest.chars().next() else {
            break;
        };
        if (first == '"' || first == '\'')
            && let Some(end) = rest[1..].find(first)
        {
            let after = &rest[end + 2..];
            // A closing quote followed by a letter is an apostrophe ("doesn't").
            if after
                .chars()
                .next()
                .is_none_or(|next| !next.is_alphanumeric())
            {
                values.push(rest[1..=end].to_owned());
                pattern.extend([first, '*', first]);
                rest = after;
                continue;
            }
        }
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let word = &rest[..end];
        rest = &rest[end..];
        let trimmed = word.trim_end_matches(['.', ',', ';', '?', '!', ')', ':']);
        let body = trimmed.trim_start_matches('(');
        if is_value(body) {
            let start = trimmed.len() - body.len();
            pattern.push_str(&word[..start]);
            pattern.push('*');
            pattern.push_str(&word[trimmed.len()..]);
            values.push(body.to_owned());
        } else {
            pattern.push_str(word);
        }
    }
    (pattern, values)
}

/// A number, a path or a namespaced or dotted identifier.
fn is_value(word: &str) -> bool {
    let separated = word
        .char_indices()
        .any(|(index, c)| "/:.".contains(c) && index > 0 && index + 1 < word.len());
    word.chars().any(|c| c.is_ascii_digit())
        && word
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == '-')
        || separated
            && word
                .chars()
                .all(|c| c.is_alphanumeric() || "_-/:.".contains(c))
}

/// Group `(area, level, message)` entries whose messages differ only in their variable parts.
/// Groups come most frequent first: `{level, area, pattern, count, values, example}`, where
/// values lists, per `*` in pattern, how many distinct values it took and a few of them.
pub fn summarize<'a>(entries: impl IntoIterator<Item = (&'a str, &'a str, &'a str)>) -> Vec<Value> {
    struct Group<'a> {
        key: (&'a str, &'a str, String),
        count: usize,
        slots: Vec<(HashSet<String>, Vec<String>)>,
        example: &'a str,
    }
    let mut groups: Vec<Group> = Vec::new();
    let mut index: HashMap<(&str, &str, String), usize> = HashMap::new();
    for (area, level, message) in entries {
        let (pattern, values) = template(message);
        let key = (level, area, pattern);
        let at = *index.entry(key.clone()).or_insert_with(|| {
            groups.push(Group {
                key,
                count: 0,
                slots: Vec::new(),
                example: message,
            });
            groups.len() - 1
        });
        let group = &mut groups[at];
        group.count += 1;
        group
            .slots
            .resize_with(values.len().max(group.slots.len()), Default::default);
        for ((seen, samples), value) in group.slots.iter_mut().zip(values) {
            if samples.len() < SUMMARY_SAMPLES && !seen.contains(&value) {
                samples.push(value.clone());
            }
            seen.insert(value);
        }
    }
    groups.sort_by_key(|group| std::cmp::Reverse(group.count));
    groups
        .into_iter()
        .map(|group| {
            let (level, area, pattern) = group.key;
            let values: Vec<Value> = group
                .slots
                .into_iter()
                .map(|(seen, samples)| json!({ "distinct": seen.len(), "samples": samples }))
                .collect();
            json!({
                "level": level, "area": area, "pattern": pattern, "count": group.count,
                "values": values, "example": group.example,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir::TestDir;

    #[test]
    fn summaries_group_messages_that_differ_only_in_names_paths_and_numbers() {
        let material = |file: &str, name: &str| {
            format!(
                "{file} | Material \"{name}\" is referenced in file \"{file}\" but not defined."
            )
        };
        let messages = [
            material("attachables/a.json", "auulis"),
            material("attachables/b.json", "auulis"),
            material("attachables/a.json", "gkhguv"),
            "re:form.ans_head | re:form.ans_head | geometry not found?".to_owned(),
            "Entity .json 'a.json' referenced particle 'x:y' that doesn't exist".to_owned(),
            "Entity .json 'b.json' referenced particle 'x:z' that doesn't exist".to_owned(),
        ];
        let groups = summarize(messages.iter().map(|text| ("Json", "error", text.as_str())));
        assert_eq!(groups.len(), 3, "{groups:#?}");
        assert_eq!(
            groups[0]["pattern"],
            "* | Material \"*\" is referenced in file \"*\" but not defined."
        );
        assert_eq!(groups[0]["count"], 3);
        assert_eq!(groups[0]["values"][1]["distinct"], 2);
        assert_eq!(
            groups[0]["values"][1]["samples"],
            json!(["auulis", "gkhguv"])
        );
        assert_eq!(
            groups[1]["pattern"],
            "Entity .json '*' referenced particle '*' that doesn't exist"
        );
        assert_eq!(groups[2]["pattern"], "* | * | geometry not found?");
    }

    #[test]
    fn entries_parse_area_level_and_multiline_messages() {
        let data = b"12:00:01[Scripting][error]-Error: boom\n  at main.js:3\n\n12:00:02[Texture][warning]-missing a.png\r\n\r\n";
        let (entries, consumed) = parse_entries(data);
        assert_eq!(consumed, data.len());
        assert_eq!(
            entries[0],
            Entry {
                time: "12:00:01".into(),
                area: "Scripting".into(),
                level: "error".into(),
                message: "Error: boom\n  at main.js:3".into(),
            }
        );
        assert_eq!(entries[1].level, "warning");
        assert_eq!(entries[1].message, "missing a.png");
    }

    #[test]
    fn partial_tails_are_left_for_the_next_read() {
        let data = b"12:00:01[A][info]-one\n\n12:00:02[B][error]-two\n";
        let (entries, consumed) = parse_entries(data);
        assert_eq!(entries.len(), 1);
        assert_eq!(&data[consumed..], b"12:00:02[B][error]-two\n");
        let (entries, consumed) = parse_entries(b"12:00:02[B][err");
        assert!(entries.is_empty());
        assert_eq!(consumed, 0);
    }

    #[test]
    fn cursors_resume_after_complete_entries_and_restart_on_new_sessions() {
        let dir = TestDir::new("content-log");
        let first = dir.path().join("ContentLog__1.txt");
        fs::write(&first, "10:00:00[A][error]-one\n\n10:00:01[A][warn").unwrap();
        let (entries, cursor) = read(&[&first], None).unwrap();
        assert_eq!(entries.len(), 1);
        let cursor = cursor.unwrap();
        assert_eq!(Cursor::parse(&cursor.encode()).unwrap(), cursor);
        let mut file = fs::OpenOptions::new().append(true).open(&first).unwrap();
        std::io::Write::write_all(&mut file, b"ing]-two\n\n").unwrap();
        let (entries, next) = read(&[&first], Some(&cursor)).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].message, "two");
        let (entries, _) = read(&[&first], next.as_ref()).unwrap();
        assert!(entries.is_empty());

        let second = dir.path().join("ContentLog__2.txt");
        fs::write(&second, "11:00:00[B][info]-fresh\n\n").unwrap();
        let (entries, cursor) = read(&[&second, &first], next.as_ref()).unwrap();
        assert_eq!(entries[0].message, "fresh");
        assert_eq!(cursor.unwrap().file, "ContentLog__2.txt");
    }

    #[test]
    fn bad_cursors_and_odd_lines_are_handled() {
        assert!(Cursor::parse("nope").is_err());
        assert!(Cursor::parse("@4").is_err());
        assert_eq!(
            Cursor::parse("a@b@7").unwrap(),
            Cursor {
                file: "a@b".into(),
                offset: 7
            }
        );
        let (entries, _) = parse_entries(b"free text\n\n");
        assert_eq!(entries[0].level, "unknown");
        assert_eq!(entries[0].message, "free text");
    }
}
