//! The session's use of the unstable layer. Nothing here fails a stable tool: callers get
//! `false`/`null` plus a note, and fall back to the stable behaviour.

use std::{sync::Arc, time::Duration};

use serde_json::{Value, json};

use super::{Session, hud};
use crate::{agent::try_connect, clock::poll, error::Result, unstable::live};

/// How often waits poll the unstable layer.
pub const LIVE_POLL: Duration = Duration::from_millis(50);
const LIVE_BATCH: u32 = 512;

impl Session {
    /// Use an agent that already answers in the running game, without injecting one.
    pub(super) fn connect_running_agent(&mut self) {
        if self.agent.is_none() && self.install.game_pid().is_some() {
            self.agent = try_connect(&self.install.agent_dir()).ok().flatten();
        }
    }

    /// Whether capability `name` can be used now; otherwise the reason goes to `notes`.
    pub(super) fn unstable_capability(&mut self, name: &str, notes: &mut Vec<String>) -> bool {
        let usable = self.unstable.ensure(self.agent.is_some()) && self.unstable.active(name);
        if !usable && let Some(note) = self.unstable.note() {
            notes.push(note.to_owned());
        }
        usable
    }

    /// Like [`Self::unstable_capability`], but also lets a pending capability answer:
    /// for capabilities whose first request is what confirms them.
    pub(super) fn unstable_requestable(&mut self, name: &str, notes: &mut Vec<String>) -> bool {
        let usable = self.unstable.ensure(self.agent.is_some()) && self.unstable.requestable(name);
        if !usable && let Some(note) = self.unstable.note() {
            notes.push(note.to_owned());
        }
        usable
    }

    /// The screen over gameplay, or None when the HUD has the input or nothing tells: only the
    /// live gameplay stack of the screens capability shows it.
    pub(super) fn covering_screen(&mut self) -> Option<String> {
        let mut notes = Vec::new();
        if !self.unstable_capability("screens", &mut notes) {
            return None;
        }
        let stacks = self.unstable.screens().ok()?;
        match live::gameplay_top(&stacks) {
            Some(top) if live::hud_on_top(top) => None,
            Some(top) => Some(top.to_owned()),
            None => Some("no gameplay screen (the player is not in a world)".into()),
        }
    }

    pub(crate) fn unstable_status(&mut self) -> Value {
        self.connect_running_agent();
        self.unstable.ensure(self.agent.is_some());
        let mut report = self.unstable.report();
        report["note"] = json!(self.unstable.note());
        report
    }

    /// `state.unstable`: capabilities, screen stacks and the camera when the layer is
    /// loaded, else null.
    pub(super) fn unstable_state(&mut self) -> Value {
        if !self.unstable.ensure(self.agent.is_some()) {
            return Value::Null;
        }
        let screens = if self.unstable.active("screens") {
            self.unstable.screens().map_or_else(
                |error| json!({ "error": error.to_string() }),
                |stacks| json!(stacks),
            )
        } else {
            Value::Null
        };
        let camera = if self.unstable.active("camera") {
            self.unstable.camera().map_or_else(
                |error| json!({ "error": error.to_string() }),
                |camera| hud::camera_json(&camera),
            )
        } else {
            Value::Null
        };
        json!({
            "capabilities": self.unstable.capabilities(),
            "screens": screens,
            "camera": camera,
        })
    }

    /// The live sequence number after every buffered content-log entry.
    pub(super) fn live_log_end(&mut self) -> Result<u64> {
        let mut seq = 0;
        loop {
            let batch = self.unstable.content_log(seq, LIVE_BATCH)?;
            if batch.entries.is_empty() {
                return Ok(batch.next.max(seq));
            }
            seq = batch.next;
        }
    }

    /// Live entries after `since` whose level matches, waiting up to `wait` for one.
    pub(super) fn read_live_log(
        &mut self,
        since: u64,
        levels: &[String],
        wait: Duration,
    ) -> Result<Value> {
        let mut seq = since;
        let mut dropped = 0;
        let clock = Arc::clone(&self.clock);
        let entries = poll(&*clock, wait, LIVE_POLL, || {
            let batch = self.unstable.content_log(seq, LIVE_BATCH)?;
            seq = batch.next.max(seq);
            dropped += batch.dropped;
            let matching: Vec<Value> = batch
                .entries
                .iter()
                .filter(|entry| {
                    levels.is_empty()
                        || levels
                            .iter()
                            .any(|level| live::level_matches(level, entry.level))
                })
                .map(live::entry_json)
                .collect();
            Ok((!matching.is_empty()).then_some(matching))
        })?;
        Ok(json!({
            "entries": entries.unwrap_or_default(),
            "cursor": live::live_cursor(seq),
            "dropped": dropped,
        }))
    }
}
