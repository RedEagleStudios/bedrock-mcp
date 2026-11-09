//! The MCP session: state shared by tool calls and tool dispatch.

mod bds;
mod events;
mod files;
mod hud;
mod lifecycle;
mod observe;
mod play;
mod render;
mod scene;
mod sequences;
#[cfg(test)]
mod tests;
mod timeline;
mod ui;
mod unstable;
pub(crate) mod world_state;
mod worlds;

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use serde_json::Value;

use crate::{
    agent::AgentConnection,
    camera::CameraPath,
    clock::{Clock, poll},
    content_log::Cursor,
    drivers::Background,
    error::{Error, Result, bail},
    events::EventHub,
    game_files, inject,
    input::Controller,
    install::Install,
    keys::{self, Bindings},
    packs::GlobalPack,
    recorder::Recording,
    rpc::{Content, Pump, ToolServer},
    tools::{self, Toolsets},
    unstable::{Host, InstallHost, Unstable, live},
    world::World,
    ws::GameSocket,
};

use lifecycle::LocalServer;

/// How long the game gets to connect after the agent typed `/connect`.
const WEBSOCKET_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a screen gets to close after Escape.
const SCREEN_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
/// Screens over gameplay that Escape closes, so typing `/connect` can go ahead.
const CLOSABLE_SCREENS: [&str; 3] = ["pause", "chat", "content_log_history"];

pub struct Session {
    install: Install,
    clock: Arc<dyn Clock>,
    agent: Option<AgentConnection>,
    controller: Controller,
    /// `Err` holds why the WebSocket server could not listen.
    world: Result<World, String>,
    camera: Option<CameraPath>,
    recording: Option<Recording>,
    local_server: Option<LocalServer>,
    /// The last answer [`Session::in_world`] gave, and how many times it saw the player
    /// enter a world: the world loads this session observed.
    last_in_world: Option<bool>,
    world_loads: u64,
    /// Errors counted in the current content log, up to `content_cursor`.
    content_errors: u64,
    content_cursor: Option<Cursor>,
    /// Global packs staged by activate/deactivate, not yet applied.
    staged_packs: Option<Vec<GlobalPack>>,
    /// The optional unstable layer; every use falls back to stable behaviour.
    unstable: Unstable,
    /// The toolsets `tools/list` offers.
    toolsets: Toolsets,
    /// The merged WebSocket and bridge event stream.
    events: EventHub,
    /// Background action sequences.
    background: Background,
    /// Turns true once the MCP client went away.
    client_gone: Option<Arc<AtomicBool>>,
    /// One line per tool call; see [`timeline`].
    timeline: crate::timeline::Timeline,
}

impl Session {
    pub fn new(install: Install, clock: Arc<dyn Clock>, toolsets: Toolsets) -> Self {
        let host = InstallHost {
            install: install.clone(),
            clock: Arc::clone(&clock),
        };
        let mut session = Self::with_unstable_host(install, clock, Box::new(host));
        session.toolsets = toolsets;
        session
    }

    fn with_unstable_host(install: Install, clock: Arc<dyn Clock>, host: Box<dyn Host>) -> Self {
        let world = GameSocket::bind()
            .map(|socket| World::new(Arc::new(socket)))
            .map_err(|error| format!("The WebSocket server could not listen: {error}"));
        Self {
            timeline: crate::timeline::Timeline::new(&install.agent_dir(), clock.wall()),
            install,
            clock,
            agent: None,
            controller: Controller::default(),
            world,
            camera: None,
            recording: None,
            local_server: None,
            last_in_world: None,
            world_loads: 0,
            content_errors: 0,
            content_cursor: None,
            staged_packs: None,
            unstable: Unstable::new(host),
            toolsets: Toolsets::all(),
            events: EventHub::default(),
            background: Background::default(),
            client_gone: None,
        }
    }

    pub(crate) fn world(&self) -> Result<World> {
        self.world.clone().map_err(Error::Message)
    }

    /// The game installation this session drives.
    pub(crate) fn install(&self) -> &Install {
        &self.install
    }

    fn running_pid(&self) -> Result<u32> {
        self.install
            .game_pid()
            .ok_or_else(|| Error::msg("Minecraft is not running; call game_launch first."))
    }

    /// The player's bindings; empty when options.txt cannot be read.
    fn bindings(&self) -> Bindings {
        game_files::options_path(&self.install.prefix)
            .and_then(|path| game_files::read_options(&path))
            .map(|options| keys::bindings(&options))
            .unwrap_or_default()
    }

    /// Whether the player is in a world, and what told: the live screen stacks (a gameplay
    /// stack exists), else the WebSocket. The game keeps its WebSocket connection after it
    /// leaves a world, so a connection alone proves nothing: `testfor @s` succeeds only in a
    /// world, and the menus answer that the command is unknown. None when neither can tell;
    /// why goes to `notes`.
    fn in_world(&mut self, notes: &mut Vec<String>) -> (Option<bool>, Option<&'static str>) {
        let (in_world, source) = self.read_in_world(notes);
        if in_world == Some(true) && self.last_in_world != Some(true) {
            self.world_loads += 1;
        }
        if in_world.is_some() {
            self.last_in_world = in_world;
        }
        (in_world, source)
    }

    fn read_in_world(&mut self, notes: &mut Vec<String>) -> (Option<bool>, Option<&'static str>) {
        if self.unstable_capability("screens", notes) {
            match self.unstable.screens() {
                Ok(stacks) => {
                    return (Some(live::gameplay_top(&stacks).is_some()), Some("screens"));
                }
                Err(error) => notes.push(format!("unstable screens failed ({error})")),
            }
        }
        match self.world() {
            Ok(world) if world.connected() => match world.test("testfor @s") {
                Ok(in_world) => return (Some(in_world), Some("websocket")),
                Err(error) => notes.push(format!("the WebSocket did not answer ({error})")),
            },
            _ => notes.push("the WebSocket is not connected".to_owned()),
        }
        (None, None)
    }

    /// Run `action` with the agent, attaching (and injecting) first when needed and
    /// reconnecting once if the connection turns out to be gone.
    fn with_agent<T>(
        &mut self,
        mut action: impl FnMut(&mut AgentConnection, &mut Controller) -> Result<T>,
    ) -> Result<T> {
        let mut attempt = 0;
        loop {
            if self.agent.is_none() {
                self.agent = Some(inject::attach(&self.install, &*self.clock)?);
            }
            let agent = self.agent.as_mut().expect("attached above");
            match action(agent, &mut self.controller) {
                Err(Error::Disconnected(message)) => {
                    self.agent = None;
                    self.controller.forget();
                    attempt += 1;
                    if attempt == 2 {
                        return Err(Error::Disconnected(message));
                    }
                }
                other => return other,
            }
        }
    }

    /// The connected WebSocket world, typing `/connect` through chat when the player is in a
    /// world but the game has not connected yet.
    pub(crate) fn ensure_world(&mut self) -> Result<World> {
        let world = self.world()?;
        if world.connected() {
            return Ok(world);
        }
        self.running_pid()?;
        let mut notes = Vec::new();
        let (in_world, _) = self.in_world(&mut notes);
        if in_world == Some(false) {
            bail!(
                "Minecraft's WebSocket is not connected and the player is not in a world. Join a \
                 world (with cheats for commands) first; the server connects it from there."
            );
        }
        self.clear_gameplay_screen()?;
        let command = format!("/connect 127.0.0.1:{}", world.socket().port());
        let bindings = self.bindings();
        let clock = Arc::clone(&self.clock);
        self.with_agent(|agent, controller| {
            controller.send_chat(agent, &*clock, &bindings, &command)
        })?;
        if !world.socket().wait_connected(WEBSOCKET_CONNECT_TIMEOUT) {
            let unsure = if in_world.is_none() {
                format!(
                    " Nothing confirmed that the player is in a world ({}), so the server typed \
                     it anyway.",
                    notes.join("; ")
                )
            } else {
                String::new()
            };
            bail!(
                "Minecraft did not connect to ws://127.0.0.1:{}. In a world without cheats, \
                 /connect gets the answer \"Cheats aren't enabled in this world.\" in chat (the \
                 game's commands.generic.disabled text); turn cheats on in the world's settings. \
                 Otherwise turn on Settings → General → WebSockets and turn off Require \
                 Encrypted WebSockets (game_launch sets both while the game is stopped), or \
                 run {command} in chat yourself.{unsure}",
                world.socket().port()
            );
        }
        Ok(world)
    }

    /// Make sure chat can open: close a pause, chat or content log history screen with
    /// Escape, and refuse when another screen covers gameplay, since typing into it would go
    /// somewhere else. The history screen opens by itself when a world loads with pack errors
    /// and the player's options have `content_log_gui_show_on_errors` on.
    fn clear_gameplay_screen(&mut self) -> Result<()> {
        let Some(screen) = self.covering_screen() else {
            return Ok(());
        };
        if !CLOSABLE_SCREENS
            .iter()
            .any(|closable| screen.contains(closable))
        {
            bail!(
                "{screen} is open over the game, so the server cannot type /connect into chat. \
                 Close it (frame_capture shows what it asks), then try again."
            );
        }
        let escape = keys::resolve("Escape", &self.bindings())?;
        let clock = Arc::clone(&self.clock);
        self.with_agent(|agent, controller| {
            controller.press(agent, &*clock, std::slice::from_ref(&escape), 1)
        })?;
        let closed = poll(
            &*clock,
            SCREEN_CLOSE_TIMEOUT,
            Duration::from_millis(100),
            || Ok(self.covering_screen().is_none().then_some(())),
        )?;
        if closed.is_none() {
            let still = self.covering_screen().unwrap_or(screen);
            bail!(
                "{still} stayed open after Escape, so the server did not type /connect. Close \
                 it, then try again."
            );
        }
        Ok(())
    }
}

impl ToolServer for Session {
    fn tools(&self) -> Vec<Value> {
        tools::definitions(&self.toolsets)
    }

    fn offers(&self, name: &str) -> bool {
        tools::offers(&self.toolsets, name)
    }

    fn call(&mut self, name: &str, arguments: Value) -> Result<Vec<Content>> {
        let call = self.call_started(name, &arguments);
        let allowed = if self.background.nested {
            self.background.conflict(name, &arguments)
        } else {
            Ok(())
        };
        let result = allowed.and_then(|()| {
            let toolsets = self.toolsets.clone();
            tools::call(&toolsets, self, name, arguments)
        });
        self.call_finished(call, &result);
        result
    }

    fn close(&mut self) {
        self.release_session();
    }

    fn work(&mut self, pump: &mut dyn Pump) {
        // A recording does not pause while a background run plays, and marks where it ended.
        let run = self.background.active().map(|log| log.borrow().id.clone());
        if run.is_some() {
            self.background_busy(true);
        }
        self.run_background(pump);
        if let Some(run) = run {
            self.background_busy(false);
            self.mark(&format!("{run} ended"));
        }
    }

    fn attach_client(&mut self, gone: Arc<AtomicBool>) {
        self.client_gone = Some(gone);
    }
}

impl Session {
    /// Whether the MCP client went away.
    pub(crate) fn client_gone(&self) -> bool {
        self.client_gone
            .as_ref()
            .is_some_and(|gone| gone.load(Ordering::SeqCst))
    }
}
