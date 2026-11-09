//! Starting, joining, crashing and stopping the game, and the session's local server.

use std::{
    collections::BTreeMap,
    fs,
    net::IpAddr,
    process::{Command, Stdio},
    time::Duration,
};

use protocol::{CrashKind, Request, Status};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    Session,
    observe::{menu_lines, shows_menu},
};
use crate::{
    agent::Agent,
    clock::poll,
    error::{Error, Result, bail},
    game_files::{self, WEBSOCKET_OPTIONS},
    inject,
    platform::{self, CRASH_HANDLER, Desktop, ProcessGroup, Window},
    screen_text,
};

/// The Hyprland workspace the game window opens on, without switching to it. Unset leaves it
/// where Hyprland puts new windows (the active workspace); other desktops report a note.
pub const WORKSPACE_SETTING: &str = "BEDROCK_MCP_WORKSPACE";
const DEFAULT_LAUNCH_TIMEOUT: Duration = Duration::from_secs(300);
/// Hidden-window frame rate while the game loads to its menu, when the desktop does not
/// report its monitors' refresh rate.
const LOADING_FPS: i64 = 60;
/// How long OCR gets to see the menu after the marker files report it.
const MENU_TEXT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long after the previous game exits the next launch waits. Our choice: a launch right
/// after an exit can fail.
const REOPEN_DELAY: Duration = Duration::from_secs(5);
const STOP_TIMEOUT: Duration = Duration::from_secs(60);
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(60);
const SERVER_STOP_GRACE: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(250);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchArgs {
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    size: Option<[u32; 2]>,
    timeout_ms: Option<u64>,
    pid: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalServerArgs {
    command: Vec<String>,
    port: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectArgs {
    address: String,
    #[serde(default)]
    allow_remote: bool,
    local_server: Option<LocalServerArgs>,
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrashArgs {
    kind: CrashKind,
}

/// `host:port` or `[v6]:port`.
pub fn parse_address(address: &str) -> Result<(String, u16)> {
    let invalid = || Error::msg(format!("Invalid address {address:?}; use host:port."));
    let (host, port) = address.rsplit_once(':').ok_or_else(invalid)?;
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let port = port
        .parse::<u16>()
        .ok()
        .filter(|&port| port != 0)
        .ok_or_else(invalid)?;
    if host.is_empty() {
        return Err(invalid());
    }
    Ok((host.to_owned(), port))
}

pub fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// The `connect` deep link, as
/// https://learn.microsoft.com/en-us/minecraft/creator/reference/content/deep-links documents it.
pub fn connect_link(host: &str, port: u16) -> String {
    format!("minecraft://connect/?serverUrl={host}&serverPort={port}")
}

/// A server process this session started, in its own process group.
pub struct LocalServer {
    process: ProcessGroup,
    port: u16,
    log: std::path::PathBuf,
}

impl LocalServer {
    fn stop(self, clock: &dyn crate::clock::Clock) -> Value {
        let pid = self.process.id();
        self.process.stop(clock, SERVER_STOP_GRACE);
        json!({ "pid": pid, "port": self.port, "stopped": true })
    }
}

/// Move, size and describe the game window once the agent is attached. What the desktop
/// cannot do becomes a note; the window is then described as it is.
fn arrange_window(
    desktop: &dyn Desktop,
    pid: u32,
    size: Option<[u32; 2]>,
    workspace: Option<&str>,
    status: &Status,
    notes: &mut Vec<String>,
) -> Value {
    if let Some(workspace) = workspace
        && let Err(error) = desktop.move_to_workspace(pid, workspace)
    {
        notes.push(format!(
            "{WORKSPACE_SETTING}={workspace} not applied: {error}"
        ));
    }
    let resized = size.and_then(|[width, height]| {
        desktop
            .resize(pid, width, height)
            .map_err(|error| notes.push(format!("size {width}x{height} not applied: {error}")))
            .ok()
    });
    let window = match resized {
        Some(window) => Ok(window),
        None => desktop
            .window(pid)
            .map(|window| window.unwrap_or_else(|| Window::from_agent(status, desktop.name()))),
    };
    if let Err(error) = desktop.keep_rendering(pid) {
        notes.push(format!(
            "The game will slow to about one frame a second while its window is hidden: {error}"
        ));
    }
    window.map_or_else(
        |error| json!({ "error": error.to_string() }),
        |window| window.to_json(),
    )
}

impl Session {
    pub(crate) fn launch_client(&mut self, args: LaunchArgs) -> Result<Value> {
        let env: Vec<(String, String)> = args.env.into_iter().collect();
        let timeout = args
            .timeout_ms
            .map_or(DEFAULT_LAUNCH_TIMEOUT, Duration::from_millis);
        self.install = self.install.select(args.pid)?;
        self.launch_game(&args.args, &env, args.size, timeout)
    }

    /// Launch the game unless it runs, wait for the main menu, attach the agent.
    fn launch_game(
        &mut self,
        args: &[String],
        env: &[(String, String)],
        size: Option<[u32; 2]>,
        timeout: Duration,
    ) -> Result<Value> {
        let mut notes = Vec::new();
        let workspace = std::env::var(WORKSPACE_SETTING)
            .ok()
            .map(|workspace| workspace.trim().to_owned())
            .filter(|workspace| !workspace.is_empty());
        let already_running = self.install.game_pid().is_some();
        // Menu text OCR read while the game loaded, if it saw the menu first.
        let mut menu_text: Option<Vec<String>> = None;
        if already_running {
            if !args.is_empty() || !env.is_empty() {
                notes.push("Minecraft was already running, so args and env were not used; quit first, or use connect to restart it with a server link.".to_owned());
            }
        } else {
            match game_files::options_path(&self.install.prefix) {
                Ok(path) => game_files::write_options(&path, &WEBSOCKET_OPTIONS)?,
                Err(_) => notes.push("options.txt does not exist yet, so WebSockets could not be enabled; quit and launch again after this first start.".to_owned()),
            }
            // Wait REOPEN_DELAY after the previous game exited. The code takes the newest log's
            // last write as the time that game exited.
            let last_write = game_files::newest_log(&self.install.prefix)
                .and_then(|log| std::fs::metadata(log).ok()?.modified().ok());
            let since_exit = last_write
                .and_then(|written| self.clock.wall().duration_since(written).ok())
                .unwrap_or(REOPEN_DELAY);
            if let Some(remaining) = REOPEN_DELAY.checked_sub(since_exit)
                && !remaining.is_zero()
            {
                self.clock.sleep(remaining);
                notes.push(format!(
                    "Waited {:.1} s first: Minecraft exited moments ago, and a launch right \
                     after an exit can fail.",
                    remaining.as_secs_f64()
                ));
            }
            let since = self.clock.wall() - Duration::from_secs(2);
            if let Some(workspace) = &workspace
                && let Err(error) = platform::desktop().open_on_workspace(workspace)
            {
                notes.push(format!(
                    "{WORKSPACE_SETTING}={workspace} not applied: {error}"
                ));
            }
            // The game writes its main-menu marker files once the menu is up; OCR of the menu
            // can see it sooner.
            self.install = self.install.launch(args, env, timeout, &*self.clock)?;
            let install = &self.install;
            let desktop = platform::desktop();
            // A window that opens on a hidden workspace renders about one frame a second, so
            // loading to the menu would crawl: keep it rendering as soon as it exists, at full
            // rate until the menu is up.
            let loading_fps = desktop.refresh_rate().unwrap_or(LOADING_FPS);
            let restore_fps = desktop.raise_hidden_fps(loading_fps).ok().flatten();
            let mut rendering = false;
            let capture = self
                .install
                .agent_dir()
                .join("screenshots")
                .join(format!(".launch-{}.png", std::process::id()));
            let mut fonts = None;
            let reached = poll(&*self.clock, timeout, POLL, || {
                if install.crash_handler_running() {
                    bail!("Minecraft crashed while starting ({CRASH_HANDLER} is running).");
                }
                let Some(pid) = install.game_pid() else {
                    return Ok(None);
                };
                let window = desktop.window(pid).ok().flatten();
                if !rendering && window.is_some() {
                    rendering = desktop.keep_rendering(pid).is_ok();
                }
                let marked = game_files::menu_reached(&install.prefix, since);
                // The menu can be drawn before its marker files are written: read the screen
                // too, where the desktop captures windows without the agent.
                if fonts.is_none() {
                    fonts = install
                        .game_exe(pid)
                        .and_then(|exe| screen_text::fonts(exe.parent()?).ok());
                }
                if let (Some(window), Some(fonts)) = (&window, &fonts)
                    && let Some(Ok(_)) = desktop.screenshot(window, &capture, None)
                    && let Ok(lines) = menu_lines(&capture, window, fonts)
                    && shows_menu(&lines)
                {
                    menu_text = Some(lines);
                    return Ok(Some(pid));
                }
                Ok(marked.then_some(pid))
            });
            let _ = fs::remove_file(&capture);
            if let Some(previous) = restore_fps {
                desktop.restore_hidden_fps(previous);
            }
            if reached?.is_none() {
                bail!(
                    "Minecraft did not reach its main menu within {} s. Details: {}",
                    timeout.as_secs(),
                    self.install.logs_dir().display()
                );
            }
        }
        self.agent = None;
        self.controller.forget();
        let mut agent = inject::attach(&self.install, &*self.clock)?;
        let status = agent.status()?;
        self.agent = Some(agent);
        let pid = self.running_pid()?;
        let window = arrange_window(
            platform::desktop(),
            pid,
            size,
            workspace.as_deref(),
            &status,
            &mut notes,
        );
        // The marker files say the menu was built; OCR checks it is drawn. The code takes text
        // below the top of the window as past the loading screen. The launch loop may have
        // read it.
        let menu_text = if already_running {
            Value::Null
        } else if let Some(lines) = menu_text {
            json!(lines)
        } else {
            match self.wait_for_menu_text(MENU_TEXT_TIMEOUT) {
                Ok(lines) => json!(lines),
                Err(why) => {
                    notes.push(format!("The menu was not confirmed on screen: {why}"));
                    Value::Null
                }
            }
        };
        let port = self.world().map(|world| world.socket().port()).ok();
        Ok(json!({
            "pid": pid,
            "already_running": already_running,
            "agent": status,
            "window": window,
            "menu_text": menu_text,
            "websocket_port": port,
            "notes": notes,
        }))
    }

    pub(crate) fn connect(&mut self, args: ConnectArgs) -> Result<Value> {
        let (host, port) = parse_address(&args.address)?;
        if !args.allow_remote && !is_loopback(&host) {
            bail!(
                "{} is not a loopback address; pass allow_remote: true to join it with the signed-in account.",
                args.address
            );
        }
        let server = match args.local_server {
            Some(local) => Some(self.start_local_server(local)?),
            None => None,
        };
        let restarted = self.install.game_pid().is_some();
        if restarted {
            self.stop_game()?;
        }
        let timeout = args
            .timeout_ms
            .map_or(DEFAULT_LAUNCH_TIMEOUT, Duration::from_millis);
        let link = connect_link(&host, port);
        let launched = self.launch_game(std::slice::from_ref(&link), &[], None, timeout)?;
        Ok(json!({
            "address": args.address,
            "link": link,
            "restarted": restarted,
            "note": "server_join starts the game with the link and stops a running game first; wait_until in_world to know when the join finished.",
            "local_server": server,
            "launch": launched,
        }))
    }

    fn start_local_server(&mut self, local: LocalServerArgs) -> Result<Value> {
        let Some((program, rest)) = local.command.split_first() else {
            bail!("local_server.command is empty.");
        };
        if let Some(previous) = self.local_server.take() {
            previous.stop(&*self.clock);
        }
        if platform::udp_port_bound(local.port) {
            bail!("UDP port {} is already in use.", local.port);
        }
        let log = self.install.agent_dir().join("local-server.log");
        fs::create_dir_all(self.install.agent_dir())?;
        let output = fs::File::create(&log)?;
        let mut command = Command::new(program);
        command
            .args(rest)
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output);
        let process = ProcessGroup::spawn(&mut command)
            .map_err(|error| Error::msg(format!("Cannot start {program}: {error}")))?;
        let mut server = LocalServer {
            process,
            port: local.port,
            log,
        };
        let bound = poll(&*self.clock, SERVER_START_TIMEOUT, POLL, || {
            if let Some(status) = server.process.try_wait()? {
                bail!(
                    "The local server exited ({status}). Details: {}",
                    server.log.display()
                );
            }
            Ok(platform::udp_port_bound(local.port).then_some(()))
        });
        match bound {
            Ok(Some(())) => {
                let report =
                    json!({ "pid": server.process.id(), "port": local.port, "log": server.log });
                self.local_server = Some(server);
                Ok(report)
            }
            Ok(None) => {
                server.stop(&*self.clock);
                bail!(
                    "The local server did not bind UDP port {} within {} s.",
                    local.port,
                    SERVER_START_TIMEOUT.as_secs()
                )
            }
            Err(error) => {
                server.stop(&*self.clock);
                Err(error)
            }
        }
    }

    /// Release what the session drives in the game and stop the game; waits for it to exit.
    fn stop_game(&mut self) -> Result<()> {
        self.release_input();
        self.release_camera();
        self.agent = None;
        self.install.stop()?;
        let install = &self.install;
        let stopped = poll(&*self.clock, STOP_TIMEOUT, POLL, || {
            Ok(install.game_pid().is_none().then_some(()))
        })?;
        if stopped.is_none() {
            bail!(
                "Minecraft did not exit within {} s of being asked to stop.",
                STOP_TIMEOUT.as_secs()
            );
        }
        Ok(())
    }

    pub(crate) fn crash(&mut self, args: CrashArgs) -> Result<Value> {
        self.with_agent(|agent, _| agent.send(Request::Crash { kind: args.kind }))?;
        self.agent = None;
        self.controller.forget();
        Ok(json!({
            "crashed": args.kind,
            "note": format!("The agent replied before crashing the game; check state for {CRASH_HANDLER}."),
        }))
    }

    pub(crate) fn quit(&mut self) -> Value {
        let mut report = self.release_session();
        report["game"] = if self.install.game_pid().is_some() {
            match self.install.stop() {
                Ok(output) => json!({ "stopping": true, "output": output }),
                Err(error) => json!({ "error": error.to_string() }),
            }
        } else {
            json!({ "stopping": false, "note": "Minecraft is not running." })
        };
        report
    }

    fn release_input(&mut self) -> bool {
        let released = match self.agent.as_mut() {
            Some(agent) => self.controller.release_all(agent).is_ok(),
            None => false,
        };
        self.controller.forget();
        released
    }

    fn release_camera(&mut self) -> Option<Value> {
        let camera = self.camera.take()?;
        let world = self.world().ok()?;
        Some(match camera.release(&world) {
            Ok(()) => json!({ "released": true }),
            Err(error) => json!({ "error": error.to_string() }),
        })
    }

    /// Release input and the camera, finish any recording and stop the local server.
    pub(super) fn release_session(&mut self) -> Value {
        let input_released = self.release_input();
        let camera = self.release_camera();
        let recording = self.recording.take().map(|recording| {
            self.finish_recording(recording)
                .unwrap_or_else(|error| json!({ "error": error.to_string() }))
        });
        let server = self
            .local_server
            .take()
            .map(|server| server.stop(&*self.clock));
        self.agent = None;
        json!({ "input_released": input_released, "camera": camera, "recording": recording, "local_server": server })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn status() -> Status {
        Status {
            protocol: protocol::PROTOCOL_VERSION,
            pid: 7,
            width: 1280,
            height: 720,
            foreground: false,
            game_active: true,
            mouse_readings: 0,
            gameinput_versions: Vec::new(),
            pointer: [0, 0],
            pointer_override: false,
            input_locked: false,
            takeovers: 0,
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_desktop_without_window_control_turns_size_and_workspace_into_notes() {
        let mut notes = Vec::new();
        let window = arrange_window(
            platform::generic_desktop(),
            7,
            Some([800, 600]),
            Some("3"),
            &status(),
            &mut notes,
        );
        assert_eq!(window["size"], json!([1280, 720]));
        assert_eq!(window["scale"], 1.0);
        assert_eq!(window["workspace"], Value::Null);
        assert_eq!(window["desktop"], "generic");
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes[0].starts_with(&format!("{WORKSPACE_SETTING}=3 not applied")));
        assert!(notes[1].starts_with("size 800x600 not applied"));
        assert!(
            notes
                .iter()
                .all(|note| note.contains("not supported on this desktop"))
        );
        notes.clear();
        let window = arrange_window(
            platform::generic_desktop(),
            7,
            None,
            None,
            &status(),
            &mut notes,
        );
        assert!(notes.is_empty());
        assert_eq!(window["physical_size"], json!([1280, 720]));
    }

    #[test]
    fn addresses_parse_with_ipv6_brackets() {
        assert_eq!(
            parse_address("127.0.0.1:19132").unwrap(),
            ("127.0.0.1".into(), 19132)
        );
        assert_eq!(parse_address("[::1]:19133").unwrap(), ("::1".into(), 19133));
        assert_eq!(
            parse_address("play.example.net:19132").unwrap().0,
            "play.example.net"
        );
        for bad in ["127.0.0.1", ":19132", "host:0", "host:99999", "host:x"] {
            assert!(parse_address(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn only_loopback_hosts_are_local() {
        for host in ["127.0.0.1", "127.8.9.10", "::1", "localhost", "LOCALHOST"] {
            assert!(is_loopback(host), "{host}");
        }
        for host in [
            "10.0.0.1",
            "0.0.0.0",
            "example.com",
            "::",
            "127.0.0.1.example.com",
        ] {
            assert!(!is_loopback(host), "{host}");
        }
    }

    #[test]
    fn connect_links_name_host_and_port() {
        assert_eq!(
            connect_link("127.0.0.1", 19132),
            "minecraft://connect/?serverUrl=127.0.0.1&serverPort=19132"
        );
    }
}
