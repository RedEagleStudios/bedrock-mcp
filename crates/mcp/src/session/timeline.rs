//! The session timeline and recording chapters: every tool call is logged to the timeline
//! file and, while a recording runs, marks a chapter at its place in the video.

use std::time::{Instant, SystemTime};

use serde::Deserialize;
use serde_json::{Value, json};

use super::Session;
use crate::{
    error::{Error, Result},
    rpc::Content,
    timeline::{self, DEFAULT_LIMIT},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimelineArgs {
    since: Option<u64>,
    limit: Option<usize>,
}

/// A tool call in progress.
pub(super) struct CallStart {
    tool: String,
    args_summary: String,
    wall: SystemTime,
    started: Instant,
    /// Where the call starts in the running recording's video.
    recording_offset: Option<f64>,
    /// The real time the call started, for its chapter.
    chapter_at: Instant,
}

fn unix_millis(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

impl Session {
    /// Note that tool `tool` starts; a running recording stops pausing until it ends.
    pub(super) fn call_started(&mut self, tool: &str, arguments: &Value) -> CallStart {
        let recording_offset = self.recording.as_ref().and_then(|recording| {
            recording.call_started();
            recording.video_seconds()
        });
        CallStart {
            tool: tool.to_owned(),
            args_summary: timeline::summarize_args(arguments),
            wall: self.clock.wall(),
            started: self.clock.now(),
            recording_offset,
            chapter_at: Instant::now(),
        }
    }

    /// Log a finished call to the timeline and mark its chapter in the running recording.
    /// A call that answers it waits for a background run is called again; only its answer
    /// counts.
    pub(super) fn call_finished(&mut self, call: CallStart, result: &Result<Vec<Content>>) {
        if let Some(recording) = self.recording.as_ref() {
            recording.call_finished();
        }
        if matches!(result, Err(Error::Waiting)) {
            return;
        }
        let tick = result
            .as_ref()
            .ok()
            .and_then(|content| timeline::reply_tick(content));
        if let Some(recording) = self.recording.as_mut() {
            let mut title = call.tool.clone();
            if !call.args_summary.is_empty() {
                title = format!("{title} {}", call.args_summary);
            }
            if let Some(tick) = tick {
                title = format!("{title} (tick {tick})");
            }
            recording.mark_at(call.chapter_at, title);
        }
        let duration = self.clock.now().saturating_duration_since(call.started);
        let entry = json!({
            "tool": call.tool,
            "args_summary": call.args_summary,
            "started_unix_ms": unix_millis(call.wall),
            "duration_ms": duration.as_millis() as u64,
            "ok": result.is_ok(),
            "error_head": result.as_ref().err().map(|error| timeline::error_head(&error.to_string())),
            "tick": tick,
            "recording_offset_s": call.recording_offset.map(round3),
        });
        // The timeline is a record for the agent; a full disk must not fail the call.
        let _ = self.timeline.append(entry);
    }

    /// Mark a chapter titled `title` in the running recording at this moment; does nothing
    /// when nothing records. Tool calls mark their own chapters; call this for moments inside
    /// one call, such as the start of each action sequence step.
    pub(crate) fn mark(&mut self, title: &str) {
        if let Some(recording) = self.recording.as_mut() {
            recording.mark_at(Instant::now(), title.to_owned());
        }
    }

    /// Background work started (`true`) or ended: a recording does not pause while it runs.
    pub(super) fn background_busy(&self, busy: bool) {
        if let Some(recording) = self.recording.as_ref() {
            if busy {
                recording.call_started();
            } else {
                recording.call_finished();
            }
        }
    }

    pub(crate) fn timeline_read(&mut self, args: TimelineArgs) -> Result<Value> {
        self.timeline
            .read(args.since, args.limit.unwrap_or(DEFAULT_LIMIT))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{
        clock::fake::FakeClock,
        install::Install,
        rpc::ToolServer,
        testdir::TestDir,
        unstable::fake::{FakeHost, Script},
    };

    #[test]
    fn every_call_is_logged_with_its_outcome() {
        let dir = TestDir::new("session-timeline");
        let install = Install {
            profile: dir.path().join("profile"),
            prefix: dir.path().join("pfx"),
        };
        let host = Box::new(FakeHost(Arc::new(std::sync::Mutex::new(Script::working()))));
        let mut session = Session::with_unstable_host(install, Arc::new(FakeClock::new()), host);
        assert!(
            session
                .call("no_such_tool", json!({"target": "zombie"}))
                .is_err()
        );
        let Content::Text(text) = &session.call("timeline_read", json!({})).unwrap()[0] else {
            panic!("a text reply");
        };
        let reply: Value = serde_json::from_str(text).unwrap();
        let entries = reply["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry["seq"], 1);
        assert_eq!(entry["tool"], "no_such_tool");
        assert_eq!(entry["args_summary"], "zombie");
        assert_eq!(entry["ok"], false);
        assert_eq!(entry["error_head"], "Unknown tool: no_such_tool");
        assert_eq!(entry["recording_offset_s"], Value::Null);
        assert!(entry["started_unix_ms"].is_u64() && entry["duration_ms"].is_u64());
        // The read itself is logged after it answered.
        let since = session
            .timeline_read(TimelineArgs {
                since: Some(1),
                limit: None,
            })
            .unwrap();
        assert_eq!(since["entries"][0]["tool"], "timeline_read");
        assert_eq!(since["entries"][0]["ok"], true);
    }
}
