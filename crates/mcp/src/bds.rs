//! GameTests on a Bedrock Dedicated Server (BDS) with no client: a scratch server folder that
//! links the user's server files, a fresh or copied world with the Beta APIs experiment, and
//! a console driver that runs `/gametest` and reads the results from the server's output.

use std::{
    env, fs,
    io::{self, BufRead, BufReader, BufWriter, Read, Write},
    net::UdpSocket,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use crate::{
    content_log,
    error::{Error, Result, bail},
    gametest::{self, Run},
    log_diagnostics::{self, Diagnostic, LogEntry, PackRoot, Severity},
    nbt::{LevelDat, Tag},
    packs::{self, Kind, Pack},
    worlds::{self, WorldFolder},
};

pub const SERVER_SETTING: &str = "BEDROCK_MCP_BDS";
const DOWNLOAD: &str = "https://www.minecraft.net/en-us/download/server/bedrock";
#[cfg(windows)]
const BINARY: &str = "bedrock_server.exe";
#[cfg(not(windows))]
const BINARY: &str = "bedrock_server";
const LEVEL_NAME: &str = "gametest";
/// Top-level server entries the run makes itself instead of linking.
const FRESH: [&str; 7] = [
    "server.properties",
    "worlds",
    "development_behavior_packs",
    "development_resource_packs",
    "development_skin_packs",
    "logs",
    "minecraftpe",
];
/// Server files smaller than this are copied, so a server that rewrites its config files
/// writes the copies.
const COPY_LIMIT: u64 = 1 << 20;
const BETA_EXPERIMENTS: [&str; 3] = [
    "gametest",
    "experiments_ever_used",
    "saved_with_toggled_experiments",
];
const FLAT_GENERATOR: i32 = 2;
const GAMERULES: [&str; 3] = [
    "gamerule domobspawning false",
    "gamerule dodaylightcycle false",
    "gamerule doweathercycle false",
];
const START_TIMEOUT: Duration = Duration::from_secs(180);
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
/// Results are complete once every expected test has one and output stayed quiet this long.
const SETTLE: Duration = Duration::from_secs(5);
/// A command's reply: the lines that arrive until output is quiet this long.
const REPLY_QUIET: Duration = Duration::from_millis(300);
const REPLY_FIRST: Duration = Duration::from_secs(5);
const REPLY_MAX: Duration = Duration::from_secs(10);
/// An ERROR this soon after the gametest command, before any test activity, is its answer.
const ANSWER_WINDOW: Duration = Duration::from_secs(2);
const TICK: Duration = Duration::from_millis(100);
/// Lines of the log quoted when the server does not start.
const LOG_TAIL: usize = 20;
/// A load check stops once output was quiet this long after "Server started.", which covers
/// the scripts' first run and function loading.
const CHECK_QUIET: Duration = Duration::from_secs(2);
/// ... or this long after the start, for packs whose scripts keep logging.
const CHECK_AFTER_START: Duration = Duration::from_secs(15);

pub const DEFAULT_TIMEOUT_MS: u64 = 900_000;
pub const DEFAULT_IDLE_MS: u64 = 120_000;

/// What a run uses.
pub struct Options {
    pub server: PathBuf,
    /// The scratch folder, recreated by the run.
    pub run_dir: PathBuf,
    pub packs: Vec<PathBuf>,
    /// A client world to copy, and the client's `com.mojang` folder its packs come from.
    pub world: Option<(WorldFolder, PathBuf)>,
    pub commands: Vec<String>,
    pub run: Run,
    pub timeout: Duration,
    pub idle: Duration,
}

/// `$BEDROCK_MCP_BDS`, else the usual install folder.
pub fn default_server() -> PathBuf {
    if let Some(path) = env::var_os(SERVER_SETTING).filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    data_home().join("bedrock-server").join("current")
}

#[cfg(windows)]
fn data_home() -> PathBuf {
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_default()
}

#[cfg(not(windows))]
fn data_home() -> PathBuf {
    if let Some(path) = env::var_os("XDG_DATA_HOME").filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".local/share"))
        .unwrap_or_default()
}

pub fn development_dir(kind: Kind) -> &'static str {
    match kind {
        Kind::Behavior => "development_behavior_packs",
        Kind::Resource => "development_resource_packs",
    }
}

/// Run the tests and report; see the `gametest_server` tool for the reply.
pub fn run(options: &Options) -> Result<Value> {
    let began = Instant::now();
    let command = gametest::run_command(&options.run)?;
    if let Some(bad) = options
        .commands
        .iter()
        .find(|line| line.contains(['\n', '\r']))
    {
        bail!("Console command {bad:?} spans several lines; give one command per entry.");
    }
    let dir = &options.run_dir;
    let log_path = dir.join("server.log");
    let Scratch {
        log,
        user: _,
        mut notes,
    } = prepare_scratch(&ScratchSpec {
        server: &options.server,
        dir,
        log_path: &log_path,
        packs: &options.packs,
        world: options.world.as_ref(),
        beta_apis: true,
        timeout: START_TIMEOUT,
    })?;

    let mut server = Server::start(dir, log)?;
    if !server.wait_started(START_TIMEOUT) {
        let output = server.stop();
        bail!(
            "The server {} before \"Server started.\" (log {}). Last lines:\n{}",
            if output.exited {
                "exited"
            } else {
                "did not start"
            },
            log_path.display(),
            log_tail(&log_path)
        );
    }
    for rule in GAMERULES {
        server.command(rule);
    }
    let command_replies: Vec<Value> = options
        .commands
        .iter()
        .map(|line| json!({ "command": line, "reply": server.command(line) }))
        .collect();
    let outcome = server.run_tests(&command, single_test_runs(&options.run), options);
    let output = server.stop();

    let expected = match outcome {
        Outcome::Answer(_) => 0,
        _ => single_test_runs(&options.run)
            .or(output.expected)
            .unwrap_or(0),
    };
    let results: Vec<Value> = output
        .results
        .iter()
        .map(|result| json!({ "test": result.test, "passed": result.passed, "message": result.message }))
        .collect();
    let failed: Vec<Value> = output
        .results
        .iter()
        .filter(|result| !result.passed)
        .map(|result| json!({ "test": result.test, "message": result.message }))
        .collect();
    let passed = output.results.iter().filter(|result| result.passed).count();
    let errors = content_log::summarize(
        output
            .problems
            .iter()
            .filter(|problem| problem.level == "ERROR")
            .map(|problem| (problem.area.as_str(), "ERROR", problem.message.as_str())),
    );
    match &outcome {
        Outcome::Done => {}
        Outcome::Answer(message) => {
            notes.push(format!("The server answered {command:?} with: {message}"))
        }
        Outcome::Idle => notes.push(format!(
            "No test made progress for {} ms, so the run stopped early (idle_ms).",
            options.idle.as_millis()
        )),
        Outcome::Timeout => notes.push(format!(
            "The tests did not finish within {} ms (timeout_ms).",
            options.timeout.as_millis()
        )),
        Outcome::Exited => notes.push("The server exited during the run; see log.".into()),
    }
    if expected == 0 {
        notes.push(
            "No test ran. Check that the pack registers tests with that id or tag and that the \
             pack stack lists the pack; errors show why a pack did not load."
                .into(),
        );
    } else {
        let got = match single_test_runs(&options.run) {
            Some(_) => output.result_lines,
            None => output.results.len(),
        };
        if got < expected {
            notes.push(format!(
                "{} of {expected} expected test runs have no result.",
                expected - got
            ));
        }
    }
    if !failed.is_empty() {
        notes.push(format!("{} tests failed.", failed.len()));
    }
    if output.pack_stack.iter().any(|line| line.ends_with("None")) && !options.packs.is_empty() {
        notes.push(
            "The pack stack is empty although packs were given: the server may be older than \
             the packs' min_engine_version."
                .into(),
        );
    }
    if !output.exited_cleanly {
        notes.push("The server did not quit after stop and was killed.".into());
    }
    Ok(json!({
        "server": { "path": options.server, "version": output.version },
        "command": command,
        "expected": expected,
        "passed": passed,
        "failed": failed,
        "results": results,
        "timed_out": matches!(outcome, Outcome::Idle | Outcome::Timeout),
        "errors": errors,
        "pack_stack": output.pack_stack,
        "command_replies": command_replies,
        "log": log_path,
        "elapsed_ms": began.elapsed().as_millis() as u64,
        "notes": notes,
    }))
}

/// What a load check uses.
pub struct CheckOptions {
    pub server: PathBuf,
    /// The scratch folder, recreated by the check and removed after it.
    pub run_dir: PathBuf,
    /// Where the server's output goes; it outlives the scratch folder.
    pub log_path: PathBuf,
    pub packs: Vec<PathBuf>,
    pub beta_apis: bool,
    /// The whole check: world creation, start and the quiet wait.
    pub timeout: Duration,
}

/// Start a server with the packs in a fresh flat world, collect what it reports while it
/// loads them, stop it and remove the scratch folder; see the `bds_check` tool for the reply.
pub fn check(options: &CheckOptions) -> Result<Value> {
    let began = Instant::now();
    if options.packs.is_empty() {
        bail!("Give at least one pack folder to check.");
    }
    let prepared = prepare_scratch(&ScratchSpec {
        server: &options.server,
        dir: &options.run_dir,
        log_path: &options.log_path,
        packs: &options.packs,
        world: None,
        beta_apis: options.beta_apis,
        timeout: options.timeout,
    });
    let result = prepared.and_then(|scratch| check_loaded(options, scratch, began));
    let _ = fs::remove_dir_all(&options.run_dir);
    result
}

fn check_loaded(options: &CheckOptions, scratch: Scratch, began: Instant) -> Result<Value> {
    let Scratch {
        log,
        user,
        mut notes,
    } = scratch;
    let mut server = Server::start(&options.run_dir, log)?;
    let remaining = options.timeout.saturating_sub(began.elapsed());
    let started = server.wait_started(remaining);
    let timed_out = if started {
        !server.wait_quiet(
            CHECK_QUIET,
            CHECK_AFTER_START.min(options.timeout.saturating_sub(began.elapsed())),
        )
    } else {
        !server.output.exited
    };
    let output = server.stop();
    if !started {
        notes.push(format!(
            "The server {} before \"Server started.\". Last lines of the log:\n{}",
            if timed_out {
                "did not start within timeout_ms"
            } else {
                "exited"
            },
            log_tail(&options.log_path)
        ));
    } else if timed_out {
        notes.push(format!(
            "The server kept printing for {} s after it started, so the check stopped it; \
             later problems are not listed.",
            CHECK_AFTER_START.as_secs()
        ));
    }
    if !output.exited_cleanly {
        notes.push("The server did not quit after stop and was killed.".into());
    }
    let roots: Vec<PackRoot> = user
        .iter()
        .map(|(kind, pack, path)| PackRoot {
            kind: *kind,
            path: path.clone(),
            name: pack.name.clone(),
        })
        .collect();
    let entries: Vec<LogEntry> = output
        .problems
        .iter()
        .map(|problem| LogEntry {
            area: &problem.area,
            level: &problem.level,
            message: &problem.message,
        })
        .collect();
    let mut diagnostics = log_diagnostics::map(&entries, &roots, "bds");
    if started {
        diagnostics.extend(unloaded_packs(&roots, &user, &output.pack_stack));
    }
    Ok(json!({
        "server": { "path": options.server, "version": output.version },
        "started": started,
        "timed_out": timed_out,
        "diagnostics": diagnostics,
        "counts": log_diagnostics::counts(&diagnostics),
        "pack_stack": output.pack_stack,
        "log": options.log_path,
        "elapsed_ms": began.elapsed().as_millis() as u64,
        "notes": notes,
    }))
}

/// An error for each given pack the server's pack stack does not list.
fn unloaded_packs(roots: &[PackRoot], user: &[UserPack], pack_stack: &[String]) -> Vec<Diagnostic> {
    user.iter()
        .zip(roots)
        .filter(|((_, pack, _), _)| {
            let id = format!("id: {}", pack.uuid.to_ascii_lowercase());
            !pack_stack
                .iter()
                .any(|line| line.to_ascii_lowercase().contains(&id))
        })
        .map(|((kind, pack, _), _)| Diagnostic {
            file: Some(format!(
                "{}/manifest.json",
                match kind {
                    Kind::Behavior => "BP",
                    Kind::Resource => "RP",
                }
            )),
            line: None,
            column: None,
            severity: Severity::Error,
            rule: "pack-not-loaded".into(),
            message: format!(
                "The server's pack stack does not list {} ({}); the server may be older than \
                 its min_engine_version or Script API module versions.",
                pack.name, pack.uuid
            ),
            source: "bds",
            in_pack: true,
        })
        .collect()
}

/// A given pack: its kind, manifest and canonical folder.
pub type UserPack = (Kind, Pack, PathBuf);

/// What a scratch server needs.
struct ScratchSpec<'a> {
    server: &'a Path,
    dir: &'a Path,
    log_path: &'a Path,
    packs: &'a [PathBuf],
    world: Option<&'a (WorldFolder, PathBuf)>,
    beta_apis: bool,
    /// For making a fresh world.
    timeout: Duration,
}

/// A scratch server folder ready to start: its log, the given packs and notes so far.
struct Scratch {
    log: BufWriter<fs::File>,
    user: Vec<UserPack>,
    notes: Vec<String>,
}

/// Recreate `dir` as a server folder that links the server's files, holds a copy of the
/// client world or a fresh flat one, and applies the given packs to it.
fn prepare_scratch(spec: &ScratchSpec) -> Result<Scratch> {
    if !spec.server.join(BINARY).is_file() {
        bail!(
            "No Bedrock Dedicated Server at {} ({BINARY} is missing). Download it from {DOWNLOAD}, \
             unzip it there, or pass server or set {SERVER_SETTING} to its folder.",
            spec.server.display()
        );
    }
    let user = user_packs(spec.packs)?;
    let dir = spec.dir;
    if dir.exists() {
        fs::remove_dir_all(dir)
            .map_err(|error| Error::msg(format!("Cannot clear {}: {error}", dir.display())))?;
    }
    fs::create_dir_all(dir)?;
    link_server(spec.server, dir)?;
    write_properties(dir)?;
    for kind in [Kind::Behavior, Kind::Resource] {
        fs::create_dir_all(dir.join(development_dir(kind)))?;
    }
    let world = dir.join("worlds").join(LEVEL_NAME);
    fs::create_dir_all(dir.join("worlds"))?;
    if let Some(parent) = spec.log_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut log = BufWriter::new(fs::File::create(spec.log_path)?);
    let mut notes: Vec<String> = Vec::new();
    match spec.world {
        Some((client, shared)) => {
            copy_dir(&client.path, &world)?;
            fs::write(world.join("levelname.txt"), LEVEL_NAME)?;
            link_world_packs(&world, shared, dir, &user, &mut notes)?;
        }
        None => create_world(dir, &world, &mut log, spec.log_path, spec.timeout)?,
    }
    prepare_level(
        &world.join("level.dat"),
        spec.world.is_none(),
        spec.beta_apis,
    )?;
    for (kind, pack, source) in &user {
        let target = unique_child(&dir.join(development_dir(*kind)), source);
        link_dir(source, &target)?;
        let list_path = world.join(kind.world_list());
        let mut list = packs::read_pack_list(&list_path)?;
        let entry = worlds::entry(pack);
        match list
            .iter_mut()
            .find(|item| item.pack_id.eq_ignore_ascii_case(&pack.uuid))
        {
            Some(item) => *item = entry,
            None => list.insert(0, entry),
        }
        packs::write_pack_list(&list_path, &list)?;
    }
    for kind in [Kind::Behavior, Kind::Resource] {
        let list_path = world.join(kind.world_list());
        if !list_path.exists() {
            packs::write_pack_list(&list_path, &[])?;
        }
    }
    Ok(Scratch { log, user, notes })
}

/// For `gametest run`, the number of runs; the server prints no count for it.
fn single_test_runs(run: &Run) -> Option<usize> {
    run.test.as_ref().map(|_| run.repeat.unwrap_or(1) as usize)
}

/// The given pack folders with their kind; the manifest decides.
pub fn user_packs(paths: &[PathBuf]) -> Result<Vec<UserPack>> {
    let mut found = Vec::new();
    for path in paths {
        let source = fs::canonicalize(path)
            .map_err(|error| Error::msg(format!("Pack folder {}: {error}", path.display())))?;
        let (Some(parent), Some(name)) = (source.parent(), source.file_name()) else {
            bail!("{} is not a pack folder.", path.display());
        };
        let name = name.to_string_lossy();
        let pack = [Kind::Behavior, Kind::Resource]
            .into_iter()
            .find_map(|kind| {
                packs::packs_in(parent, "given", kind)
                    .into_iter()
                    .find(|pack| pack.folder == name)
                    .map(|pack| (kind, pack))
            });
        let Some((kind, pack)) = pack else {
            bail!(
                "{} has no manifest.json with data, script or resources modules.",
                path.display()
            );
        };
        found.push((kind, pack, source));
    }
    Ok(found)
}

/// Link the installed packs a copied world applies into the run's development folders.
fn link_world_packs(
    world: &Path,
    shared: &Path,
    dir: &Path,
    user: &[(Kind, Pack, PathBuf)],
    notes: &mut Vec<String>,
) -> Result<()> {
    for kind in [Kind::Behavior, Kind::Resource] {
        let own = packs::packs_in(&world.join(kind.world_dir()), "world", kind);
        let installed = packs::installed_packs(shared, kind);
        for entry in packs::read_pack_list(&world.join(kind.world_list()))? {
            let id = &entry.pack_id;
            let matches = |pack: &Pack| pack.uuid.eq_ignore_ascii_case(id);
            if own.iter().any(matches) || user.iter().any(|(_, pack, _)| matches(pack)) {
                continue;
            }
            match installed.iter().find(|pack| matches(pack)) {
                Some(pack) => {
                    let source = shared.join(&pack.location).join(&pack.folder);
                    let target = unique_child(&dir.join(development_dir(kind)), &source);
                    link_dir(&source, &target)?;
                }
                None => notes.push(format!(
                    "The world applies pack {id}, which is not installed in the game's pack folders."
                )),
            }
        }
    }
    Ok(())
}

/// `parent/<source's name>`, numbered when that name is taken.
fn unique_child(parent: &Path, source: &Path) -> PathBuf {
    let name = source
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "pack".into());
    let mut target = parent.join(&name);
    let mut number = 2;
    while target.symlink_metadata().is_ok() {
        target = parent.join(format!("{name}_{number}"));
        number += 1;
    }
    target
}

/// Link the server's files into `dir`, except the ones the run makes itself.
fn link_server(server: &Path, dir: &Path) -> Result<()> {
    for entry in fs::read_dir(server)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_text = name.to_string_lossy();
        if FRESH.contains(&name_text.as_ref()) || name_text.ends_with(".zip") {
            continue;
        }
        let source = entry.path();
        let target = dir.join(&name);
        let metadata = fs::metadata(&source)?;
        if metadata.is_dir() {
            link_dir(&source, &target)?;
        } else if metadata.len() < COPY_LIMIT {
            fs::copy(&source, &target)?;
        } else {
            link_file(&source, &target)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn link_file(source: &Path, target: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(source, target)
}

#[cfg(unix)]
fn link_dir(source: &Path, target: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(source, target)
}

/// A hard link, or a copy across volumes.
#[cfg(windows)]
fn link_file(source: &Path, target: &Path) -> io::Result<()> {
    fs::hard_link(source, target).or_else(|_| fs::copy(source, target).map(drop))
}

/// A directory symlink (it needs Developer Mode), else a tree of linked files.
#[cfg(windows)]
fn link_dir(source: &Path, target: &Path) -> io::Result<()> {
    if std::os::windows::fs::symlink_dir(source, target).is_ok() {
        return Ok(());
    }
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        let to = target.join(entry.file_name());
        if fs::metadata(&path)?.is_dir() {
            link_dir(&path, &to)?;
        } else {
            link_file(&path, &to)?;
        }
    }
    Ok(())
}

pub fn copy_dir(source: &Path, target: &Path) -> io::Result<()> {
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        let to = target.join(entry.file_name());
        if fs::metadata(&path)?.is_dir() {
            copy_dir(&path, &to)?;
        } else {
            fs::copy(&path, &to)?;
        }
    }
    Ok(())
}

/// A UDP port whose successor is free too, for IPv4 and IPv6.
fn free_ports() -> Result<(u16, u16)> {
    for _ in 0..32 {
        let v4 = UdpSocket::bind("0.0.0.0:0")?;
        let port = v4.local_addr()?.port();
        let Some(next) = port.checked_add(1) else {
            continue;
        };
        if UdpSocket::bind(("::", next)).is_ok() || UdpSocket::bind(("0.0.0.0", next)).is_ok() {
            return Ok((port, next));
        }
    }
    bail!("Found no free pair of UDP ports for the server.")
}

fn write_properties(dir: &Path) -> Result<()> {
    let (port, port_v6) = free_ports()?;
    let text = format!(
        "server-name=bedrock-mcp gametest\n\
         gamemode=creative\n\
         difficulty=easy\n\
         allow-cheats=true\n\
         max-players=1\n\
         online-mode=false\n\
         allow-list=false\n\
         server-port={port}\n\
         server-portv6={port_v6}\n\
         enable-lan-visibility=false\n\
         level-name={LEVEL_NAME}\n\
         level-seed=\n\
         tick-distance=4\n\
         content-log-file-enabled=false\n\
         content-log-console-output-enabled=true\n\
         content-log-level=info\n\
         emit-server-telemetry=false\n"
    );
    fs::write(dir.join("server.properties"), text)?;
    Ok(())
}

/// Let the server make a world, then empty its chunks so the flat generator takes over.
fn create_world(
    dir: &Path,
    world: &Path,
    log: &mut BufWriter<fs::File>,
    log_path: &Path,
    timeout: Duration,
) -> Result<()> {
    let first = log.get_ref().try_clone()?;
    let mut server = Server::start(dir, BufWriter::new(first))?;
    let started = server.wait_started(timeout);
    server.stop();
    let _ = writeln!(log, "--- world created; the server starts again ---");
    if !started || !world.join("level.dat").is_file() {
        bail!(
            "The server did not create a world in {} (log {}). Last lines:\n{}",
            world.display(),
            log_path.display(),
            log_tail(log_path)
        );
    }
    fs::remove_dir_all(world.join("db"))?;
    Ok(())
}

/// Turn on the Beta APIs experiment when asked, and the flat generator for a fresh world.
fn prepare_level(path: &Path, flat: bool, beta_apis: bool) -> Result<()> {
    let data = fs::read(path)
        .map_err(|error| Error::msg(format!("Cannot read {}: {error}", path.display())))?;
    let mut level = LevelDat::parse(&data)
        .map_err(|error| Error::msg(format!("{}: {error}", path.display())))?;
    if beta_apis {
        let experiments = level.root.compound_mut("experiments");
        for key in BETA_EXPERIMENTS {
            experiments.set(key, Tag::Byte(1));
        }
    }
    if flat {
        level.root.set("Generator", Tag::Int(FLAT_GENERATOR));
    }
    fs::write(path, level.to_bytes()?)?;
    Ok(())
}

fn log_tail(path: &Path) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(LOG_TAIL)..].join("\n")
}

/// `[2026-01-02 03:04:05:678 INFO] message` → (level, message).
fn split_prefix(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix('[')?;
    let (head, message) = rest.split_once(']')?;
    let level = head.rsplit(' ').next()?;
    if !matches!(level, "INFO" | "WARN" | "ERROR" | "VERBOSE")
        || !head.starts_with(|c: char| c.is_ascii_digit())
    {
        return None;
    }
    Some((level, message.strip_prefix(' ').unwrap_or(message)))
}

/// `[Scripting] message` → ("Scripting", "message"); no area gives "".
fn split_area(message: &str) -> (&str, &str) {
    message
        .strip_prefix('[')
        .and_then(|rest| rest.split_once(']'))
        .filter(|(area, _)| !area.is_empty() && !area.contains(' '))
        .map(|(area, rest)| (area, rest.trim_start()))
        .unwrap_or(("", message))
}

// The console strings matched below ("Version: ", "Server started.", "Pack Stack - ",
// "Running N tests with tag", "Running test batch", "onTestPassed: ", "onTestFailed: ") come
// from the console output of the public Bedrock Dedicated Server download (DOWNLOAD).
fn gametest_line(line: &str) -> bool {
    line.starts_with("onTest") || line.starts_with("Running test batch")
}

#[derive(Debug, PartialEq)]
struct TestResult {
    test: String,
    passed: bool,
    message: String,
}

/// What the server printed, read line by line.
#[derive(Debug, Default)]
struct Output {
    version: Option<String>,
    started: bool,
    /// From `Running N tests with tag`, else the sum of the batch sizes.
    expected: Option<usize>,
    batch_total: usize,
    /// One per test, its last result, in first-result order.
    results: Vec<TestResult>,
    result_lines: usize,
    /// GameTest lines seen: batches, structure loads and results.
    activity: usize,
    pack_stack: Vec<String>,
    /// ERROR and WARN entries in order, continuation lines joined.
    problems: Vec<Problem>,
    pending: Option<(String, String)>,
    exited: bool,
    exited_cleanly: bool,
}

/// A console ERROR or WARN entry: `[2026-01-02 03:04:05:678 ERROR] [Recipes] message`.
#[derive(Debug, PartialEq)]
struct Problem {
    level: String,
    area: String,
    message: String,
}

impl Output {
    fn feed(&mut self, line: &str) {
        if let Some((level, message)) = split_prefix(line) {
            self.flush();
            if let Some(version) = message.strip_prefix("Version: ") {
                self.version
                    .get_or_insert_with(|| version.trim().to_owned());
            } else if message.trim() == "Server started." {
                self.started = true;
            } else if let Some(stack) = message.strip_prefix("Pack Stack - ") {
                self.pack_stack.push(stack.trim().to_owned());
            } else if let Some(count) = tests_with_tag(message) {
                self.expected = Some(count);
            }
            self.pending = Some((level.to_owned(), message.to_owned()));
        } else if gametest_line(line) {
            self.flush();
            self.activity += 1;
            if let Some(count) = batch_size(line) {
                self.batch_total += count;
            } else if let Some(result) = parse_result(line) {
                self.result_lines += 1;
                match self
                    .results
                    .iter_mut()
                    .find(|known| known.test == result.test)
                {
                    Some(known) => *known = result,
                    None => self.results.push(result),
                }
            }
        } else if line.trim() == "Quit correctly" {
            self.flush();
            self.exited_cleanly = true;
        } else if let Some((_, message)) = &mut self.pending
            && !line.trim().is_empty()
        {
            message.push('\n');
            message.push_str(line);
        }
    }

    fn flush(&mut self) {
        if let Some((level, message)) = self.pending.take()
            && matches!(level.as_str(), "ERROR" | "WARN")
        {
            let (area, message) = split_area(&message);
            self.problems.push(Problem {
                level,
                area: area.to_owned(),
                message: message.to_owned(),
            });
        }
    }

    fn finish(&mut self) {
        self.flush();
        if self.expected.is_none() && self.batch_total > 0 {
            self.expected = Some(self.batch_total);
        }
    }
}

fn tests_with_tag(message: &str) -> Option<usize> {
    let rest = message.strip_prefix("Running ")?;
    let (count, rest) = rest.split_once(' ')?;
    rest.starts_with("tests with tag")
        .then(|| count.parse().ok())?
}

fn batch_size(line: &str) -> Option<usize> {
    let rest = line.strip_prefix("Running test batch")?;
    let open = rest.rfind('(')?;
    rest[open + 1..].split(' ').next()?.parse().ok()
}

fn parse_result(line: &str) -> Option<TestResult> {
    let (passed, rest) = if let Some(rest) = line.strip_prefix("onTestPassed: ") {
        (true, rest)
    } else {
        (false, line.strip_prefix("onTestFailed: ")?)
    };
    let (test, message) = rest.split_once(" - ").unwrap_or((rest, ""));
    Some(TestResult {
        test: test.trim().to_owned(),
        passed,
        message: message.trim().to_owned(),
    })
}

enum Outcome {
    Done,
    /// The gametest command was refused with this message.
    Answer(String),
    Idle,
    Timeout,
    Exited,
}

/// A running server: its console input and its output lines.
struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    log: BufWriter<fs::File>,
    output: Output,
    /// When the last line arrived.
    last_line: Instant,
}

fn pump(stream: impl Read + Send + 'static, lines: mpsc::Sender<String>) {
    thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut buffer = Vec::new();
        loop {
            buffer.clear();
            match reader.read_until(b'\n', &mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buffer);
                    let line = line.trim_end_matches(['\n', '\r']).to_owned();
                    if lines.send(line).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

impl Server {
    fn start(dir: &Path, log: BufWriter<fs::File>) -> Result<Self> {
        let mut command = Command::new(dir.join(BINARY));
        command
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(not(windows))]
        command.env("LD_LIBRARY_PATH", ".");
        let mut child = command.spawn().map_err(|error| {
            Error::msg(format!(
                "Cannot start {}: {error}",
                dir.join(BINARY).display()
            ))
        })?;
        let (sender, lines) = mpsc::channel();
        pump(child.stdout.take().expect("piped"), sender.clone());
        pump(child.stderr.take().expect("piped"), sender);
        Ok(Self {
            stdin: child.stdin.take(),
            child,
            lines,
            log,
            output: Output::default(),
            last_line: Instant::now(),
        })
    }

    /// The next line within `timeout`; `None` on timeout or once the server closed its output.
    fn next(&mut self, timeout: Duration) -> Option<String> {
        match self.lines.recv_timeout(timeout) {
            Ok(line) => {
                let _ = writeln!(self.log, "{line}");
                self.output.feed(&line);
                self.last_line = Instant::now();
                Some(line)
            }
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => {
                self.output.exited = true;
                thread::sleep(timeout.min(TICK));
                None
            }
        }
    }

    fn wait_started(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while !self.output.started && !self.output.exited && Instant::now() < deadline {
            self.next(TICK);
        }
        self.output.started
    }

    /// Read until no line arrives for `quiet`; false when `limit` passed first.
    fn wait_quiet(&mut self, quiet: Duration, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        loop {
            if self.output.exited || self.last_line.elapsed() >= quiet {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            self.next(TICK);
        }
    }

    fn send(&mut self, line: &str) {
        let _ = writeln!(self.log, "> {line}");
        if let Some(stdin) = &mut self.stdin
            && (writeln!(stdin, "{line}").is_err() || stdin.flush().is_err())
        {
            self.stdin = None;
        }
    }

    /// Send a console command and return the lines it printed.
    fn command(&mut self, line: &str) -> Vec<String> {
        self.send(line);
        let sent = Instant::now();
        let mut reply = Vec::new();
        loop {
            let waited = sent.elapsed();
            let limit = if reply.is_empty() {
                REPLY_FIRST
            } else {
                REPLY_QUIET
            };
            if waited >= REPLY_MAX || self.output.exited {
                break;
            }
            let Some(text) = self.next(limit.min(REPLY_MAX - waited)) else {
                break;
            };
            let text = match split_prefix(&text) {
                Some(("INFO", message)) => message.to_owned(),
                Some((level, message)) => format!("{level} {message}"),
                None => text,
            };
            if !text.trim().is_empty() {
                reply.push(text);
            }
        }
        reply
    }

    fn run_tests(
        &mut self,
        command: &str,
        single_runs: Option<usize>,
        options: &Options,
    ) -> Outcome {
        let problems_before = self.output.problems.len();
        self.send(command);
        let sent = Instant::now();
        let deadline = sent + options.timeout;
        let mut progress = sent;
        let mut seen = (self.output.activity, self.output.result_lines);
        loop {
            self.next(TICK);
            let now = Instant::now();
            if self.output.exited {
                return Outcome::Exited;
            }
            if self.output.activity != seen.0 {
                progress = now;
            }
            let results_changed = self.output.result_lines != seen.1;
            seen = (self.output.activity, self.output.result_lines);
            if self.output.activity == 0 && now - sent <= ANSWER_WINDOW + TICK {
                self.output.flush();
                if let Some(problem) = self.output.problems[problems_before..]
                    .iter()
                    .find(|problem| problem.level == "ERROR")
                {
                    return Outcome::Answer(problem.message.clone());
                }
            }
            let expected = single_runs.or(self.output.expected);
            let finished = expected.is_some_and(|expected| match single_runs {
                Some(_) => self.output.result_lines >= expected,
                None => self.output.results.len() >= expected,
            });
            let stopped = single_runs.is_some()
                && options.run.stop_on_failure == Some(true)
                && self.output.results.iter().any(|result| !result.passed);
            if (finished || stopped) && !results_changed && now - progress >= SETTLE {
                return Outcome::Done;
            }
            if now - progress >= options.idle {
                return Outcome::Idle;
            }
            if now >= deadline {
                return Outcome::Timeout;
            }
        }
    }

    /// Send `stop`, wait for the server to quit (killing it after a while), and return what
    /// it printed.
    fn stop(mut self) -> Output {
        if !self.output.exited {
            self.send("stop");
        }
        self.stdin = None;
        let deadline = Instant::now() + STOP_TIMEOUT;
        loop {
            self.next(TICK);
            if matches!(self.child.try_wait(), Ok(Some(_))) || Instant::now() >= deadline {
                break;
            }
        }
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        while self.next(Duration::from_millis(50)).is_some() {}
        let _ = self.log.flush();
        self.output.finish();
        self.output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir::TestDir;

    fn feed(lines: &[&str]) -> Output {
        let mut output = Output::default();
        for line in lines {
            output.feed(line);
        }
        output.finish();
        output
    }

    #[test]
    fn console_output_gives_counts_results_and_errors() {
        let output = feed(&[
            "[2026-01-02 03:04:05:678 INFO] Version: 1.0.0.0",
            "[2026-01-02 03:04:05:678 INFO] Pack Stack - [00] demo (id: x, version: 1.0.0) @ development_behavior_packs/demo",
            "[2026-01-02 03:04:05:678 INFO] Server started.",
            "Running test batch ':0' (3 tests)...",
            "[2026-01-02 03:04:05:678 INFO] Running 3 tests with tag 'smoke'...",
            "onTestStructureLoaded: smoke:pass",
            "onTestPassed: smoke:pass",
            "onTestFailed: smoke:fail - on purpose - really",
            "[2026-01-02 03:04:05:678 ERROR] [Scripting] boom",
            "    at main.js:1",
            "onTestFailed: smoke:zombie - first try",
            "onTestPassed: smoke:zombie",
            "[2026-01-02 03:04:05:678 ERROR] No tests found for tag 'x'",
            "Quit correctly",
        ]);
        assert_eq!(output.version.as_deref(), Some("1.0.0.0"));
        assert!(output.started && output.exited_cleanly);
        assert_eq!(output.expected, Some(3));
        assert_eq!(output.result_lines, 4);
        assert_eq!(
            output.results,
            vec![
                TestResult {
                    test: "smoke:pass".into(),
                    passed: true,
                    message: String::new()
                },
                TestResult {
                    test: "smoke:fail".into(),
                    passed: false,
                    message: "on purpose - really".into()
                },
                TestResult {
                    test: "smoke:zombie".into(),
                    passed: true,
                    message: String::new()
                },
            ]
        );
        let problem = |area: &str, message: &str| Problem {
            level: "ERROR".into(),
            area: area.into(),
            message: message.into(),
        };
        assert_eq!(
            output.problems,
            vec![
                problem("Scripting", "boom\n    at main.js:1"),
                problem("", "No tests found for tag 'x'"),
            ]
        );
        assert_eq!(output.pack_stack.len(), 1);
    }

    #[test]
    fn batch_sizes_count_when_the_tag_line_is_missing() {
        let output = feed(&[
            "Running test batch ':0' (2 tests)...",
            "Running test batch ':1' (1 tests)...",
        ]);
        assert_eq!(output.expected, Some(3));
    }

    #[test]
    fn level_preparation_turns_on_beta_apis_and_flat_terrain() {
        let dir = TestDir::new("bds-level");
        let path = dir.path().join("level.dat");
        let level = LevelDat {
            storage_version: 10,
            root_name: String::new(),
            root: crate::nbt::Compound(vec![("Generator".into(), Tag::Int(1))]),
        };
        fs::write(&path, level.to_bytes().unwrap()).unwrap();
        prepare_level(&path, true, true).unwrap();
        let level = LevelDat::parse(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(level.root.get("Generator"), Some(&Tag::Int(FLAT_GENERATOR)));
        let Some(Tag::Compound(experiments)) = level.root.get("experiments") else {
            panic!("experiments");
        };
        for key in BETA_EXPERIMENTS {
            assert_eq!(experiments.get(key), Some(&Tag::Byte(1)));
        }
    }

    #[test]
    fn load_problems_map_to_pack_files_and_lines() {
        let dir = TestDir::new("bds-check");
        let bp = dir.path().join("demo_bp");
        fs::create_dir_all(bp.join("recipes")).unwrap();
        fs::create_dir_all(bp.join("functions")).unwrap();
        fs::write(
            bp.join("recipes/broken.json"),
            "{\n  \"format_version\": \"1.20.10\"\n  \"x\": 1\n}\n",
        )
        .unwrap();
        fs::write(bp.join("functions/setup.mcfunction"), "say hi\nbad\n").unwrap();
        let output = feed(&[
            "[2026-01-02 03:04:05:678 INFO] Pack Stack - None",
            "[2026-01-02 03:04:05:678 ERROR] [Recipes]  | JSON: recipes/broken.json has an error: ",
            " * Line 3, Column 3",
            "  Missing ',' or '}' in object declaration",
            "",
            "[2026-01-02 03:04:05:678 INFO] Server started.",
            "[2026-01-02 03:04:05:678 WARN] [Commands] Function setup failed to load correctly with error(s):",
            "",
            "[2026-01-02 03:04:05:678 WARN] [Commands] Error on line 2: unknown or invalid command 'bad'",
        ]);
        let roots = [PackRoot {
            kind: Kind::Behavior,
            path: bp.clone(),
            name: "Demo BP".into(),
        }];
        let entries: Vec<LogEntry> = output
            .problems
            .iter()
            .map(|problem| LogEntry {
                area: &problem.area,
                level: &problem.level,
                message: &problem.message,
            })
            .collect();
        let found = log_diagnostics::map(&entries, &roots, "bds");
        assert_eq!(found.len(), 3, "{found:#?}");
        assert_eq!(found[0].file.as_deref(), Some("BP/recipes/broken.json"));
        assert_eq!((found[0].line, found[0].column), (Some(3), Some(3)));
        assert_eq!(found[0].severity, Severity::Error);
        assert_eq!(
            found[2].file.as_deref(),
            Some("BP/functions/setup.mcfunction")
        );
        assert_eq!(found[2].line, Some(2));
        assert_eq!(found[2].severity, Severity::Warning);

        let pack = Pack {
            name: "Demo BP".into(),
            uuid: "AB12".into(),
            version: json!([1, 0, 0]),
            folder: "demo_bp".into(),
            location: "given".into(),
        };
        let user = [(Kind::Behavior, pack, bp)];
        let missing = unloaded_packs(&roots, &user, &output.pack_stack);
        assert_eq!(missing[0].file.as_deref(), Some("BP/manifest.json"));
        assert_eq!(missing[0].rule, "pack-not-loaded");
        let listed = ["[00] Demo BP (id: ab12, version: 1.0.0) @ x".to_owned()];
        assert!(unloaded_packs(&roots, &user, &listed).is_empty());
    }
}
