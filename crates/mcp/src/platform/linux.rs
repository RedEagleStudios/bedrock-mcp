//! Linux: the game runs under Wine or Proton, started by any launcher. The server finds it by
//! its process and derives the prefix, Wine and game folder from it; mcbe-gdk-linux is the
//! configured install it falls back to and launches. The window belongs to the desktop
//! backend in [`super::desktop`].

use std::{
    env,
    ffi::{OsStr, OsString},
    fs::{self, File},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{ffi::OsStringExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant, SystemTime},
};

use serde_json::Value;

use super::VideoEncoder;
use crate::{
    clock::{Clock, poll},
    error::{Error, Result, bail},
    game_files,
    install::{GAME_EXE, Install, windows_artifact},
    recorder::ffmpeg,
};

const LAUNCHER: &str = "mcbe-gdk-linux";
/// A shell command that starts the game; it replaces mcbe-gdk-linux for game_launch.
pub const LAUNCH_COMMAND: &str = "BEDROCK_MCP_LAUNCH_COMMAND";
const DEBUGGERS: [&str; 2] = ["winedbg", "winedbg.exe"];
/// What [`Install::crash_handler_running`] looks for, for messages.
pub const CRASH_HANDLER: &str = "Wine's crash debugger";
/// How to build the Windows artifacts the server injects.
pub const BUILD_HINT: &str = "build it with `cargo build --release --target x86_64-pc-windows-gnu -p bedrock-agent -p bedrock-unstable -p bedrock-inject` (or scripts/build-windows.sh)";
/// Where the Windows build outputs sit relative to this executable's directory.
pub const ARTIFACT_DIRS: &[&str] = &["../x86_64-pc-windows-gnu/release"];

const INJECTOR_NAME: &str = "bedrock-inject.exe";
const INJECTOR_TIMEOUT: Duration = Duration::from_secs(60);
const SYNC_VARIABLES: [&str; 3] = ["WINEFSYNC", "WINEESYNC", "WINENTSYNC"];
const ENGINE_LIBRARY_DIRS: [&str; 2] = ["x86_64-linux-gnu", "i386-linux-gnu"];
/// Folders inside a prefix whose DLLs belong to Windows or installed runtimes, not mods.
const PREFIX_SYSTEM_DIRS: [&str; 3] = [
    "drive_c/windows",
    "drive_c/Program Files",
    "drive_c/Program Files (x86)",
];

/// ffmpeg's input pipe size; Linux allows unprivileged pipes up to 1 MiB by default.
const PIPE_BYTES: i32 = 1 << 20;
const RENDER_NODES: &str = "/dev/dri";

/// A running Minecraft process and the Wine prefix it runs in (canonical).
#[derive(Clone, Debug, PartialEq)]
struct RunningGame {
    pid: u32,
    prefix: PathBuf,
}

/// Every Wine process whose program (argv[0]) is Minecraft.Windows.exe, by pid.
fn running_games() -> Vec<RunningGame> {
    let own = std::process::id();
    let mut games: Vec<RunningGame> = proc_pids()
        .filter(|&pid| pid != own)
        .filter(|&pid| {
            argv(pid)
                .first()
                .is_some_and(|program| basename(program) == GAME_EXE)
        })
        .map(|pid| RunningGame {
            pid,
            prefix: wine_prefix(&environ(pid)),
        })
        .collect();
    games.sort_by_key(|game| game.pid);
    games
}

fn describe(games: &[RunningGame]) -> String {
    if games.is_empty() {
        return "no Minecraft process runs".into();
    }
    let list: Vec<String> = games
        .iter()
        .map(|game| format!("pid {} (prefix {})", game.pid, game.prefix.display()))
        .collect();
    format!("Minecraft processes: {}", list.join(", "))
}

/// The process's Wine prefix: `WINEPREFIX` (Wine's default `~/.wine` without it), with
/// symlinks such as Proton's `pfx/pfx` and trailing slashes resolved.
fn wine_prefix(env: &[(String, OsString)]) -> PathBuf {
    let prefix = lookup(env, "WINEPREFIX")
        .map(PathBuf::from)
        .or_else(|| lookup(env, "HOME").map(|home| Path::new(home).join(".wine")))
        .unwrap_or_default();
    canonical(&prefix)
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// A non-empty variable of a process environment.
fn lookup<'a>(env: &'a [(String, OsString)], name: &str) -> Option<&'a OsStr> {
    env.iter()
        .find(|(key, value)| key == name && !value.is_empty())
        .map(|(_, value)| value.as_os_str())
}

fn var(name: &str) -> Option<OsString> {
    env::var_os(name).filter(|value| !value.is_empty())
}

/// `${XDG_DATA_HOME:-~/.local/share}`.
fn data_home() -> PathBuf {
    var("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(var("HOME").unwrap_or_else(|| "/".into())).join(".local/share")
    })
}

/// This server's folder for a game in `prefix` that no configured install owns: the prefix
/// path with `/` turned into `-`, under `<data home>/bedrock-mcp/games`.
fn discovered_profile(data_home: &Path, prefix: &Path) -> PathBuf {
    let key: String = prefix
        .to_string_lossy()
        .trim_matches('/')
        .chars()
        .map(|c| if c == '/' { '-' } else { c })
        .collect();
    data_home.join("bedrock-mcp/games").join(key)
}

/// The profile whose `agent` folder holds the agent DLL mapped in `/proc/<pid>/maps`, so a
/// game that already has the agent keeps using its endpoint instead of loading a second copy.
fn mapped_agent_profile(maps: &str) -> Option<PathBuf> {
    let dll = mapped_paths(maps).find(|path| {
        path.file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case(protocol::DLL_NAME))
    })?;
    let dir = dll.parent()?;
    (dir.file_name()? == "agent").then(|| dir.parent().map(Path::to_path_buf))?
}

/// Whether an executable `name` is on `PATH`.
fn on_path(name: &str) -> bool {
    var("PATH").is_some_and(|path| env::split_paths(&path).any(|dir| dir.join(name).is_file()))
}

/// `profile` is this server's folder for the game, `prefix` its Wine prefix. The configured
/// install is mcbe-gdk-linux's: root `R`, profile `R/profile`, prefix
/// `R/profile/compatdata/pfx`.
impl Install {
    /// The game that runs, when exactly one does or one runs in the configured install's
    /// prefix; else the configured install.
    pub fn from_env() -> Self {
        let configured = Self::configured();
        if configured.game_pid().is_some() {
            return configured;
        }
        match running_games().as_slice() {
            [game] => Self::for_game(game),
            _ => configured,
        }
    }

    /// mcbe-gdk-linux's install: `$MCBE_GDK_ROOT`, else
    /// `${XDG_DATA_HOME:-~/.local/share}/mcbe-gdk-linux`; `$BOL_WINEPREFIX` overrides the
    /// prefix.
    fn configured() -> Self {
        let root = var("MCBE_GDK_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| data_home().join(LAUNCHER));
        let profile = root.join("profile");
        let prefix = var("BOL_WINEPREFIX")
            .map(PathBuf::from)
            .unwrap_or_else(|| profile.join("compatdata/pfx"));
        Self { profile, prefix }
    }

    /// The install of a running game: the configured one when the game runs in its prefix,
    /// else the game's prefix with the profile whose agent the game already loaded, or this
    /// server's own folder for that prefix.
    fn for_game(game: &RunningGame) -> Self {
        let configured = Self::configured();
        if canonical(&configured.prefix) == game.prefix {
            return configured;
        }
        let maps = fs::read_to_string(format!("/proc/{}/maps", game.pid)).unwrap_or_default();
        let profile = mapped_agent_profile(&maps)
            .unwrap_or_else(|| discovered_profile(&data_home(), &game.prefix));
        Self {
            profile,
            prefix: game.prefix.clone(),
        }
    }

    /// The install game_launch drives: game `pid` when given; else this one while its game
    /// runs, the only running game, or, with none running, the install the next launch
    /// starts. Fails when several games run and `pid` does not pick one.
    pub fn select(&self, pid: Option<u32>) -> Result<Self> {
        let games = running_games();
        let own = canonical(&self.prefix);
        let follow = |game: &RunningGame| {
            if game.prefix == own {
                self.clone()
            } else {
                Self::for_game(game)
            }
        };
        if let Some(pid) = pid {
            let game = games.iter().find(|game| game.pid == pid).ok_or_else(|| {
                Error::msg(format!(
                    "Process {pid} is not a running Minecraft; {}.",
                    describe(&games)
                ))
            })?;
            return Ok(follow(game));
        }
        match games.as_slice() {
            [] if var(LAUNCH_COMMAND).is_some() => Ok(self.clone()),
            [] => Ok(Self::configured()),
            [game] => Ok(follow(game)),
            _ => bail!(
                "Several games run; pass pid to choose one. {}.",
                describe(&games)
            ),
        }
    }

    /// Where a failed start leaves details: the launcher's `profile/logs` when it has one,
    /// else the game's own log folder.
    pub fn logs_dir(&self) -> PathBuf {
        let launcher_logs = self.profile.join("logs");
        if launcher_logs.is_dir() {
            return launcher_logs;
        }
        game_files::data_roots(&self.prefix)
            .into_iter()
            .next()
            .map_or(launcher_logs, |root| root.join("logs"))
    }

    /// Whether this is mcbe-gdk-linux's install and its command is available.
    fn launcher_owned(&self) -> bool {
        canonical(&self.prefix) == canonical(&Self::configured().prefix) && on_path(LAUNCHER)
    }

    /// The Proton engine directory from mcbe-gdk-linux's `profile/settings.json`.
    fn configured_engine(&self) -> Result<PathBuf> {
        let path = self.profile.join("settings.json");
        let settings: Value = fs::read(&path)
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
            .ok_or_else(|| {
                Error::msg(format!(
                    "Cannot tell which Wine runs the game: its process names none and {} is unreadable.",
                    path.display()
                ))
            })?;
        match settings.get("proton").and_then(|value| value.as_str()) {
            Some(engine) if !engine.is_empty() => Ok(PathBuf::from(engine)),
            _ => bail!(
                "{} names no compatibility engine; run 'mcbe-gdk-linux update'.",
                path.display()
            ),
        }
    }

    /// The Wine that runs game `pid`, from its process, else mcbe-gdk-linux's engine.
    fn wine(&self, pid: u32, env: &[(String, OsString)]) -> Result<PathBuf> {
        let exe = fs::read_link(format!("/proc/{pid}/exe")).ok();
        if let Some(wine) = game_wine(env, exe.as_deref()) {
            return Ok(wine);
        }
        let wine = self.configured_engine()?.join("files/bin/wine");
        if !wine.is_file() {
            bail!(
                "The compatibility engine is missing ({}); run 'mcbe-gdk-linux update'.",
                wine.display()
            );
        }
        Ok(wine)
    }

    /// PIDs whose `WINEPREFIX` resolves to this prefix.
    fn prefix_pids(&self) -> Vec<u32> {
        let Ok(target) = fs::canonicalize(&self.prefix) else {
            return Vec::new();
        };
        let own = std::process::id();
        let mut found: Vec<u32> = proc_pids()
            .filter(|&pid| pid != own)
            .filter(|&pid| {
                lookup(&environ(pid), "WINEPREFIX")
                    .and_then(|value| fs::canonicalize(value).ok())
                    .is_some_and(|real| real == target)
            })
            .collect();
        found.sort_unstable();
        found
    }

    /// The game process in this prefix.
    pub fn game_pid(&self) -> Option<u32> {
        let own = canonical(&self.prefix);
        running_games()
            .into_iter()
            .find(|game| game.prefix == own)
            .map(|game| game.pid)
    }

    /// Whether Wine's crash debugger runs in the prefix.
    pub fn crash_handler_running(&self) -> bool {
        self.prefix_pids().into_iter().any(|pid| {
            argv(pid)
                .iter()
                .any(|arg| DEBUGGERS.contains(&basename(arg).as_str()))
        })
    }

    /// The Unix path of the game executable of `pid`: its Windows argv[0] through the
    /// prefix's `dosdevices`, else the image Wine mapped (from `/proc/<pid>/maps`), else the
    /// executable in its working directory.
    pub fn game_exe(&self, pid: u32) -> Option<PathBuf> {
        let program = argv(pid).into_iter().next()?.to_string_lossy().into_owned();
        windows_to_unix(&self.prefix, &program)
            .filter(|path| path.is_file())
            .or_else(|| {
                let maps = fs::read_to_string(format!("/proc/{pid}/maps")).ok()?;
                mapped_exe(&maps)
            })
            .or_else(|| {
                let cwd = fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
                fs::read_dir(cwd)
                    .ok()?
                    .flatten()
                    .map(|entry| entry.path())
                    .find(|path| {
                        path.file_name()
                            .is_some_and(|name| name.to_string_lossy().to_lowercase() == GAME_EXE)
                    })
            })
    }

    /// File names of DLLs mapped in game `pid` that come from neither the game, Wine, the
    /// prefix's Windows and program folders, nor this server: other mods, which may hook
    /// the same functions as the agent.
    pub fn other_mods(&self, pid: u32) -> Vec<String> {
        let Ok(maps) = fs::read_to_string(format!("/proc/{pid}/maps")) else {
            return Vec::new();
        };
        let env = environ(pid);
        let mut known: Vec<PathBuf> = PREFIX_SYSTEM_DIRS
            .iter()
            .map(|dir| self.prefix.join(dir))
            .collect();
        known.push(self.agent_dir());
        known.extend(
            self.game_exe(pid)
                .and_then(|exe| exe.parent().map(Path::to_path_buf)),
        );
        let exe = fs::read_link(format!("/proc/{pid}/exe")).ok();
        known.extend(game_wine(&env, exe.as_deref()).and_then(|wine| wine_root(&wine)));
        if let Some(paths) = lookup(&env, "WINEDLLPATH") {
            known.extend(env::split_paths(paths));
        }
        let known: Vec<PathBuf> = known.iter().map(|dir| canonical(dir)).collect();
        foreign_dlls(&maps, &known)
    }

    /// Start the game detached: `$BEDROCK_MCP_LAUNCH_COMMAND` through `sh -c` (with `args`
    /// as `$@`), else `mcbe-gdk-linux launch [args...]`. Returns the install to drive: for a
    /// launch command, the game it started, once its process appears within `timeout`.
    pub fn launch(
        &self,
        args: &[String],
        extra_env: &[(String, String)],
        timeout: Duration,
        clock: &dyn Clock,
    ) -> Result<Self> {
        let custom = var(LAUNCH_COMMAND);
        let (mut command, name, log) = if let Some(shell) = &custom {
            let log = data_home().join("bedrock-mcp/launch-command.log");
            let mut command = Command::new("sh");
            command.arg("-c").arg(shell).arg("sh").args(args);
            (command, LAUNCH_COMMAND.to_owned(), Some(log))
        } else if self.launcher_owned() {
            let mut command = Command::new(LAUNCHER);
            command.arg("launch").args(args);
            (command, format!("{LAUNCHER} launch"), None)
        } else {
            bail!(
                "Minecraft is not running and this server cannot start it: set {LAUNCH_COMMAND} \
                 to a shell command that starts it, or install {LAUNCHER}. Or start the game \
                 with your launcher and call game_launch again to attach."
            );
        };
        let (output, errors) = match &log {
            Some(log) => {
                fs::create_dir_all(log.parent().unwrap_or(Path::new("/")))?;
                let file = File::create(log)?;
                (Stdio::from(file.try_clone()?), Stdio::from(file))
            }
            None => (Stdio::null(), Stdio::null()),
        };
        command
            .envs(extra_env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(output)
            .stderr(errors);
        detach(&mut command);
        command
            .spawn()
            .map_err(|error| Error::msg(format!("Cannot run {name}: {error}")))?;
        let Some(log) = log else {
            return Ok(Self::configured());
        };
        let started = poll(clock, timeout, Duration::from_millis(250), || {
            Ok(running_games().into_iter().next())
        })?;
        let game = started.ok_or_else(|| {
            Error::msg(format!(
                "{LAUNCH_COMMAND} started no Minecraft process within {} s. Its output: {}",
                timeout.as_secs(),
                log.display()
            ))
        })?;
        Ok(if canonical(&self.prefix) == game.prefix {
            self.clone()
        } else {
            Self::for_game(&game)
        })
    }

    /// Stop the game: `mcbe-gdk-linux stop` for its install, else SIGTERM to the game
    /// process, which Wine turns into a process exit.
    pub fn stop(&self) -> Result<String> {
        if self.launcher_owned() {
            let output = Command::new(LAUNCHER)
                .arg("stop")
                .stdin(Stdio::null())
                .output()
                .map_err(|error| Error::msg(format!("Cannot run {LAUNCHER} stop: {error}")))?;
            let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if !output.status.success() {
                bail!(
                    "{LAUNCHER} stop failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            return Ok(text);
        }
        let Some(pid) = self.game_pid() else {
            return Ok("Minecraft is not running.".into());
        };
        let signalled = i32::try_from(pid)
            // SAFETY: kill has no memory preconditions.
            .is_ok_and(|pid| unsafe { libc::kill(pid, libc::SIGTERM) } == 0);
        if !signalled {
            bail!(
                "Cannot stop Minecraft (pid {pid}): {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(format!("Sent SIGTERM to Minecraft (pid {pid})."))
    }
}

/// The folders that may hold `Minecraft Bedrock` data folders: every Wine user's roaming
/// AppData in the prefix.
pub fn roaming_dirs(prefix: &Path) -> Vec<PathBuf> {
    let mut homes: Vec<PathBuf> = fs::read_dir(prefix.join("drive_c/users"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    homes.sort();
    homes
        .into_iter()
        .map(|home| home.join("AppData/Roaming"))
        .collect()
}

/// Run `command` in a new session so it outlives this server and its terminal.
fn detach(command: &mut Command) {
    // SAFETY: setsid is async-signal-safe and touches no memory of the parent.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn proc_pids() -> impl Iterator<Item = u32> {
    fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
}

fn argv(pid: u32) -> Vec<OsString> {
    fs::read(format!("/proc/{pid}/cmdline"))
        .map(|raw| {
            raw.split(|&byte| byte == 0)
                .filter(|part| !part.is_empty())
                .map(|part| OsString::from_vec(part.to_vec()))
                .collect()
        })
        .unwrap_or_default()
}

/// `/proc/<pid>/environ` as pairs.
fn environ(pid: u32) -> Vec<(String, OsString)> {
    fs::read(format!("/proc/{pid}/environ"))
        .map(|raw| parse_environ(&raw))
        .unwrap_or_default()
}

fn parse_environ(raw: &[u8]) -> Vec<(String, OsString)> {
    raw.split(|&byte| byte == 0)
        .filter_map(|item| {
            let split = item.iter().position(|&byte| byte == b'=')?;
            let key = String::from_utf8(item[..split].to_vec()).ok()?;
            Some((key, OsString::from_vec(item[split + 1..].to_vec())))
        })
        .collect()
}

/// Lower-cased last component of a Unix or Windows path.
fn basename(argument: &OsString) -> String {
    let text = argument.to_string_lossy();
    let trimmed = text.trim_end_matches(['/', '\\']);
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .to_lowercase()
}

/// `C:\a\b.exe` as `<prefix>/dosdevices/c:/a/b.exe`; Unix paths stay as they are.
fn windows_to_unix(prefix: &Path, program: &str) -> Option<PathBuf> {
    if program.starts_with('/') {
        return Some(PathBuf::from(program));
    }
    let (drive, rest) = program.split_once(':')?;
    if drive.len() != 1 {
        return None;
    }
    let rest = rest.replace('\\', "/");
    Some(
        prefix
            .join("dosdevices")
            .join(format!("{}:", drive.to_ascii_lowercase()))
            .join(rest.trim_start_matches('/')),
    )
}

/// The file paths mapped in a `/proc/<pid>/maps` listing; paths may contain spaces.
fn mapped_paths(maps: &str) -> impl Iterator<Item = PathBuf> + '_ {
    maps.lines().filter_map(|line| {
        let mut rest = line;
        for _ in 0..5 {
            rest = rest.trim_start();
            rest = &rest[rest.find(' ')?..];
        }
        let path = rest.trim();
        path.starts_with('/')
            .then(|| PathBuf::from(path.trim_end_matches(" (deleted)")))
    })
}

/// The mapped file in `/proc/<pid>/maps` whose name is the game executable.
fn mapped_exe(maps: &str) -> Option<PathBuf> {
    mapped_paths(maps).find(|path| basename(&path.clone().into_os_string()) == GAME_EXE)
}

/// File names of the DLLs mapped in `maps` outside the `known` folders, sorted, once each.
fn foreign_dlls(maps: &str, known: &[PathBuf]) -> Vec<String> {
    let mut names: Vec<String> = mapped_paths(maps)
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("dll"))
        })
        .filter(|path| !known.iter().any(|dir| path.starts_with(dir)))
        .filter_map(|path| Some(path.file_name()?.to_string_lossy().into_owned()))
        .collect();
    names.sort_by_key(|name| name.to_lowercase());
    names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    names
}

/// The `wine` that runs a game process: Proton's `$PROTONPATH/files/bin/wine`, else the
/// `wine` in a `bin` folder above the process image (Wine's preloader) or next to it, else
/// `$WINELOADER`.
fn game_wine(env: &[(String, OsString)], exe: Option<&Path>) -> Option<PathBuf> {
    let proton = lookup(env, "PROTONPATH").map(|dir| Path::new(dir).join("files/bin/wine"));
    let near_image = exe.and_then(|exe| {
        exe.ancestors()
            .skip(1)
            .map(|dir| dir.join("bin/wine"))
            .chain(exe.parent().map(|dir| dir.join("wine")))
            .find(|wine| wine.is_file())
    });
    let loader = lookup(env, "WINELOADER").map(PathBuf::from);
    proton
        .filter(|wine| wine.is_file())
        .or(near_image)
        .or(loader.filter(|wine| wine.is_file()))
}

/// The Wine installation `wine` belongs to: the folder above its `bin`, or its own folder
/// for a loader outside `bin`.
fn wine_root(wine: &Path) -> Option<PathBuf> {
    let dir = wine.parent()?;
    Some(if dir.file_name()? == "bin" {
        dir.parent()?.to_path_buf()
    } else {
        dir.to_path_buf()
    })
}

pub fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// When process `pid` started, from `/proc/<pid>/stat` and the boot time.
pub fn started_at(pid: u32) -> Option<SystemTime> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesised command name; starttime is field 22 overall.
    let rest = &stat[stat.rfind(')')? + 2..];
    let ticks: u64 = rest.split(' ').nth(19)?.parse().ok()?;
    let boot: u64 = fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("btime ")?.trim().parse().ok())?;
    // SAFETY: sysconf has no preconditions.
    let hertz = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) })
        .ok()
        .filter(|&hz| hz > 0)?;
    Some(
        SystemTime::UNIX_EPOCH
            + Duration::from_secs(boot)
            + Duration::from_millis(ticks * 1000 / hertz),
    )
}

/// Environment for the game's wine to join the game's wineserver: the inherited
/// environment, the prefix, quiet Wine debug output, the game's sync mode, and the Wine
/// installation's libraries (under `root`) first on `LD_LIBRARY_PATH`.
fn injector_environment(
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
    root: &Path,
    prefix: &Path,
    game: &[(String, OsString)],
) -> Vec<(OsString, OsString)> {
    let replaced = |key: &OsString| {
        SYNC_VARIABLES.iter().any(|name| key == name)
            || key == "WINEPREFIX"
            || key == "LD_LIBRARY_PATH"
    };
    let mut existing_libraries = None;
    let mut env: Vec<(OsString, OsString)> = Vec::new();
    for (key, value) in inherited {
        if key == "LD_LIBRARY_PATH" && !value.is_empty() {
            existing_libraries = Some(value.clone());
        }
        if !replaced(&key) {
            env.push((key, value));
        }
    }
    env.push(("WINEPREFIX".into(), prefix.into()));
    for name in SYNC_VARIABLES {
        if let Some((_, value)) = game.iter().find(|(key, _)| key == name) {
            env.push((name.into(), value.clone()));
        }
    }
    let mut libraries: Vec<OsString> = ENGINE_LIBRARY_DIRS
        .iter()
        .map(|dir| root.join("lib").join(dir).into())
        .collect();
    libraries.extend(existing_libraries);
    env.push((
        "LD_LIBRARY_PATH".into(),
        libraries.join(std::ffi::OsStr::new(":")),
    ));
    if !env.iter().any(|(key, _)| key == "WINEDEBUG") {
        env.push(("WINEDEBUG".into(), "-all".into()));
    }
    env
}

/// The path the agent in the game opens for `path`: `Z:\...`, as Wine maps the root
/// filesystem.
pub fn agent_path(path: &Path) -> String {
    format!("Z:{}", path.display().to_string().replace('/', "\\"))
}

fn injector_failure(code: Option<i32>) -> String {
    match code {
        Some(2) => "the injector was called with bad arguments".into(),
        Some(3) => "Minecraft.Windows.exe was not found in the Wine prefix".into(),
        Some(4) => "Wine refused to open the Minecraft process".into(),
        Some(5) => "the DLL path could not be written into Minecraft".into(),
        Some(6) => "the remote loader thread could not be started".into(),
        Some(7) => {
            "LoadLibrary returned 0; check that the DLL is 64-bit and its dependencies exist".into()
        }
        Some(8) => "loading the DLL timed out".into(),
        Some(code) => format!("the injector failed with code {code}"),
        None => "the injector was killed by a signal".into(),
    }
}

/// Load the staged `dll` into the game by running `bedrock-inject.exe` under the game's Wine.
pub fn inject(install: &Install, pid: u32, dll: &Path, clock: &dyn Clock) -> Result<()> {
    let game = environ(pid);
    let wine = install.wine(pid, &game)?;
    let root = wine_root(&wine).unwrap_or_default();
    let injector = windows_artifact(INJECTOR_NAME)?;
    let env = injector_environment(std::env::vars_os(), &root, &install.prefix, &game);
    let log_path = install.agent_dir().join("injector.log");
    let log = File::create(&log_path)?;
    let mut child = Command::new(&wine)
        .arg(&injector)
        .arg(agent_path(dll))
        .arg("Minecraft.Windows.exe")
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()
        .map_err(|error| Error::msg(format!("Cannot run {}: {error}", wine.display())))?;
    let status = poll(clock, INJECTOR_TIMEOUT, Duration::from_millis(100), || {
        Ok(child.try_wait()?)
    })?;
    let Some(status) = status else {
        let _ = child.kill();
        let _ = child.wait();
        bail!(
            "The injector did not finish within 60 seconds. Details: {}",
            log_path.display()
        );
    };
    if !status.success() {
        let output = fs::read_to_string(&log_path).unwrap_or_default();
        let name = dll.file_name().unwrap_or_default().to_string_lossy();
        bail!(
            "Could not inject {name}: {}. {}Details: {}",
            injector_failure(status.code()),
            output
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .map(|line| format!("{}. ", line.trim()))
                .unwrap_or_default(),
            log_path.display()
        );
    }
    Ok(())
}

/// The shared-memory file the agent copies frames of game `pid` into; RAM-backed.
pub fn frame_file(pid: u32, stamp_ms: u128) -> PathBuf {
    PathBuf::from(format!("/dev/shm/bedrock-frames-{pid}-{stamp_ms}"))
}

/// Local ports with a bound socket in a `/proc/net/udp{,6}` table.
fn udp_ports(table: &str) -> Vec<u16> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let local = line.split_whitespace().nth(1)?;
            u16::from_str_radix(local.rsplit_once(':')?.1, 16).ok()
        })
        .collect()
}

/// Whether any socket has UDP port `port` bound.
pub fn udp_port_bound(port: u16) -> bool {
    ["/proc/net/udp", "/proc/net/udp6"]
        .iter()
        .any(|path| fs::read_to_string(path).is_ok_and(|table| udp_ports(&table).contains(&port)))
}

/// A child process in its own session (process group), stopped as a group.
pub struct ProcessGroup {
    child: Child,
}

impl ProcessGroup {
    pub fn spawn(command: &mut Command) -> std::io::Result<Self> {
        detach(command);
        Ok(Self {
            child: command.spawn()?,
        })
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }

    pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// SIGTERM the group, then SIGKILL it when the leader has not exited after `grace`.
    pub fn stop(mut self, clock: &dyn Clock, grace: Duration) {
        let pid = self.child.id();
        if let Ok(group) = i32::try_from(pid) {
            // SAFETY: kill has no memory preconditions; the group is our child's session.
            unsafe { libc::kill(-group, libc::SIGTERM) };
        }
        let exited = poll(clock, grace, Duration::from_millis(100), || {
            Ok(self.child.try_wait()?)
        });
        if !matches!(exited, Ok(Some(_))) {
            if let Ok(group) = i32::try_from(pid) {
                // SAFETY: as above.
                unsafe { libc::kill(-group, libc::SIGKILL) };
            }
            let _ = self.child.wait();
        }
    }
}

/// The first GPU render node, which VAAPI encodes on.
fn render_node() -> Result<PathBuf> {
    let mut nodes: Vec<PathBuf> = fs::read_dir(RENDER_NODES)
        .map_err(|error| Error::msg(format!("Cannot list {RENDER_NODES}: {error}")))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("renderD"))
        })
        .collect();
    nodes.sort();
    nodes
        .into_iter()
        .next()
        .ok_or_else(|| Error::msg("No GPU render node in /dev/dri; recordings encode with VAAPI."))
}

/// VAAPI on the first render node.
pub fn video_encoder(codec: &str) -> Result<VideoEncoder> {
    vaapi(&render_node()?, codec)
}

fn vaapi(device: &Path, codec: &str) -> Result<VideoEncoder> {
    let encoder = match codec {
        "h264" => "h264_vaapi",
        "hevc" => "hevc_vaapi",
        other => bail!("codec must be h264 or hevc, not {other}."),
    };
    Ok(VideoEncoder {
        global: vec![
            "-vaapi_device".into(),
            device.to_string_lossy().into_owned(),
        ],
        upload: "hwupload,scale_vaapi=format=nv12".into(),
        codec: ["-c:v", encoder, "-qp", "18"].map(str::to_owned).into(),
    })
}

/// Spawn `command` writing into its stdin through a 1 MiB pipe; the default 64 KiB buffer
/// splits each frame into hundreds of writes.
pub fn spawn_piped(command: &mut Command) -> std::io::Result<(Child, File)> {
    let mut child = command.stdin(Stdio::piped()).spawn()?;
    let stdin = child.stdin.take().expect("piped above");
    // SAFETY: fcntl on a pipe descriptor this process owns.
    unsafe { libc::fcntl(stdin.as_raw_fd(), libc::F_SETPIPE_SZ, PIPE_BYTES) };
    Ok((child, File::from(OwnedFd::from(stdin))))
}

/// The default output's audio, recorded by its own ffmpeg from PulseAudio's monitor source.
/// It runs apart from the video: a live audio input in the same process stalls the video
/// pipe.
pub struct AudioCapture {
    child: Child,
    path: PathBuf,
    started: Instant,
}

impl AudioCapture {
    /// How the mux copies this audio into the MP4: it is AAC already.
    pub const MUX_CODEC: &[&str] = &["-c", "copy"];

    /// Record next to `recording`, as `<stem>.audio.m4a`.
    pub fn start(recording: &Path) -> Result<Self> {
        let path = recording.with_extension("audio.m4a");
        let started = Instant::now();
        let child = ffmpeg(&audio_args(&path), &recording.with_extension("audio.log"))?
            .stdin(Stdio::null())
            .spawn()
            .map_err(|error| Error::msg(format!("Cannot run ffmpeg: {error}")))?;
        Ok(Self {
            child,
            path,
            started,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// When capture started.
    pub fn started(&self) -> Instant {
        self.started
    }

    /// Stop the capture and wait up to `timeout` for the file; false when it did not finish.
    pub fn finish(&mut self, clock: &dyn Clock, timeout: Duration) -> bool {
        if let Ok(pid) = i32::try_from(self.child.id()) {
            // SAFETY: signalling our own child; ffmpeg finishes its file on SIGINT.
            unsafe { libc::kill(pid, libc::SIGINT) };
        }
        let finished = poll(clock, timeout, Duration::from_millis(50), || {
            Ok(self.child.try_wait()?)
        });
        if matches!(finished, Ok(Some(_))) {
            return true;
        }
        let _ = self.child.kill();
        false
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }
}

/// The `ffmpeg` arguments that record the default output's audio to `path`.
fn audio_args(path: &Path) -> Vec<String> {
    let mut args: Vec<String> = [
        "-hide_banner",
        "-loglevel",
        "warning",
        "-nostdin",
        "-y",
        "-f",
        "pulse",
        "-i",
        "@DEFAULT_MONITOR@",
        "-c:a",
        "aac",
        "-b:a",
        "192k",
    ]
    .map(str::to_owned)
    .into();
    args.push(path.to_string_lossy().into_owned());
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::video_args;

    #[test]
    fn basenames_split_on_both_separators_and_ignore_case() {
        let name = |text: &str| basename(&OsString::from(text));
        assert_eq!(
            name(r"C:\XboxGames\Minecraft\Content\Minecraft.Windows.exe"),
            GAME_EXE
        );
        assert_eq!(name("/opt/proton/files/bin/winedbg"), "winedbg");
        assert_eq!(name("winedbg.exe"), "winedbg.exe");
    }

    #[test]
    fn environ_splits_on_the_first_equals() {
        let pairs = parse_environ(b"WINEPREFIX=/a=b\0BROKEN\0WINEFSYNC=1\0");
        assert_eq!(pairs[0], ("WINEPREFIX".into(), "/a=b".into()));
        assert_eq!(pairs[1], ("WINEFSYNC".into(), "1".into()));
        assert_eq!(pairs.len(), 2);
    }

    #[test]
    fn game_executables_resolve_through_dosdevices_or_maps() {
        assert_eq!(
            windows_to_unix(
                Path::new("/pfx"),
                r"C:\XboxGames\Minecraft\Content\Minecraft.Windows.exe"
            )
            .unwrap(),
            Path::new("/pfx/dosdevices/c:/XboxGames/Minecraft/Content/Minecraft.Windows.exe")
        );
        assert_eq!(
            windows_to_unix(Path::new("/pfx"), "/opt/game.exe").unwrap(),
            Path::new("/opt/game.exe")
        );
        assert!(windows_to_unix(Path::new("/pfx"), "game.exe").is_none());
        let maps = "7f0-7f1 r--p 00000000 00:1f 12 /usr/lib/libc.so.6\n\
                    140000000-140001000 r--p 00000000 00:1f 34 /games/Content/Minecraft.Windows.exe\n";
        assert_eq!(
            mapped_exe(maps).unwrap(),
            Path::new("/games/Content/Minecraft.Windows.exe")
        );
        assert!(mapped_exe("7f0-7f1 r--p 0 0 0\n").is_none());
    }

    fn env_of(pairs: &[(&str, &Path)]) -> Vec<(String, OsString)> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.as_os_str().to_owned()))
            .collect()
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"").unwrap();
    }

    #[test]
    fn prefixes_resolve_proton_links_and_trailing_slashes() {
        let dir = crate::testdir::TestDir::new("prefix");
        let prefix = dir.path().join("compatdata/pfx");
        fs::create_dir_all(&prefix).unwrap();
        std::os::unix::fs::symlink(".", prefix.join("pfx")).unwrap();
        let nested = PathBuf::from(format!("{}/pfx/", prefix.display()));
        let real = fs::canonicalize(&prefix).unwrap();
        assert_eq!(wine_prefix(&env_of(&[("WINEPREFIX", &nested)])), real);
        assert_eq!(wine_prefix(&env_of(&[("WINEPREFIX", &prefix)])), real);
        let home = dir.path().join("home");
        fs::create_dir_all(home.join(".wine")).unwrap();
        assert_eq!(
            wine_prefix(&env_of(&[("HOME", &home)])),
            fs::canonicalize(home.join(".wine")).unwrap()
        );
    }

    #[test]
    fn wine_is_found_for_proton_and_plain_wine() {
        let dir = crate::testdir::TestDir::new("wine");
        let proton = dir.path().join("proton");
        let preloader = proton.join("files/lib/wine/x86_64-unix/wine-preloader");
        for file in [
            proton.join("files/bin/wine"),
            proton.join("files/lib/wine/x86_64-unix/wine"),
            preloader.clone(),
        ] {
            touch(&file);
        }
        let bin_wine = proton.join("files/bin/wine");
        assert_eq!(
            game_wine(&env_of(&[("PROTONPATH", &proton)]), None),
            Some(bin_wine.clone())
        );
        assert_eq!(game_wine(&[], Some(&preloader)), Some(bin_wine.clone()));
        assert_eq!(wine_root(&bin_wine), Some(proton.join("files")));

        let usr = dir.path().join("usr");
        touch(&usr.join("bin/wine"));
        touch(&usr.join("bin/wine-preloader"));
        assert_eq!(
            game_wine(&[], Some(&usr.join("bin/wine-preloader"))),
            Some(usr.join("bin/wine"))
        );
        assert_eq!(wine_root(&usr.join("bin/wine")), Some(usr.clone()));

        let loader = dir.path().join("opt/wine64");
        touch(&loader);
        assert_eq!(
            game_wine(&env_of(&[("WINELOADER", &loader)]), None),
            Some(loader)
        );
        assert_eq!(game_wine(&[], None), None);
    }

    #[test]
    fn foreign_dlls_skip_known_folders() {
        let maps = "1000-2000 r--p 00000000 00:1f 1 /games/mc/fmod.dll\n\
                    2000-3000 r--p 00000000 00:1f 2 /pfx/drive_c/windows/system32/dxgi.dll\n\
                    3000-4000 r--p 00000000 00:1f 3 /pfx/drive_c/Program Files (x86)/Runtime/rt.dll\n\
                    4000-5000 r--p 00000000 00:1f 4 /home/bob/mods/Shiny Client.dll\n\
                    5000-6000 r--p 00001000 00:1f 4 /home/bob/mods/Shiny Client.dll\n\
                    6000-7000 r--p 00000000 00:1f 5 /data/bedrock-mcp/agent/bedrock_agent.dll\n\
                    7000-8000 r--p 00000000 00:1f 6 /opt/hooks/HOOK.DLL (deleted)\n\
                    8000-9000 rw-p 00000000 00:00 0 \n";
        let known = [
            "/games/mc",
            "/pfx/drive_c/windows",
            "/pfx/drive_c/Program Files (x86)",
        ]
        .map(PathBuf::from)
        .into_iter()
        .chain([PathBuf::from("/data/bedrock-mcp/agent")])
        .collect::<Vec<_>>();
        assert_eq!(foreign_dlls(maps, &known), ["HOOK.DLL", "Shiny Client.dll"]);
        assert_eq!(
            mapped_agent_profile(maps),
            Some(PathBuf::from("/data/bedrock-mcp"))
        );
        assert_eq!(
            mapped_agent_profile("1-2 r--p 0 0 0 /x/bedrock_agent.dll\n"),
            None
        );
    }

    #[test]
    fn discovered_games_get_a_folder_per_prefix() {
        assert_eq!(
            discovered_profile(Path::new("/data"), Path::new("/home/bob/games/pfx")),
            Path::new("/data/bedrock-mcp/games/home-bob-games-pfx")
        );
    }

    fn lookup<'a>(env: &'a [(OsString, OsString)], key: &str) -> Option<&'a str> {
        env.iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.to_str().unwrap())
    }

    #[test]
    fn injector_environment_follows_the_game() {
        let inherited = [
            ("PATH", "/usr/bin"),
            ("WINEESYNC", "1"),
            ("WINEPREFIX", "/elsewhere"),
            ("LD_LIBRARY_PATH", "/opt/lib"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let game = [("WINEFSYNC".to_owned(), OsString::from("1"))];
        let env = injector_environment(
            inherited,
            Path::new("/engine/files"),
            Path::new("/pfx"),
            &game,
        );
        assert_eq!(lookup(&env, "PATH"), Some("/usr/bin"));
        assert_eq!(lookup(&env, "WINEPREFIX"), Some("/pfx"));
        assert_eq!(lookup(&env, "WINEDEBUG"), Some("-all"));
        assert_eq!(lookup(&env, "WINEFSYNC"), Some("1"));
        assert_eq!(lookup(&env, "WINEESYNC"), None);
        assert_eq!(
            lookup(&env, "LD_LIBRARY_PATH"),
            Some("/engine/files/lib/x86_64-linux-gnu:/engine/files/lib/i386-linux-gnu:/opt/lib")
        );
        assert_eq!(env.iter().filter(|(key, _)| key == "WINEPREFIX").count(), 1);
    }

    #[test]
    fn injector_environment_keeps_a_chosen_winedebug() {
        let inherited = [(OsString::from("WINEDEBUG"), OsString::from("+loaddll"))];
        let env = injector_environment(inherited, Path::new("/e/files"), Path::new("/p"), &[]);
        assert_eq!(lookup(&env, "WINEDEBUG"), Some("+loaddll"));
        assert_eq!(
            lookup(&env, "LD_LIBRARY_PATH"),
            Some("/e/files/lib/x86_64-linux-gnu:/e/files/lib/i386-linux-gnu")
        );
    }

    #[test]
    fn agent_paths_use_the_z_drive() {
        assert_eq!(
            agent_path(Path::new("/home/u/agent/bedrock_agent.dll")),
            r"Z:\home\u\agent\bedrock_agent.dll"
        );
    }

    #[test]
    fn udp_tables_list_bound_local_ports() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops\n\
                     \x20 12: 0100007F:4ABC 00000000:0000 07 00000000:00000000 00:00000000 00000000  1000        0 1 2 0 0\n\
                     \x20 13: 00000000000000000000000001000000:0035 00000000000000000000000000000000:0000 07 0 0 0 0 0\n";
        assert_eq!(udp_ports(table), [0x4ABC, 53]);
    }

    #[test]
    fn ffmpeg_encodes_on_vaapi_from_the_agent_format() {
        let encoder = vaapi(Path::new("/dev/dri/renderD128"), "hevc").unwrap();
        let args = video_args(
            &encoder,
            (1280, 720, protocol::frames::format::RGB10A2),
            120,
            Path::new("/tmp/a.mp4"),
        )
        .unwrap()
        .join(" ");
        assert_eq!(
            args,
            "-hide_banner -loglevel warning -nostdin -y -vaapi_device /dev/dri/renderD128 \
             -f rawvideo -pixel_format x2bgr10le -video_size 1280x720 -framerate 120 -i pipe:0 \
             -vf format=bgr0,hwupload,scale_vaapi=format=nv12 -c:v hevc_vaapi -qp 18 /tmp/a.mp4"
        );
        assert!(vaapi(Path::new("/d"), "av1").is_err());
    }
}
