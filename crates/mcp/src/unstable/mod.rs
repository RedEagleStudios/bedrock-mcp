//! The optional unstable layer: `bedrock_unstable.dll`, which reaches game internals through
//! vtables it finds by RTTI or, without RTTI, by string anchors. It loads on any build once the
//! stable agent is attached; each capability checks itself on that build and reports its own
//! state. Every failure is remembered per game process and turns into a note: callers fall back
//! to the stable behaviour.

mod client;
pub mod live;

use std::{sync::Arc, time::Duration};

use protocol::unstable::{
    CameraState, Capability, CapabilityState, DLL_NAME, EntitySnapshot, HudState, LOG_FILE,
    LogBatch, NativePlayer, OreUiView, Perf, PlayerRender, PropertySnapshot, PropertyTarget, Reply,
    Request, SceneStack, Status, UiScreen,
};
use serde_json::{Value, json};

use crate::{
    clock::Clock,
    error::{Error, Result},
    inject,
    install::Install,
};
pub use client::Link;

/// `BEDROCK_MCP_UNSTABLE=0` keeps the layer from loading.
pub const SETTING: &str = "BEDROCK_MCP_UNSTABLE";

/// The DLL may need a moment after injection before it listens.
const ENDPOINT_TIMEOUT: Duration = Duration::from_secs(15);

/// What loading needs from the machine; tests replace it.
pub trait Host {
    fn game_pid(&self) -> Option<u32>;
    /// `BEDROCK_MCP_UNSTABLE`.
    fn setting(&self) -> Option<String>;
    /// Connect to a running unstable endpoint, or inject the DLL and connect.
    fn attach(&self, pid: u32) -> Result<Box<dyn Link>>;
}

/// The real machine: the running game's installation and the injector. It follows the game
/// that runs, which game_launch may have found after this host was made.
pub struct InstallHost {
    pub install: Install,
    pub clock: Arc<dyn Clock>,
}

impl Host for InstallHost {
    fn game_pid(&self) -> Option<u32> {
        self.install.select(None).ok()?.game_pid()
    }

    fn setting(&self) -> Option<String> {
        std::env::var(SETTING).ok()
    }

    fn attach(&self, pid: u32) -> Result<Box<dyn Link>> {
        let install = self.install.select(Some(pid))?;
        let dir = install.agent_dir();
        if let Some(connection) = client::try_connect(&dir)? {
            return Ok(Box::new(connection));
        }
        let connection = inject::inject_and_connect(
            &install,
            pid,
            DLL_NAME,
            LOG_FILE,
            ENDPOINT_TIMEOUT,
            &*self.clock,
            || client::try_connect(&dir),
        )?;
        Ok(Box::new(connection))
    }
}

enum Load {
    /// Not tried for this process yet.
    Idle,
    Loaded {
        link: Box<dyn Link>,
        status: Status,
    },
    /// Failed for this process; retried after the game restarts.
    Failed(String),
}

/// The unstable layer's state for the current game process.
pub struct Unstable {
    host: Box<dyn Host>,
    pid: Option<u32>,
    load: Load,
    /// Why the last [`Unstable::ensure`] did not load it.
    note: Option<String>,
}

impl Unstable {
    pub fn new(host: Box<dyn Host>) -> Self {
        Self {
            host,
            pid: None,
            load: Load::Idle,
            note: None,
        }
    }

    fn disabled(&self) -> bool {
        self.host.setting().as_deref().map(str::trim) == Some("0")
    }

    /// Load the layer if it is not disabled and the stable agent is attached; true when it
    /// is loaded. Never fails: the reason lands in [`Unstable::note`].
    pub fn ensure(&mut self, agent_attached: bool) -> bool {
        // Disabled costs nothing: no process scan.
        if self.disabled() {
            self.note = Some(format!("unstable layer not loaded: {SETTING}=0"));
            return false;
        }
        let pid = self.host.game_pid();
        if pid != self.pid {
            // A different (or no) game process: forget everything about the old one.
            self.pid = pid;
            self.load = Load::Idle;
        }
        let Some(pid) = pid else {
            self.note = Some("Minecraft is not running".into());
            return false;
        };
        match &self.load {
            Load::Loaded { .. } => return true,
            Load::Failed(reason) => {
                self.note = Some(format!(
                    "unstable layer failed for this game process: {reason}"
                ));
                return false;
            }
            Load::Idle => {}
        }
        if !agent_attached {
            self.note = Some("unstable layer waits for the stable agent to be attached".into());
            return false;
        }
        let loaded =
            self.host
                .attach(pid)
                .and_then(|mut link| match link.request(&Request::Status)? {
                    Reply::Status(status) => Ok(Load::Loaded { link, status }),
                    other => Err(Error::msg(format!("unexpected status reply {other:?}"))),
                });
        self.load = loaded.unwrap_or_else(|error| Load::Failed(error.to_string()));
        if let Load::Failed(reason) = &self.load {
            self.note = Some(format!("unstable layer failed to load: {reason}"));
            return false;
        }
        self.note = None;
        true
    }

    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    /// Send a request; a broken connection marks the layer failed for this process.
    fn request(&mut self, request: &Request) -> Result<Reply> {
        let Load::Loaded { link, .. } = &mut self.load else {
            return Err(Error::msg("the unstable layer is not loaded"));
        };
        match link.request(request) {
            Ok(Reply::Error { message }) => Err(Error::msg(format!("unstable layer: {message}"))),
            Ok(reply) => Ok(reply),
            Err(error) => {
                let reason = error.to_string();
                self.load = Load::Failed(reason.clone());
                self.note = Some(format!("unstable layer failed: {reason}"));
                Err(Error::msg(reason))
            }
        }
    }

    /// Whether capability `name` is Active, refreshing the status while it is not: a
    /// pending capability may have confirmed, a failed one recovered. Otherwise the note
    /// names its state and the DLL's reason.
    pub fn active(&mut self, name: &str) -> bool {
        fn find<'a>(load: &'a Load, name: &str) -> Option<&'a Capability> {
            match load {
                Load::Loaded { status, .. } => status
                    .capabilities
                    .iter()
                    .find(|capability| capability.name == name),
                _ => None,
            }
        }
        if find(&self.load, name).is_some_and(|found| found.state != CapabilityState::Active)
            && let Ok(Reply::Status(fresh)) = self.request(&Request::Status)
            && let Load::Loaded { status, .. } = &mut self.load
        {
            *status = fresh;
        }
        if !matches!(self.load, Load::Loaded { .. }) {
            return false;
        }
        let note = match find(&self.load, name) {
            Some(found) if found.state == CapabilityState::Active => return true,
            Some(found) => {
                let state = json!(found.state);
                let state = state.as_str().unwrap_or_default();
                if found.detail.is_empty() {
                    format!("unstable capability {name} is {state}")
                } else {
                    format!("unstable capability {name} is {state}: {}", found.detail)
                }
            }
            None => format!("unstable capability {name} is not reported by the loaded DLL"),
        };
        self.note = Some(note);
        false
    }

    /// Whether a request to capability `name` may run: it is active, or pending. A pending
    /// capability that confirms itself on its first request (ui_tree, oreui) would otherwise
    /// wait forever; the DLL checks the layout on that request and answers with an error
    /// rather than a guess.
    pub fn requestable(&mut self, name: &str) -> bool {
        if self.active(name) {
            return true;
        }
        let Load::Loaded { status, .. } = &self.load else {
            return false;
        };
        let pending = status.capabilities.iter().any(|capability| {
            capability.name == name && capability.state == CapabilityState::Pending
        });
        if pending {
            self.note = None;
        }
        pending
    }

    pub fn content_log(&mut self, since: u64, limit: u32) -> Result<LogBatch> {
        match self.request(&Request::ContentLog { since, limit })? {
            Reply::ContentLog(batch) => Ok(batch),
            other => Err(Error::msg(format!(
                "unexpected content log reply {other:?}"
            ))),
        }
    }

    pub fn screens(&mut self) -> Result<Vec<SceneStack>> {
        match self.request(&Request::Screens)? {
            Reply::Screens { stacks } => Ok(stacks),
            other => Err(Error::msg(format!("unexpected screens reply {other:?}"))),
        }
    }

    /// The control trees of the screens on top of the scene stacks (or of `stack` only).
    pub fn ui_tree(
        &mut self,
        stack: Option<u64>,
        max_depth: Option<u32>,
        visible_only: bool,
    ) -> Result<Vec<UiScreen>> {
        let request = Request::UiTree {
            stack,
            max_depth,
            visible_only,
        };
        match self.request(&request)? {
            Reply::UiTree { screens } => Ok(screens),
            other => Err(Error::msg(format!("unexpected ui tree reply {other:?}"))),
        }
    }

    pub fn camera(&mut self) -> Result<CameraState> {
        match self.request(&Request::Camera)? {
            Reply::Camera(camera) => Ok(camera),
            other => Err(Error::msg(format!("unexpected camera reply {other:?}"))),
        }
    }

    /// Toasts and HUD text, with at most `chat_limit` chat lines.
    pub fn hud(&mut self, chat_limit: Option<u32>) -> Result<HudState> {
        match self.request(&Request::Hud { chat_limit })? {
            Reply::Hud(hud) => Ok(hud),
            other => Err(Error::msg(format!("unexpected hud reply {other:?}"))),
        }
    }

    /// The pages of the live Ore UI views (whose URL contains `url`, when given).
    pub fn oreui(&mut self, url: Option<String>) -> Result<Vec<OreUiView>> {
        match self.request(&Request::OreUi { url })? {
            Reply::OreUi { views } => Ok(views),
            other => Err(Error::msg(format!("unexpected oreui reply {other:?}"))),
        }
    }

    /// Capability names and states of the loaded layer.
    pub fn capabilities(&self) -> Option<Value> {
        let Load::Loaded { status, .. } = &self.load else {
            return None;
        };
        Some(
            status
                .capabilities
                .iter()
                .map(|capability| (capability.name.clone(), json!(capability.state)))
                .collect::<serde_json::Map<_, _>>()
                .into(),
        )
    }

    /// The `unstable_status` report; refreshes the DLL's status when loaded.
    pub fn report(&mut self) -> Value {
        if matches!(self.load, Load::Loaded { .. })
            && let Ok(Reply::Status(fresh)) = self.request(&Request::Status)
            && let Load::Loaded { status, .. } = &mut self.load
        {
            *status = fresh;
        }
        let load = match &self.load {
            Load::Idle => json!({ "state": "not_loaded", "detail": self.note }),
            Load::Loaded { .. } => json!({ "state": "loaded" }),
            Load::Failed(reason) => json!({ "state": "failed", "detail": reason }),
        };
        let status = match &self.load {
            Load::Loaded { status, .. } => json!(status),
            _ => Value::Null,
        };
        json!({
            "setting": self.host.setting(),
            "enabled": !self.disabled(),
            "pid": self.pid,
            "load": load,
            "status": status,
        })
    }

    /// Entities on the game's local server, read on its thread at the next tick.
    pub fn world_entities(&mut self, request: Request) -> Result<EntitySnapshot> {
        match self.request(&request)? {
            Reply::WorldEntities(snapshot) => Ok(snapshot),
            other => Err(Error::msg(format!("unexpected entities reply {other:?}"))),
        }
    }

    /// One player on the game's local server (the named one, else the only one), read on its
    /// thread at the next tick.
    pub fn world_player(&mut self, name: Option<String>) -> Result<NativePlayer> {
        match self.request(&Request::WorldPlayer { name })? {
            Reply::WorldPlayer(player) => Ok(player),
            other => Err(Error::msg(format!("unexpected player reply {other:?}"))),
        }
    }

    /// Dynamic properties of the world or an entity on the game's local server.
    pub fn dynamic_properties(
        &mut self,
        target: PropertyTarget,
        collection: Option<String>,
    ) -> Result<PropertySnapshot> {
        match self.request(&Request::DynamicProperties { target, collection })? {
            Reply::DynamicProperties(snapshot) => Ok(snapshot),
            other => Err(Error::msg(format!(
                "unexpected dynamic properties reply {other:?}"
            ))),
        }
    }

    /// The local player's animation components (`actor` is its unique id when known), read
    /// when next evaluated.
    pub fn player_render(
        &mut self,
        wait_ms: Option<u32>,
        actor: Option<i64>,
    ) -> Result<PlayerRender> {
        match self.request(&Request::PlayerRender { wait_ms, actor })? {
            Reply::PlayerRender(render) => Ok(render),
            other => Err(Error::msg(format!(
                "unexpected player render reply {other:?}"
            ))),
        }
    }

    /// The frame and update timing samples the DLL keeps.
    pub fn perf(&mut self) -> Result<Perf> {
        match self.request(&Request::Perf)? {
            Reply::Perf(perf) => Ok(perf),
            other => Err(Error::msg(format!("unexpected perf reply {other:?}"))),
        }
    }
}

#[cfg(test)]
pub mod fake;

#[cfg(test)]
mod tests;
