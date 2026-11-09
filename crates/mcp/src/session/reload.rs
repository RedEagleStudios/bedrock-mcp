//! `game_reload`: bring edited packs into the game, `/reload all`, wait for the world to come
//! back and report the content log entries the reload produced as diagnostics.

use std::{
    env,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::{Value, json};

use super::Session;
use crate::{
    bds,
    clock::poll,
    content_log::{self, Cursor},
    error::{Error, Result, bail},
    game_files,
    log_diagnostics::{self, LogEntry, PackRoot, Severity},
    pack_sync::{self, Target},
    unstable::live,
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(90);
const DEFAULT_SETTLE: Duration = Duration::from_secs(3);
const MAX_SETTLE: Duration = Duration::from_secs(30);
/// How long the world gets to close after `/reload all` before the wait for it to reopen.
const CLOSE_WINDOW: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(250);
const RGL_TIMEOUT: Duration = Duration::from_secs(300);
/// Lines of rgl's output kept in the reply.
const RGL_TAIL: usize = 30;
const LIVE_BATCH: u32 = 512;
/// Live batches read after the reload, so a flooding pack cannot stall the call.
const MAX_LIVE_BATCHES: usize = 64;
/// rgl reads the game's data folder from this variable where it cannot find it itself.
const COM_MOJANG: &str = "COM_MOJANG";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReloadArgs {
    #[serde(default)]
    packs: Vec<PathBuf>,
    #[serde(default)]
    project: Option<PathBuf>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    settle_ms: Option<u64>,
}

/// Where the content log continues after the reload.
enum LogMark {
    Live(u64),
    File(Option<Cursor>),
}

/// An entry read back, owned.
struct ReadEntry {
    area: String,
    level: String,
    message: String,
}

impl Session {
    pub(crate) fn game_reload(&mut self, args: ReloadArgs) -> Result<Value> {
        let began = Instant::now();
        if args.packs.is_empty() == args.project.is_none() {
            bail!("Give either packs (pack folders) or project (an rgl project folder).");
        }
        let world = self.ensure_world()?;
        let shared = game_files::shared_dir(&self.install.prefix)?;
        let mut notes = Vec::new();
        let (targets, rgl) = match &args.project {
            Some(project) => sync_project(project, &shared, &mut notes)?,
            None => {
                let given = bds::user_packs(&args.packs)?;
                let targets = pack_sync::match_installed(&shared, &given)?;
                pack_sync::copy_all(&targets)?;
                (targets, Value::Null)
            }
        };
        let roots = roots(&targets);

        let mark = self.log_mark(&mut notes)?;
        let body = world.raw("reload all")?;
        if body["statusCode"].as_i64().unwrap_or(0) != 0 {
            bail!(
                "The game refused /reload all: {}",
                body["statusMessage"].as_str().unwrap_or("no message")
            );
        }
        // The reloaded world may have gained or lost the pack.
        world.forget_bridge();
        let timeout = args
            .timeout_ms
            .map_or(DEFAULT_TIMEOUT, Duration::from_millis);
        let back = self.wait_reopened(timeout, &mut notes)?;
        let settle = args
            .settle_ms
            .map_or(DEFAULT_SETTLE, Duration::from_millis)
            .min(MAX_SETTLE);
        if back {
            self.clock.sleep(settle);
        } else {
            notes.push(format!(
                "The world was not back within {} ms (timeout_ms); check frame_capture for a \
                 disconnect screen. The diagnostics cover what the log held by then.",
                timeout.as_millis()
            ));
        }
        let (entries, source) = self.log_since(mark, &mut notes)?;
        let problems: Vec<LogEntry> = entries
            .iter()
            .filter(|entry| Severity::from_level(&entry.level) != Severity::Info)
            .map(|entry| LogEntry {
                area: &entry.area,
                level: &entry.level,
                message: &entry.message,
            })
            .collect();
        let (diagnostics, other): (Vec<_>, Vec<_>) =
            log_diagnostics::map(&problems, &roots, "content-log")
                .into_iter()
                .partition(|diagnostic| diagnostic.in_pack);
        let other_groups = content_log::summarize(other.iter().map(|diagnostic| {
            (
                diagnostic.rule.as_str(),
                diagnostic.severity.name(),
                diagnostic.message.as_str(),
            )
        }));
        Ok(json!({
            "reloaded": true,
            "world_back": back,
            "synced": targets,
            "rgl": rgl,
            "diagnostics": diagnostics,
            "counts": log_diagnostics::counts(&diagnostics),
            "other": { "count": other.len(), "groups": other_groups },
            "entries": entries.len(),
            "source": source,
            "elapsed_ms": began.elapsed().as_millis() as u64,
            "notes": notes,
        }))
    }

    /// The live sequence number when the unstable layer's content log works, else the end
    /// of the content log file.
    fn log_mark(&mut self, notes: &mut Vec<String>) -> Result<LogMark> {
        if self.unstable_capability("content_log", notes) {
            match self.live_log_end() {
                Ok(seq) => return Ok(LogMark::Live(seq)),
                Err(error) => notes.push(format!(
                    "live content log failed ({error}); reading the content log file"
                )),
            }
        }
        Ok(LogMark::File(self.content_log_end()?))
    }

    /// Entries after `mark`, and where they came from.
    fn log_since(
        &mut self,
        mark: LogMark,
        notes: &mut Vec<String>,
    ) -> Result<(Vec<ReadEntry>, &'static str)> {
        match mark {
            LogMark::Live(mut seq) => {
                let mut read = Vec::new();
                let mut dropped = 0;
                for _ in 0..MAX_LIVE_BATCHES {
                    let batch = self.unstable.content_log(seq, LIVE_BATCH)?;
                    seq = batch.next.max(seq);
                    dropped += batch.dropped;
                    if batch.entries.is_empty() {
                        break;
                    }
                    read.extend(batch.entries.into_iter().map(|entry| ReadEntry {
                        area: live::area_name(entry.area).map_or_else(String::new, str::to_owned),
                        level: entry.level.to_string(),
                        message: entry.message,
                    }));
                }
                if dropped > 0 {
                    notes.push(format!(
                        "The live content log buffer lost {dropped} entries during the reload."
                    ));
                }
                Ok((read, "live"))
            }
            LogMark::File(cursor) => {
                let logs = game_files::content_logs(&self.install.prefix);
                if logs.is_empty() {
                    notes.push(
                        "No content log to read: the unstable layer's content_log capability is \
                         not active and the game writes no content log file. Turn on Content \
                         Log File in the game's Creator settings, or load the unstable layer."
                            .to_owned(),
                    );
                    return Ok((Vec::new(), "file"));
                }
                let mut read = Vec::new();
                let mut cursor = cursor;
                loop {
                    let (entries, next) = content_log::read(&logs, cursor.as_ref())?;
                    if entries.is_empty() {
                        break;
                    }
                    cursor = next;
                    read.extend(entries.into_iter().map(|entry| ReadEntry {
                        area: entry.area,
                        level: entry.level,
                        message: entry.message,
                    }));
                }
                Ok((read, "file"))
            }
        }
    }

    /// Wait for the world to close and open again; false when it was not back in time.
    fn wait_reopened(&mut self, timeout: Duration, notes: &mut Vec<String>) -> Result<bool> {
        let clock = Arc::clone(&self.clock);
        let started = clock.now();
        let mut unknown = Vec::new();
        let closed = poll(&*clock, CLOSE_WINDOW.min(timeout), POLL, || {
            Ok((self.in_world(&mut unknown).0 == Some(false)).then_some(()))
        })?;
        if closed.is_none() {
            notes.push(
                "No check saw the world close after /reload all; it may have reopened between \
                 checks."
                    .to_owned(),
            );
        }
        let left = timeout.saturating_sub(clock.now().duration_since(started));
        let mut last = Vec::new();
        let back = poll(&*clock, left, POLL, || {
            last.clear();
            Ok((self.in_world(&mut last).0 == Some(true)).then_some(()))
        })?;
        if back.is_none() && !last.is_empty() {
            notes.push(format!(
                "Whether the player is in a world: {}",
                last.join("; ")
            ));
        }
        Ok(back.is_some())
    }
}

/// Run `rgl run` in the project when rgl is installed, else copy its pack folders over the
/// installed ones. Returns the installed packs and rgl's report (null without rgl).
fn sync_project(
    project: &Path,
    shared: &Path,
    notes: &mut Vec<String>,
) -> Result<(Vec<Target>, Value)> {
    let folders: Vec<PathBuf> = pack_sync::project_packs(project)?
        .into_iter()
        .map(|(_, folder)| folder)
        .collect();
    let given = bds::user_packs(&folders)?;
    let mut targets = pack_sync::match_installed(shared, &given)?;
    match run_rgl(project, shared, notes)? {
        Some(report) => {
            for target in &mut targets {
                target.method = "rgl";
            }
            Ok((targets, report))
        }
        None => {
            notes.push(
                "rgl is not on PATH, so the project's filters did not run: its pack folders \
                 were copied as they are."
                    .to_owned(),
            );
            pack_sync::copy_all(&targets)?;
            Ok((targets, Value::Null))
        }
    }
}

/// `rgl run` in `project`, bounded by [`RGL_TIMEOUT`]; None when rgl is not installed.
fn run_rgl(project: &Path, shared: &Path, notes: &mut Vec<String>) -> Result<Option<Value>> {
    let began = Instant::now();
    let mut command = Command::new("rgl");
    command
        .arg("run")
        .current_dir(project)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if env::var_os(COM_MOJANG).is_none_or(|value| value.is_empty()) {
        command.env(COM_MOJANG, shared);
        notes.push(format!(
            "Ran rgl with {COM_MOJANG}={}, the game's data folder.",
            shared.display()
        ));
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => bail!("Cannot start rgl: {error}"),
    };
    let (sender, output) = mpsc::channel();
    for stream in [
        child
            .stdout
            .take()
            .map(|out| Box::new(out) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|err| Box::new(err) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let sender = sender.clone();
        thread::spawn(move || {
            let mut text = Vec::new();
            let mut stream = stream;
            let _ = stream.read_to_end(&mut text);
            let _ = sender.send(String::from_utf8_lossy(&text).into_owned());
        });
    }
    drop(sender);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if began.elapsed() >= RGL_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        thread::sleep(POLL);
    };
    // Filters may leave children holding the pipes open; take what arrived shortly after.
    let mut text = String::new();
    while let Ok(part) = output.recv_timeout(Duration::from_secs(2)) {
        text.push_str(&part);
    }
    let text = strip_colors(&text);
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let tail = lines[lines.len().saturating_sub(RGL_TAIL)..].join("\n");
    match status {
        None => bail!(
            "rgl run did not finish within {} s and was stopped. Its output:\n{tail}",
            RGL_TIMEOUT.as_secs()
        ),
        Some(status) if !status.success() => Err(Error::msg(format!(
            "rgl run failed ({status}) in {}. Its output:\n{tail}",
            project.display()
        ))),
        Some(_) => Ok(Some(json!({
            "command": "rgl run",
            "output": tail,
            "elapsed_ms": began.elapsed().as_millis() as u64,
        }))),
    }
}

/// The folders diagnostics resolve against: each source, or the installed copy when rgl
/// built it, since the game ran rgl's output.
fn roots(targets: &[Target]) -> Vec<PackRoot> {
    targets
        .iter()
        .map(|target| PackRoot {
            kind: target.kind,
            path: if target.method == "rgl" {
                target.installed.clone()
            } else {
                target.source.clone()
            },
            name: target.name.clone(),
        })
        .collect()
}

/// Text without terminal colour codes (`ESC [ ... m`), which rgl prints even into a pipe.
fn strip_colors(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for code in chars.by_ref() {
                if code.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgl_output_loses_its_colours() {
        assert_eq!(
            strip_colors("\u{1b}[34m[INFO]\u{1b}[0m Running \u{1b}[96mdefault\u{1b}[0m profile"),
            "[INFO] Running default profile"
        );
    }
}
