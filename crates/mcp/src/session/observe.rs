//! Observing: state, waits, the content log, screenshots and recordings.

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Session, hud::Death, ui::TreeClick, unstable::LIVE_POLL};
use crate::{
    agent::Agent,
    clock::poll,
    content_log::{self, Cursor, Entry},
    error::{Error, Result, bail},
    frame_file::FrameFile,
    game_files,
    platform::{self, Window},
    recorder::Recording,
    rpc::Content,
    screen_text::{self, Region},
    still,
    unstable::live::{self, LogCursor},
};

/// Highest recording rate; the game rarely presents faster.
const MAX_RECORD_FPS: u32 = 240;

const DEFAULT_WAIT: Duration = Duration::from_secs(60);
const WAIT_POLL: Duration = Duration::from_millis(250);
const DEFAULT_INLINE_WIDTH: u32 = 1280;
/// The code ignores text in this top share of the window, where the logo sits, and expects
/// menu buttons below it.
const MENU_TOP_SHARE: f64 = 0.4;
/// Lines of text below the top share that the code takes as the menu being up.
const MENU_LINES: usize = 2;
/// Wheel notches per scroll while `ui_click` searches a list, and the wait for it to settle.
const CLICK_SCROLL_NOTCHES: f64 = 2.0;
const CLICK_SCROLL_SETTLE: Duration = Duration::from_millis(500);
const DEFAULT_CLICK_SCROLLS: u32 = 10;

/// Replace a content log reply's `entries` with `groups` of entries whose messages differ only
/// in their variable parts, and `total`, the number of entries.
fn summarize_reply(reply: &mut Value, fields: impl Fn(&Value) -> (&Value, &Value)) {
    let entries = reply["entries"].take();
    let entries = entries.as_array().map(Vec::as_slice).unwrap_or_default();
    let groups = content_log::summarize(entries.iter().map(|entry| {
        let (area, level) = fields(entry);
        (
            area.as_str().unwrap_or_default(),
            level.as_str().unwrap_or_default(),
            entry["message"].as_str().unwrap_or_default(),
        )
    }));
    let object = reply
        .as_object_mut()
        .expect("content log replies are objects");
    object.remove("entries");
    object.insert("total".into(), json!(entries.len()));
    object.insert("groups".into(), json!(groups));
}

/// A screen that stops play until someone acts on it: the death screen or a disconnect
/// screen (by name in the live stacks). `live_death` is the unstable
/// layer's word on death (the camera's damage parameters or a JSON UI death screen):
/// Some(None) when it tells the player is alive, which then wins over the Bridge pack's
/// health 0 (a script read that lags the game). None leaves it to the Bridge pack.
fn attention(world: Option<&Value>, unstable: &Value, live_death: Option<Option<Death>>) -> Value {
    match live_death {
        Some(Some(death)) => {
            let detail = match &death.message {
                Some(message) => format!("The player is dead: {message}."),
                None => "The player is dead and the death screen is up.".to_owned(),
            };
            return json!({
                "kind": "death",
                "detail": detail,
                "respawn_available": death.respawn,
                "source": "unstable",
                "next": "ui_click {\"text\": \"Respawn\"}",
            });
        }
        Some(None) => {}
        None => {
            if world.and_then(|world| world["player"]["health"]["current"].as_f64()) == Some(0.0) {
                return json!({
                    "kind": "death",
                    "detail": "The player is dead and the death screen is up.",
                    "source": "bridge",
                    "next": "ui_click {\"text\": \"Respawn\"}",
                });
            }
        }
    }
    if let Some(screen) = unstable["screens"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|stack| stack["screens"].as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .find(|screen| screen.contains("disconnect"))
    {
        return json!({
            "kind": "disconnected",
            "detail": format!("The {screen} screen is up: the world closed or failed to open."),
            "next": "frame_capture with ocr to read the reason, then ui_click its button and world_open to go back",
        });
    }
    Value::Null
}

/// Set in the game's environment, the unstable layer ignores RTTI and finds classes by string
/// anchors only.
const ANCHORS_ONLY: &str = "BEDROCK_MCP_UNSTABLE_ANCHORS_ONLY";

/// Whether the game process was started with `name=1`. Only Linux shows another process's
/// environment (`/proc/<pid>/environ`); elsewhere this is false.
fn game_env_set(pid: u32, name: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        let wanted = format!("{name}=1");
        std::fs::read(format!("/proc/{pid}/environ")).is_ok_and(|raw| {
            raw.split(|byte| *byte == 0)
                .any(|item| item == wanted.as_bytes())
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (pid, name);
        false
    }
}

/// What `game_status` reads to warn about unusual modes.
struct WarningFacts<'a> {
    anchors_only: bool,
    unstable_disabled: bool,
    /// Why the unstable layer failed to load for this process.
    unstable_failed: Option<String>,
    /// `state.unstable`.
    unstable: &'a Value,
    attention: &'a Value,
    /// Whether the player is in a world; None when nothing tells.
    in_world: Option<bool>,
    connected: bool,
    /// The screen over gameplay while in a world.
    covering: Option<String>,
    /// DLLs in the game from neither the game, Wine or Windows, nor this server.
    other_mods: Vec<String>,
}

/// Unusual modes and states that change how tools behave, each `{kind, detail}`.
fn warnings(facts: &WarningFacts) -> Vec<Value> {
    let mut warnings = Vec::new();
    let mut warn =
        |kind: &str, detail: String| warnings.push(json!({ "kind": kind, "detail": detail }));
    if !facts.attention.is_null() {
        warn(
            facts.attention["kind"].as_str().unwrap_or("attention"),
            facts.attention["detail"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        );
    }
    if facts.anchors_only {
        warn(
            "unstable_anchors_only",
            format!(
                "The game was started with {ANCHORS_ONLY}=1: the unstable layer ignores RTTI and \
                 finds classes by string anchors only, so capabilities may differ from a normal \
                 launch. Restart the game without it for normal behaviour."
            ),
        );
    }
    if facts.unstable_disabled {
        warn(
            "unstable_disabled",
            format!(
                "{}=0 keeps the unstable layer from loading: live screens, HUD text, camera and \
                 content log reads fall back to slower stable paths or fail.",
                crate::unstable::SETTING
            ),
        );
    }
    if let Some(reason) = &facts.unstable_failed {
        warn(
            "unstable_failed",
            format!("The unstable layer failed for this game process: {reason}"),
        );
    }
    let broken: Vec<String> = facts.unstable["capabilities"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, state)| *state == "failed" || *state == "unavailable")
        .map(|(name, state)| format!("{name} ({})", state.as_str().unwrap_or_default()))
        .collect();
    if !broken.is_empty() {
        warn(
            "unstable_capabilities",
            format!(
                "Unstable capabilities not working: {}. Tools that use them fall back or fail; \
                 unstable_status says why.",
                broken.join(", ")
            ),
        );
    }
    if facts.in_world == Some(true) && !facts.connected {
        warn(
            "websocket_disconnected",
            "The player is in a world but the WebSocket is not connected: commands and the \
             Bridge pack need automation_connect."
                .to_owned(),
        );
    }
    if let Some(screen) = &facts.covering
        && facts.attention.is_null()
    {
        warn(
            "screen_open",
            format!(
                "{screen} covers gameplay: movement and item use do not reach the game until it closes."
            ),
        );
    }
    if !facts.other_mods.is_empty() {
        warn(
            "other_mods",
            format!(
                "Other DLLs are loaded in the game: {}. Mods may hook the same functions as the \
                 agent and the unstable layer; when input, capture or reads misbehave, try \
                 without them.",
                facts.other_mods.join(", ")
            ),
        );
    }
    warnings
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitArgs {
    condition: Value,
    timeout_ms: Option<u64>,
}

/// Where `content_log_read` reads entries.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LogSource {
    /// Live when the unstable layer's `content_log` capability is active, else the file.
    #[default]
    Auto,
    Live,
    File,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentLogArgs {
    since: Option<String>,
    #[serde(default)]
    levels: Vec<String>,
    #[serde(default)]
    summary: bool,
    #[serde(default)]
    wait_ms: u64,
    #[serde(default)]
    source: LogSource,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScreenshotArgs {
    path: Option<PathBuf>,
    inline: Option<bool>,
    max_width: Option<u32>,
    #[serde(default)]
    ocr: bool,
    region: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClickTextArgs {
    text: String,
    #[serde(default)]
    exact: bool,
    #[serde(default)]
    scroll: bool,
    max_scrolls: Option<u32>,
    #[serde(default)]
    right: bool,
    region: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordArgs {
    path: Option<PathBuf>,
    fps: Option<u32>,
    codec: Option<String>,
    audio: Option<bool>,
    game_fps: Option<u32>,
    pause_on_idle_ms: Option<u64>,
}

/// A `wait_until` condition.
#[derive(Clone, Debug, PartialEq)]
pub enum Condition {
    InWorld,
    Screen(String),
    ChunksLoaded(u32),
    Entity {
        matches: String,
        radius: f64,
    },
    CameraFinished,
    ContentLog {
        level: Option<String>,
        contains: Option<String>,
    },
    /// A line of on-screen text containing `contains` (ignoring case), read by OCR.
    Text {
        contains: String,
        region: Option<Region>,
    },
    /// An event from the merged event stream that arrives after the wait starts.
    Event {
        name: Option<String>,
        contains: Option<String>,
    },
}

impl Condition {
    pub fn parse(value: &Value) -> Result<Self> {
        let (name, body) = match value {
            Value::String(name) => (name.as_str(), &Value::Null),
            Value::Object(map) if map.len() == 1 => {
                let (name, body) = map.iter().next().expect("one entry");
                (name.as_str(), body)
            }
            _ => bail!("condition must be one of the documented objects or a unit condition name."),
        };
        let text = |key: &str| body.get(key).and_then(Value::as_str).map(str::to_owned);
        Ok(match name {
            "in_world" => Self::InWorld,
            "camera_finished" => Self::CameraFinished,
            "screen" => Self::Screen(
                text("name").ok_or_else(|| Error::msg("screen needs {\"name\": ...}."))?,
            ),
            "chunks_loaded" => Self::ChunksLoaded(
                body.get("radius")
                    .and_then(Value::as_u64)
                    .and_then(|r| u32::try_from(r).ok())
                    .unwrap_or(2),
            ),
            "entity" => Self::Entity {
                matches: text("matches")
                    .ok_or_else(|| Error::msg("entity needs {\"matches\": type id}."))?,
                radius: body.get("radius").and_then(Value::as_f64).unwrap_or(32.0),
            },
            "content_log" => Self::ContentLog {
                level: text("level").map(|level| level.to_lowercase()),
                contains: text("contains"),
            },
            "text" => Self::Text {
                contains: text("contains")
                    .filter(|text| !text.trim().is_empty())
                    .ok_or_else(|| Error::msg("text needs {\"contains\": non-empty text}."))?,
                region: body.get("region").map(Region::parse).transpose()?,
            },
            "event" => Self::Event {
                name: text("name"),
                contains: text("contains"),
            },
            other => bail!("Unknown condition {other:?}."),
        })
    }
}

fn unix_millis(time: SystemTime) -> u128 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

impl Session {
    pub(crate) fn state(&mut self) -> Value {
        let pid = self.install.game_pid();
        self.connect_running_agent();
        let status = match self.agent.as_mut().map(|agent| agent.status()) {
            Some(Ok(status)) => Some(Ok(status)),
            Some(Err(error)) => {
                self.agent = None;
                self.controller.forget();
                Some(Err(error))
            }
            None => None,
        };
        let window = pid.map(|pid| {
            let desktop = platform::desktop();
            let window = desktop.window(pid).and_then(|window| match (window, &status) {
                (Some(window), _) => Ok(window),
                (None, Some(Ok(status))) => Ok(Window::from_agent(status, desktop.name())),
                (None, _) => Err(Error::msg(format!(
                    "On this desktop ({}) the window size comes from the agent, which is not attached.",
                    desktop.name()
                ))),
            });
            window.map_or_else(
                |error| json!({ "error": error.to_string() }),
                |window| window.to_json(),
            )
        });
        let agent = match status {
            Some(Ok(status)) => json!(status),
            Some(Err(error)) => json!({ "error": error.to_string() }),
            None => json!(null),
        };
        let content_log_errors = self.count_content_errors();
        let (websocket, world) = match self.world() {
            Ok(world) => {
                let port = world.socket().port();
                let snapshot = world.connected().then(|| {
                    world
                        .snapshot()
                        .unwrap_or_else(|error| json!({ "error": error.to_string() }))
                });
                let websocket = json!({
                    "port": port,
                    "connected": world.connected(),
                    "connections": world.socket().connections(),
                    "connect_command": format!("/connect 127.0.0.1:{port}"),
                });
                (websocket, snapshot)
            }
            Err(error) => (json!({ "error": error.to_string() }), None),
        };
        let unstable = self.unstable_state();
        let live_death = self.death(&mut Vec::new());
        let attention = attention(world.as_ref(), &unstable, live_death);
        let connected = websocket["connected"] == true;
        let mut in_world_notes = Vec::new();
        let (in_world, in_world_source) = if pid.is_some() {
            self.in_world(&mut in_world_notes)
        } else {
            (Some(false), Some("not_running"))
        };
        let unstable_failed = (pid.is_some() && unstable.is_null())
            .then(|| self.unstable.report())
            .filter(|report| report["load"]["state"] == "failed")
            .map(|report| {
                report["load"]["detail"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            });
        let warnings = warnings(&WarningFacts {
            anchors_only: pid.is_some_and(|pid| game_env_set(pid, ANCHORS_ONLY)),
            unstable_disabled: std::env::var(crate::unstable::SETTING)
                .is_ok_and(|value| value.trim() == "0"),
            unstable_failed,
            unstable: &unstable,
            attention: &attention,
            in_world,
            connected,
            covering: if in_world == Some(true) {
                self.covering_screen()
            } else {
                None
            },
            other_mods: pid
                .map(|pid| self.install.other_mods(pid))
                .unwrap_or_default(),
        });
        let world = if in_world == Some(true) {
            self.with_native_player(world)
        } else {
            world
        };
        let clock = &*self.clock;
        json!({
            "running": pid.is_some(),
            "pid": pid,
            "crash_debugger": self.install.crash_handler_running(),
            "agent": agent,
            "window": window,
            "in_world": {
                "value": in_world,
                "source": in_world_source,
                "notes": if in_world.is_none() { in_world_notes } else { Vec::new() },
            },
            "content_log_errors": content_log_errors,
            "attention": attention,
            "warnings": warnings,
            "websocket": websocket,
            "world": world,
            "camera": self.camera.as_ref().map(|camera| camera.status()),
            "recording": self.recording.as_mut().map(|recording| recording.status(clock)),
            "timeline": self.timeline.path(),
            "held": self.controller.held(),
            "input_lock": self.lock.report(),
            "unstable": unstable,
        })
    }

    /// Errors in the current session's content log, counted incrementally.
    fn count_content_errors(&mut self) -> u64 {
        let logs = game_files::content_logs(&self.install.prefix);
        let Ok((entries, cursor)) = content_log::read(&logs, self.content_cursor.as_ref()) else {
            return self.content_errors;
        };
        if cursor.as_ref().map(|c| &c.file) != self.content_cursor.as_ref().map(|c| &c.file) {
            self.content_errors = 0;
        }
        self.content_errors += entries
            .iter()
            .filter(|entry| entry.level == "error")
            .count() as u64;
        self.content_cursor = cursor;
        self.content_errors
    }

    /// The cursor after every complete entry written so far.
    pub(super) fn content_log_end(&self) -> Result<Option<Cursor>> {
        let logs = game_files::content_logs(&self.install.prefix);
        let mut cursor = None;
        loop {
            let (entries, next) = content_log::read(&logs, cursor.as_ref())?;
            if entries.is_empty() {
                return Ok(next);
            }
            cursor = next;
        }
    }

    pub(crate) fn wait_for(&mut self, args: WaitArgs) -> Result<Value> {
        let condition = Condition::parse(&args.condition)?;
        let timeout = args.timeout_ms.map_or(DEFAULT_WAIT, Duration::from_millis);
        if let Condition::Event { name, contains } = &condition {
            return self.wait_for_event(
                name.as_deref(),
                contains.as_deref(),
                timeout,
                &args.condition,
            );
        }
        let mut notes = Vec::new();
        if matches!(condition, Condition::Screen(_))
            && !self.unstable_capability("screens", &mut notes)
        {
            bail!(
                "Screen conditions read the unstable layer's screens capability, which is not \
                 active ({}). Wait for text on screen with {{\"text\": {{\"contains\": ...}}}} \
                 instead.",
                notes.join("; ")
            );
        }
        if matches!(condition, Condition::InWorld) && self.in_world(&mut notes).1.is_none() {
            bail!(
                "Nothing can tell whether the player is in a world: the unstable layer's \
                 screens capability is not active ({}) and the WebSocket is not connected. \
                 Wait for text on screen with {{\"text\": {{\"contains\": ...}}}}, or check \
                 frame_capture.",
                notes.join("; ")
            );
        }
        let live_screens = matches!(condition, Condition::Screen(_));
        let mut content_cursor = match condition {
            Condition::ContentLog { .. } => self.content_log_end()?,
            _ => None,
        };
        let mut live_seq = None;
        if matches!(condition, Condition::ContentLog { .. })
            && self.unstable_capability("content_log", &mut notes)
        {
            match self.live_log_end() {
                Ok(seq) => live_seq = Some(seq),
                Err(error) => notes.push(format!(
                    "live content log failed ({error}); reading the file"
                )),
            }
        }
        let world = match condition {
            Condition::ChunksLoaded(_) | Condition::Entity { .. } => Some(self.ensure_world()?),
            _ => None,
        };
        let interval = if live_screens || live_seq.is_some() {
            LIVE_POLL
        } else {
            WAIT_POLL
        };
        let fonts = match condition {
            Condition::Text { .. } => Some(screen_text::fonts(&self.game_dir()?)?),
            _ => None,
        };
        let text_capture = self
            .install
            .agent_dir()
            .join("screenshots")
            .join(format!(".wait-text-{}.png", std::process::id()));
        let mut last_text: Vec<String> = Vec::new();
        let started = self.clock.now();
        let clock = std::sync::Arc::clone(&self.clock);
        let found = poll(&*clock, timeout, interval, || -> Result<Option<Value>> {
            Ok(match &condition {
                Condition::InWorld => {
                    self.running_pid()?;
                    let (in_world, source) = self.in_world(&mut Vec::new());
                    (in_world == Some(true)).then(|| json!({ "in_world": true, "source": source }))
                }
                Condition::Screen(name) => {
                    let stacks = self.unstable.screens()?;
                    live::top_screen_matching(&stacks, name)
                        .map(|top| json!({ "screen": top, "source": "unstable" }))
                }
                Condition::ChunksLoaded(radius) => {
                    let world = world.as_ref().expect("connected above");
                    world
                        .chunks_loaded(*radius)?
                        .then(|| json!({ "radius": radius }))
                }
                Condition::Entity { matches, radius } => {
                    let world = world.as_ref().expect("connected above");
                    world
                        .entity_near(matches, *radius)?
                        .then(|| json!({ "entity": matches }))
                }
                Condition::CameraFinished => self
                    .camera
                    .as_ref()
                    .is_none_or(|camera| camera.finished())
                    .then(|| json!({ "camera_finished": true })),
                Condition::ContentLog { level, contains } => {
                    let wanted = |message: &str| {
                        contains
                            .as_ref()
                            .is_none_or(|text| message.contains(text.as_str()))
                    };
                    if let Some(seq) = live_seq {
                        match self.unstable.content_log(seq, 512) {
                            Ok(batch) => {
                                live_seq = Some(batch.next.max(seq));
                                return Ok(batch
                                    .entries
                                    .iter()
                                    .find(|entry| {
                                        level.as_ref().is_none_or(|level| live::level_matches(level, entry.level))
                                            && wanted(&entry.message)
                                    })
                                    .map(|entry| json!({ "entry": live::entry_json(entry), "source": "live" })));
                            }
                            Err(error) => {
                                live_seq = None;
                                notes.push(format!(
                                    "live content log failed ({error}); reading the file"
                                ));
                            }
                        }
                    }
                    let logs = game_files::content_logs(&self.install.prefix);
                    let (entries, next) = content_log::read(&logs, content_cursor.as_ref())?;
                    content_cursor = next;
                    entries
                        .into_iter()
                        .find(|entry: &Entry| {
                            level.as_ref().is_none_or(|level| entry.level == *level)
                                && wanted(&entry.message)
                        })
                        .map(|entry| json!({ "entry": entry, "source": "file" }))
                }
                Condition::Text { contains, region } => {
                    let pid = self.running_pid()?;
                    let window = self.game_window(pid)?;
                    self.capture(&window, &text_capture, None)?;
                    let image = ocr::Image::open(&text_capture)
                        .map_err(|error| Error::msg(error.to_string()))?;
                    let fonts = fonts.as_deref().expect("loaded above");
                    let lines = screen_text::read(&image, window.width as f64, *region, fonts)?;
                    let found = screen_text::find(&lines, contains)
                        .map(|line| json!({ "line": line.to_json(), "source": "ocr" }));
                    last_text = lines.into_iter().map(|line| line.text).collect();
                    found
                }
                Condition::Event { .. } => unreachable!("event conditions return early"),
            })
        });
        if matches!(condition, Condition::Text { .. }) {
            let _ = std::fs::remove_file(&text_capture);
        }
        let found = found?;
        let waited_ms = clock.now().duration_since(started).as_millis();
        match found {
            Some(detail) => Ok(json!({
                "satisfied": true,
                "waited_ms": waited_ms,
                "detail": detail,
                "notes": notes,
            })),
            None if matches!(condition, Condition::Text { .. }) => bail!(
                "Timed out after {} ms waiting for {}; the last capture read: {:?}",
                timeout.as_millis(),
                args.condition,
                last_text
            ),
            None => bail!(
                "Timed out after {} ms waiting for {}{}",
                timeout.as_millis(),
                args.condition,
                if notes.is_empty() {
                    String::new()
                } else {
                    format!("; notes: {}", notes.join("; "))
                }
            ),
        }
    }

    /// `wait_until` with an event condition: the first event after the call from the merged
    /// stream whose name equals `name` (ignoring case) and whose body's JSON text contains
    /// `contains`.
    fn wait_for_event(
        &mut self,
        name: Option<&str>,
        contains: Option<&str>,
        timeout: Duration,
        condition: &Value,
    ) -> Result<Value> {
        let started = std::time::Instant::now();
        let after = self.event_cursor()?;
        let waited = self.wait_event(&after, timeout, |event| {
            name.is_none_or(|name| name.eq_ignore_ascii_case(&event.name))
                && contains.is_none_or(|text| event.body_contains(text))
        })?;
        match waited.event {
            Some(event) => Ok(json!({
                "satisfied": true,
                "waited_ms": started.elapsed().as_millis(),
                "detail": { "event": event.to_json(), "source": event.source.as_str() },
                "notes": waited.notes,
            })),
            None => bail!(
                "Timed out after {} ms waiting for {condition}.{}",
                timeout.as_millis(),
                waited
                    .notes
                    .iter()
                    .map(|note| format!(" {note}"))
                    .collect::<String>()
            ),
        }
    }

    pub(crate) fn content_log(&mut self, args: ContentLogArgs) -> Result<Value> {
        let since = args.since.as_deref().map(LogCursor::parse).transpose()?;
        let levels: Vec<String> = args
            .levels
            .iter()
            .map(|level| level.to_lowercase())
            .collect();
        let wait = Duration::from_millis(args.wait_ms);
        let mut notes = Vec::new();
        let mut cursor = match (args.source, since) {
            (LogSource::File, Some(LogCursor::Live(_))) => bail!(
                "A live cursor cannot continue the content log file; use source auto or live, or omit since."
            ),
            (source, Some(LogCursor::File(cursor))) => {
                if source == LogSource::Live {
                    notes.push("a file cursor continues the content log file".to_owned());
                }
                Some(cursor)
            }
            (LogSource::File, None) => None,
            (_, live_since) => {
                let start = match live_since {
                    Some(LogCursor::Live(seq)) => seq,
                    _ => 0,
                };
                if self.unstable_capability("content_log", &mut notes) {
                    match self.read_live_log(start, &levels, wait) {
                        Ok(mut reply) => {
                            reply["source"] = json!("live");
                            reply["notes"] = json!(notes);
                            if args.summary {
                                summarize_reply(&mut reply, |entry| {
                                    (&entry["area_name"], &entry["level_name"])
                                });
                            }
                            return Ok(reply);
                        }
                        Err(error) => notes.push(format!(
                            "live content log failed ({error}); read the file instead"
                        )),
                    }
                }
                if live_since.is_some() {
                    notes.push(
                        "the live cursor does not apply to the file, so it is read from the start"
                            .to_owned(),
                    );
                }
                None
            }
        };
        let prefix = self.install.prefix.clone();
        let entries = poll(&*self.clock, wait, WAIT_POLL, || {
            let logs = game_files::content_logs(&prefix);
            let (entries, next) = content_log::read(&logs, cursor.as_ref())?;
            cursor = next;
            let matching: Vec<Entry> = entries
                .into_iter()
                .filter(|entry| levels.is_empty() || levels.contains(&entry.level))
                .collect();
            Ok((!matching.is_empty()).then_some(matching))
        })?;
        let mut reply = json!({
            "entries": entries.unwrap_or_default(),
            "cursor": cursor.as_ref().map(Cursor::encode),
            "source": "file",
            "notes": notes,
        });
        if args.summary {
            summarize_reply(&mut reply, |entry| (&entry["area"], &entry["level"]));
        }
        Ok(reply)
    }

    pub(crate) fn screenshot(&mut self, args: ScreenshotArgs) -> Result<Vec<Content>> {
        let region = args.region.as_ref().map(Region::parse).transpose()?;
        if region.is_some() && !args.ocr {
            bail!("region limits OCR; pass ocr: true with it.");
        }
        let pid = self.running_pid()?;
        let window = self.game_window(pid)?;
        let path = args.path.unwrap_or_else(|| {
            self.install
                .agent_dir()
                .join("screenshots")
                .join(format!("screenshot-{}.png", unix_millis(self.clock.wall())))
        });
        let max_width = args.max_width.unwrap_or(DEFAULT_INLINE_WIDTH);
        let inline = args.inline.unwrap_or(true).then_some(max_width);
        let png = self.capture(&window, &path, inline)?;
        let mut info = json!({
            "path": path,
            "size": window.physical(),
            "logical_size": [window.width, window.height],
            "inline_scale": inline.map(|width| platform::inline_scale(&window, width) / window.scale),
        });
        if args.ocr {
            // The capture stands without OCR; a failure to read it is reported beside it.
            info["ocr"] = self
                .game_dir()
                .and_then(|dir| screen_text::fonts(&dir))
                .and_then(|fonts| screen_text::read_png(&path, window.width as f64, region, &fonts))
                .unwrap_or_else(|error| json!({ "error": error.to_string() }));
        }
        let mut content = vec![Content::json(&info)];
        content.extend(png.map(Content::Png));
        Ok(content)
    }

    /// The game installation directory: where the running executable lives.
    fn game_dir(&self) -> Result<PathBuf> {
        let pid = self.running_pid()?;
        self.install
            .game_exe(pid)
            .and_then(|exe| exe.parent().map(PathBuf::from))
            .ok_or_else(|| Error::msg(format!("Cannot find the game directory of process {pid}.")))
    }

    pub(crate) fn record_start(&mut self, args: RecordArgs) -> Result<Value> {
        if self.recording.as_mut().is_some_and(Recording::running) {
            bail!("A recording is already running; call video_stop first.");
        }
        let codec = args.codec.unwrap_or_else(|| "h264".into());
        if !matches!(codec.as_str(), "h264" | "hevc") {
            bail!("codec must be h264 or hevc.");
        }
        let fps = args.fps.unwrap_or(60);
        if fps == 0 || fps > MAX_RECORD_FPS {
            bail!("fps must be between 1 and {MAX_RECORD_FPS}.");
        }
        let game_fps = args.game_fps.unwrap_or(0);
        if game_fps > MAX_RECORD_FPS {
            bail!("game_fps must be between 1 and {MAX_RECORD_FPS}.");
        }
        // A game capped at or below the video rate has every frame copied.
        let copy_fps = if game_fps != 0 && game_fps <= fps {
            0
        } else {
            fps
        };
        // On Hyprland a hidden window renders at the unfocused rate. Raise it to the video rate,
        // and well above a game cap so the compositor's frame callbacks do not shift capped
        // presents.
        let hidden_fps = i64::from(fps.max((game_fps * 4).min(MAX_RECORD_FPS)));
        let pid = self.running_pid()?;
        let path = args.path.unwrap_or_else(|| {
            self.install
                .agent_dir()
                .join("recordings")
                .join(format!("recording-{}.mp4", unix_millis(self.clock.wall())))
        });
        let frames = platform::frame_file(pid, unix_millis(self.clock.wall()));
        let mut notes = Vec::new();
        let restore_fps = platform::desktop()
            .raise_hidden_fps(hidden_fps)
            .unwrap_or_else(|note| {
                notes.push(note);
                None
            });
        let request = protocol::Request::FramesStart {
            path: platform::agent_path(&frames),
            fps: copy_fps,
            game_fps,
        };
        let started = self
            .with_agent(|agent, _| agent.send(request.clone()))
            .and_then(|()| {
                Recording::start(
                    frames,
                    path,
                    fps,
                    &codec,
                    args.audio.unwrap_or(true),
                    args.pause_on_idle_ms.map(Duration::from_millis),
                    &*self.clock,
                )
            });
        let mut recording = match started {
            Ok(recording) => recording,
            Err(error) => {
                let _ = self.with_agent(|agent, _| agent.send(protocol::Request::FramesStop));
                if let Some(previous) = restore_fps {
                    platform::desktop().restore_hidden_fps(previous);
                }
                return Err(error);
            }
        };
        recording.restore_unfocused_fps = restore_fps;
        let report = json!({
            "recording": true,
            "path": recording.path,
            "log": recording.log,
            "fps": fps,
            "game_fps": (game_fps != 0).then_some(game_fps),
            "size": [recording.shape.0, recording.shape.1],
            "codec": codec,
            "pause_on_idle_ms": args.pause_on_idle_ms,
            "notes": notes,
        });
        self.recording = Some(recording);
        Ok(report)
    }

    pub(crate) fn record_stop(&mut self) -> Result<Value> {
        let Some(recording) = self.recording.take() else {
            bail!("Nothing is recording.");
        };
        self.finish_recording(recording)
    }

    /// Stop the agent's frame copy, restore the hidden-window rate and finish the file.
    pub(super) fn finish_recording(&mut self, recording: Recording) -> Result<Value> {
        // Without an agent connection the agent has already stopped copying.
        let _ = self.with_agent(|agent, _| agent.send(protocol::Request::FramesStop));
        if let Some(previous) = recording.restore_unfocused_fps {
            platform::desktop().restore_hidden_fps(previous);
        }
        recording.stop(&*self.clock)
    }

    /// The game window from the desktop, or from the agent's client size when the desktop
    /// exposes no windows.
    pub(super) fn game_window(&mut self, pid: u32) -> Result<Window> {
        let desktop = platform::desktop();
        if let Some(window) = desktop.window(pid)? {
            return Ok(window);
        }
        let status = self.with_agent(|agent, _| agent.status())?;
        Ok(Window::from_agent(&status, desktop.name()))
    }

    /// Capture the game window to `path` at full size; return an inline PNG at most
    /// `max_width` physical pixels wide when asked. The desktop captures it when it can;
    /// otherwise this takes the newest frame of the agent's frame copy: the recording's when
    /// one runs, else a one-frame copy.
    fn capture(
        &mut self,
        window: &Window,
        path: &Path,
        max_width: Option<u32>,
    ) -> Result<Option<Vec<u8>>> {
        if let Some(captured) = platform::desktop().screenshot(window, path, max_width) {
            return captured;
        }
        if let Some(recording) = self.recording.as_mut()
            && recording.running()
        {
            let file = FrameFile::open(recording.frames())?
                .ok_or_else(|| Error::msg("The recording has no frames yet."))?;
            return still::from_file(&file, &*self.clock)?.save(path, max_width);
        }
        let pid = self.running_pid()?;
        let frames = platform::frame_file(pid, unix_millis(self.clock.wall()));
        let request = protocol::Request::FramesStart {
            path: platform::agent_path(&frames),
            fps: 0,
            game_fps: 0,
        };
        self.with_agent(|agent, _| agent.send(request.clone()))?;
        let grabbed = still::grab(&frames, &*self.clock);
        let _ = self.with_agent(|agent, _| agent.send(protocol::Request::FramesStop));
        let _ = std::fs::remove_file(&frames);
        grabbed?.save(path, max_width)
    }

    /// Click the button labelled `text`: through the live UI tree when the unstable layer
    /// reads it, scrolling its list into view; otherwise find the text with OCR, scrolling
    /// the list under the region (or the window centre) when asked and it is not visible yet.
    pub(crate) fn click_text(&mut self, args: ClickTextArgs) -> Result<Value> {
        let region = args.region.as_ref().map(Region::parse).transpose()?;
        let button = if args.right {
            "MouseRight"
        } else {
            "MouseLeft"
        };
        let mut notes = Vec::new();
        let click = TreeClick {
            text: &args.text,
            exact: args.exact,
            max_scrolls: args.max_scrolls.unwrap_or(DEFAULT_CLICK_SCROLLS),
            button,
            region,
            control: None,
        };
        if let Some(reply) = self.click_with_tree(&click, &mut notes)? {
            return Ok(reply);
        }
        let fonts = screen_text::fonts(&self.game_dir()?)?;
        let path = self
            .install
            .agent_dir()
            .join("screenshots")
            .join(format!(".click-{}.png", std::process::id()));
        let mut read: Vec<String> = Vec::new();
        let max_scrolls = if args.scroll {
            args.max_scrolls.unwrap_or(DEFAULT_CLICK_SCROLLS)
        } else {
            0
        };
        let mut previous: Vec<String>;
        let mut scrolls = 0;
        let found = loop {
            let pid = self.running_pid()?;
            let window = self.game_window(pid)?;
            self.capture(&window, &path, None)?;
            let image = ocr::Image::open(&path).map_err(|error| Error::msg(error.to_string()))?;
            let lines = screen_text::read(&image, window.width as f64, region, &fonts)?;
            // An exact line wins, so "Villager" does not pick "Zombie Villager".
            let hit = screen_text::locate(&lines, &args.text, args.exact);
            previous =
                std::mem::replace(&mut read, lines.into_iter().map(|line| line.text).collect());
            if let Some(hit) = hit {
                break Some(hit);
            }
            if scrolls >= max_scrolls || (scrolls > 0 && read == previous) {
                break None;
            }
            let (x, y) = region.map_or(
                (window.width as f64 / 2.0, window.height as f64 / 2.0),
                |region| {
                    (
                        region.x + region.width / 2.0,
                        region.y + region.height / 2.0,
                    )
                },
            );
            self.input(serde_json::from_value(
                json!({ "pointer": [x, y], "wheel": { "y": -CLICK_SCROLL_NOTCHES } }),
            )?)?;
            self.clock.sleep(CLICK_SCROLL_SETTLE);
            scrolls += 1;
        };
        let _ = std::fs::remove_file(&path);
        let Some((line, how)) = found else {
            self.input(serde_json::from_value(json!({ "pointer": null }))?)?;
            bail!(
                "{:?} is not on screen{}; OCR read {read:?}{}",
                args.text,
                if args.scroll {
                    format!(" after {scrolls} scrolls")
                } else {
                    " (scroll: true searches a list)".to_owned()
                },
                if notes.is_empty() {
                    String::new()
                } else {
                    format!(". {}", notes.join("; "))
                }
            );
        };
        let (x, y) = (line.x + line.width / 2.0, line.y + line.height / 2.0);
        self.input(serde_json::from_value(
            json!({ "pointer": [x, y], "press": [button] }),
        )?)?;
        self.input(serde_json::from_value(json!({ "pointer": null }))?)?;
        Ok(json!({
            "clicked": line.to_json(),
            "match": format!("{how:?}").to_lowercase(),
            "button": button,
            "scrolls": scrolls,
            "source": "ocr",
            "notes": notes,
        }))
    }

    /// Wait up to `timeout` for menu text on screen: OCR lines below the top
    /// [`MENU_TOP_SHARE`] of the window. Returns the lines read there, or why the check could
    /// not run.
    pub(super) fn wait_for_menu_text(
        &mut self,
        timeout: Duration,
    ) -> std::result::Result<Vec<String>, String> {
        self.wait_for_lines(timeout, 1, shows_menu)
            .map_err(|last| format!("OCR read no menu text; {last}"))
    }

    /// Wait up to `timeout` for gameplay on screen as OCR sees it: two reads in a row with no
    /// text below the top [`MENU_TOP_SHARE`] of the window, where menus and the loading
    /// screen's job and tips show. The last resort when nothing else tells.
    pub(super) fn wait_for_gameplay_text(
        &mut self,
        timeout: Duration,
    ) -> std::result::Result<(), String> {
        self.wait_for_lines(timeout, 2, <[String]>::is_empty)
            .map(drop)
            .map_err(|last| format!("OCR still read menu or loading text; {last}"))
    }

    /// Poll captures until `reads` reads in a row pass `want` with their [`menu_lines`].
    fn wait_for_lines(
        &mut self,
        timeout: Duration,
        reads: usize,
        want: impl Fn(&[String]) -> bool,
    ) -> std::result::Result<Vec<String>, String> {
        let fonts = self
            .game_dir()
            .and_then(|dir| screen_text::fonts(&dir))
            .map_err(|error| format!("OCR is unavailable: {error}"))?;
        let path = self
            .install
            .agent_dir()
            .join("screenshots")
            .join(format!(".menu-{}.png", std::process::id()));
        let clock = std::sync::Arc::clone(&self.clock);
        let mut last = Vec::new();
        let mut passed = 0;
        let found = poll(
            &*clock,
            timeout,
            WAIT_POLL,
            || -> Result<Option<Vec<String>>> {
                let pid = self.running_pid()?;
                let window = self.game_window(pid)?;
                self.capture(&window, &path, None)?;
                last = menu_lines(&path, &window, &fonts)?;
                passed = if want(&last) { passed + 1 } else { 0 };
                Ok((passed >= reads).then(|| last.clone()))
            },
        );
        let _ = std::fs::remove_file(&path);
        match found {
            Ok(Some(lines)) => Ok(lines),
            Ok(None) => Err(format!(
                "within {} s it last read {last:?}",
                timeout.as_secs()
            )),
            Err(error) => Err(error.to_string()),
        }
    }
}

/// Text lines below the top [`MENU_TOP_SHARE`] of the capture at `path`. The code takes
/// [`MENU_LINES`] of them as the menu being up, since it expects the loading screen to show
/// no text there.
pub fn menu_lines(path: &Path, window: &Window, fonts: &ocr::Fonts) -> Result<Vec<String>> {
    let image = ocr::Image::open(path).map_err(|error| Error::msg(error.to_string()))?;
    let top = window.height as f64 * MENU_TOP_SHARE;
    let region = Region {
        x: 0.0,
        y: top,
        width: window.width as f64,
        height: window.height as f64 - top,
    };
    Ok(
        screen_text::read(&image, window.width as f64, Some(region), fonts)?
            .into_iter()
            .map(|line| line.text)
            .filter(|text| text.chars().filter(|c| c.is_alphanumeric()).count() >= 3)
            .collect(),
    )
}

/// Whether `lines` from [`menu_lines`] show the menu.
pub fn shows_menu(lines: &[String]) -> bool {
    lines.len() >= MENU_LINES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conditions_parse_from_objects_and_bare_names() {
        let parse = |value: Value| Condition::parse(&value);
        assert_eq!(parse(json!("in_world")).unwrap(), Condition::InWorld);
        assert_eq!(
            parse(json!({"in_world": null})).unwrap(),
            Condition::InWorld
        );
        assert_eq!(
            parse(json!({"camera_finished": null})).unwrap(),
            Condition::CameraFinished
        );
        assert_eq!(
            parse(json!({"screen": {"name": "chat"}})).unwrap(),
            Condition::Screen("chat".into())
        );
        assert_eq!(
            parse(json!({"chunks_loaded": {"radius": 4}})).unwrap(),
            Condition::ChunksLoaded(4)
        );
        assert_eq!(
            parse(json!({"entity": {"matches": "minecraft:zombie", "radius": 8}})).unwrap(),
            Condition::Entity {
                matches: "minecraft:zombie".into(),
                radius: 8.0
            }
        );
        assert_eq!(
            parse(json!({"content_log": {"level": "ERROR"}})).unwrap(),
            Condition::ContentLog {
                level: Some("error".into()),
                contains: None
            }
        );
        assert_eq!(
            parse(json!({"text": {"contains": "Resume", "region": {"x": 0, "y": 10, "width": 300, "height": 40}}})).unwrap(),
            Condition::Text {
                contains: "Resume".into(),
                region: Some(Region {
                    x: 0.0,
                    y: 10.0,
                    width: 300.0,
                    height: 40.0
                })
            }
        );
        assert_eq!(
            parse(json!({"text": {"contains": "Quit"}})).unwrap(),
            Condition::Text {
                contains: "Quit".into(),
                region: None
            }
        );
        assert_eq!(
            parse(json!({"event": {"name": "block_break", "contains": "Steve"}})).unwrap(),
            Condition::Event {
                name: Some("block_break".into()),
                contains: Some("Steve".into())
            }
        );
        assert!(parse(json!({"text": {}})).is_err());
        assert!(parse(json!({"text": {"contains": " "}})).is_err());
        assert!(parse(json!({"text": {"contains": "a", "region": {"x": 0}}})).is_err());
        assert!(parse(json!({"screen": {}})).is_err());
        assert!(parse(json!({"in_world": null, "camera_finished": null})).is_err());
        assert!(parse(json!("inventory_ready")).is_err());
        assert!(parse(json!(3)).is_err());
    }

    #[test]
    fn warnings_name_unusual_modes() {
        let unstable = json!({"capabilities": {"screens": "active", "ui_tree": "pending", "camera": "failed"}});
        let death = json!({"kind": "death", "detail": "The player is dead."});
        let check = |attention: &Value| {
            let found = warnings(&WarningFacts {
                anchors_only: true,
                unstable_disabled: false,
                unstable_failed: None,
                unstable: &unstable,
                attention,
                in_world: Some(true),
                connected: false,
                covering: Some("pause_screen".into()),
                other_mods: vec!["Shiny Client.dll".into()],
            });
            let kinds: Vec<String> = found
                .iter()
                .map(|warning| warning["kind"].as_str().unwrap().to_owned())
                .collect();
            (found, kinds)
        };
        let (found, kinds) = check(&Value::Null);
        assert_eq!(
            kinds,
            [
                "unstable_anchors_only",
                "unstable_capabilities",
                "websocket_disconnected",
                "screen_open",
                "other_mods"
            ]
        );
        let detail = found[1]["detail"].as_str().unwrap();
        assert!(
            detail.contains("camera (failed)") && !detail.contains("ui_tree"),
            "{detail}"
        );
        let (_, kinds) = check(&death);
        assert_eq!(kinds[0], "death");
        assert!(
            !kinds.contains(&"screen_open".to_owned()),
            "the death screen is reported once"
        );
    }
}
