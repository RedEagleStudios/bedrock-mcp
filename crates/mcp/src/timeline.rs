//! The session timeline: one JSON line per tool call in `<agent_dir>/timeline/<start>.jsonl`,
//! so an agent can see afterwards what it did, how long each call took and where it falls in a
//! recording.

use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};

use serde_json::{Value, json};

use crate::{
    error::{Error, Result},
    rpc::Content,
};

/// Longest `args_summary`, in characters.
const SUMMARY_CHARS: usize = 80;
/// Longest string argument shown in a summary, in characters.
const VALUE_CHARS: usize = 32;
/// Longest `error_head`, in characters.
const ERROR_CHARS: usize = 160;
/// Entries `timeline_read` returns without a limit.
pub const DEFAULT_LIMIT: usize = 50;

pub struct Timeline {
    path: PathBuf,
    file: Option<File>,
    /// The `seq` of the next entry.
    next: u64,
}

impl Timeline {
    /// The timeline of a session that started at `started`, under `agent_dir`. The file is
    /// created with the first entry.
    pub fn new(agent_dir: &Path, started: SystemTime) -> Self {
        let millis = started
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        Self {
            path: agent_dir.join("timeline").join(format!("{millis}.jsonl")),
            file: None,
            next: 1,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append `entry` (an object) as the next line, numbered by a `seq` field.
    pub fn append(&mut self, mut entry: Value) -> Result<()> {
        if self.file.is_none() {
            if let Some(parent) = self.path.parent() {
                fs::create_dir_all(parent)?;
            }
            self.file = Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)?,
            );
        }
        entry["seq"] = self.next.into();
        let mut line = serde_json::to_vec(&entry)?;
        line.push(b'\n');
        self.file.as_mut().expect("opened above").write_all(&line)?;
        self.next += 1;
        Ok(())
    }

    /// The last `limit` entries with a `seq` above `since`, oldest first.
    pub fn read(&self, since: Option<u64>, limit: usize) -> Result<Value> {
        let since = since.unwrap_or(0);
        let mut entries = Vec::new();
        match File::open(&self.path) {
            Ok(file) => {
                for line in BufReader::new(file).lines() {
                    let Ok(entry) = serde_json::from_str::<Value>(&line?) else {
                        continue;
                    };
                    if entry["seq"].as_u64().is_some_and(|seq| seq > since) {
                        entries.push(entry);
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::from(error)),
        }
        let after = entries.len();
        let entries = entries.split_off(after.saturating_sub(limit));
        Ok(json!({
            "path": self.path,
            "entries": entries,
            "skipped": after - entries.len(),
            "next_since": self.next - 1,
        }))
    }
}

/// `text` cut to `max` characters, with an ellipsis when cut.
fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text.to_owned(),
        Some((end, _)) => format!("{}…", &text[..end]),
    }
}

/// A short form of tool arguments: string values bare and first (they name targets and
/// commands), then other scalars as `key=value` and arrays and objects by their key and size.
pub fn summarize_args(arguments: &Value) -> String {
    let Value::Object(map) = arguments else {
        return String::new();
    };
    let strings = map
        .values()
        .filter_map(Value::as_str)
        .map(|text| clip(text, VALUE_CHARS));
    let others = map.iter().filter_map(|(key, value)| match value {
        Value::String(_) => None,
        Value::Array(items) => Some(format!("{key}[{}]", items.len())),
        Value::Object(fields) => Some(format!("{key}{{{}}}", fields.len())),
        other => Some(format!("{key}={other}")),
    });
    let parts: Vec<String> = strings.chain(others).collect();
    clip(&parts.join(" "), SUMMARY_CHARS)
}

/// The first line of an error message, clipped.
pub fn error_head(message: &str) -> String {
    clip(
        message.lines().next().unwrap_or_default().trim(),
        ERROR_CHARS,
    )
}

/// The game tick a JSON reply reports: an action outcome's `tick` (the confirming event's),
/// else the tick its evidence says the action ran on.
pub fn reply_tick(content: &[Content]) -> Option<u64> {
    content.iter().find_map(|item| match item {
        Content::Text(text) if text.contains("tick\"") => {
            let reply: Value = serde_json::from_str(text).ok()?;
            reply["tick"]
                .as_u64()
                .or_else(|| reply["evidence"]["acted_tick"].as_u64())
        }
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir::TestDir;

    #[test]
    fn entries_are_numbered_lines_read_back_after_a_cursor() {
        let dir = TestDir::new("timeline");
        let start = SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(1234);
        let mut timeline = Timeline::new(dir.path(), start);
        assert_eq!(timeline.path(), dir.path().join("timeline/1234.jsonl"));
        // Nothing written yet reads as empty.
        assert_eq!(timeline.read(None, 10).unwrap()["entries"], json!([]));
        for tool in ["game_status", "attack", "video_stop"] {
            timeline
                .append(json!({ "tool": tool, "ok": true }))
                .unwrap();
        }
        let text = fs::read_to_string(timeline.path()).unwrap();
        assert_eq!(
            text.lines().next().unwrap(),
            r#"{"ok":true,"seq":1,"tool":"game_status"}"#
        );
        let all = timeline.read(None, 10).unwrap();
        assert_eq!(all["entries"].as_array().unwrap().len(), 3);
        assert_eq!(all["next_since"], 3);
        let after = timeline.read(Some(1), 1).unwrap();
        assert_eq!(
            after["entries"],
            json!([{"seq": 3, "tool": "video_stop", "ok": true}])
        );
        assert_eq!(after["skipped"], 1);
    }

    #[test]
    fn arguments_summarize_to_one_short_line() {
        let summary = summarize_args(&json!({
            "target": "-326417514479",
            "approach": true,
            "steps": [1, 2, 3],
            "region": {"x": 1, "y": 2},
            "radius": 4.5,
        }));
        assert_eq!(
            summary,
            "-326417514479 approach=true radius=4.5 region{2} steps[3]"
        );
        let long = summarize_args(&json!({ "command": "x".repeat(100) }));
        assert_eq!(long.chars().count(), VALUE_CHARS + 1);
        assert_eq!(summarize_args(&json!(null)), "");
        assert_eq!(error_head("first line\nsecond"), "first line");
    }

    #[test]
    fn the_tick_comes_from_an_action_outcome() {
        let reply = |value: Value| vec![Content::json(&value)];
        assert_eq!(reply_tick(&reply(json!({"tick": 445260}))), Some(445260));
        let unconfirmed = json!({"confirmed": false, "evidence": {"acted_tick": 504024}});
        assert_eq!(reply_tick(&reply(unconfirmed)), Some(504024));
        assert_eq!(reply_tick(&reply(json!({"result": {"tick": 5}}))), None);
        assert_eq!(reply_tick(&[Content::Png(Vec::new())]), None);
    }
}
