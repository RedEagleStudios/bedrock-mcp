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

fn development_dir(kind: Kind) -> &'static str {
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
    if !options.server.join(BINARY).is_file() {
        bail!(
            "No Bedrock Dedicated Server at {} ({BINARY} is missing). Download it from {DOWNLOAD}, \
             unzip it there, or pass server or set {SERVER_SETTING} to its folder.",
            options.server.display()
        );
    }
    let dir = &options.run_dir;
    if dir.exists() {
        fs::remove_dir_all(dir)
            .map_err(|error| Error::msg(format!("Cannot clear {}: {error}", dir.display())))?;
    }
    fs::create_dir_all(dir)?;
    link_server(&options.server, dir)?;
    write_properties(dir)?;
    for kind in [Kind::Behavior, Kind::Resource] {
        fs::create_dir_all(dir.join(development_dir(kind)))?;
    }
    let world = dir.join("worlds").join(LEVEL_NAME);
    fs::create_dir_all(dir.join("worlds"))?;
    let log_path = dir.join("server.log");
    let mut log = BufWriter::new(fs::File::create(&log_path)?);
    let mut notes: Vec<String> = Vec::new();

    let user = user_packs(&options.packs)?;
    match &options.world {
        Some((client, shared)) => {
            copy_dir(&client.path, &world)?;
            fs::write(world.join("levelname.txt"), LEVEL_NAME)?;
            link_world_packs(&world, shared, dir, &user, &mut notes)?;
        }
        None => create_world(dir, &world, &mut log)?,
    }
    prepare_level(&world.join("level.dat"), options.world.is_none())?;
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

    let mut server = Server::start(dir, log)?;
    if !server.wait_started(START_TIMEOUT) {
        let output = server.stop();
        let tail = log_tail(&log_path);
        let reason = if output.exited {
            "exited"
        } else {
            "did not start"
        };
        bail!(
            "The server {reason} before \"Server started.\" (log {}). Last lines:\n{tail}",
            log_path.display()
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
            .errors
            .iter()
            .map(|(area, message)| (area.as_str(), "ERROR", message.as_str())),
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

/// For `gametest run`, the number of runs; the server prints no count for it.
fn single_test_runs(run: &Run) -> Option<usize> {
    run.test.as_ref().map(|_| run.repeat.unwrap_or(1) as usize)
}

/// The given pack folders with their kind; the manifest decides.
fn user_packs(paths: &[PathBuf]) -> Result<Vec<(Kind, Pack, PathBuf)>> {
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

fn copy_dir(source: &Path, target: &Path) -> io::Result<()> {
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
fn create_world(dir: &Path, world: &Path, log: &mut BufWriter<fs::File>) -> Result<()> {
    let first = log.get_ref().try_clone()?;
    let mut server = Server::start(dir, BufWriter::new(first))?;
    let started = server.wait_started(START_TIMEOUT);
    server.stop();
    let _ = writeln!(log, "--- world created; test run follows ---");
    if !started || !world.join("level.dat").is_file() {
        bail!(
            "The server did not create a world in {} (log {}). Last lines:\n{}",
            world.display(),
            dir.join("server.log").display(),
            log_tail(&dir.join("server.log"))
        );
    }
    fs::remove_dir_all(world.join("db"))?;
    Ok(())
}

/// Turn on the Beta APIs experiment, and the flat generator for a fresh world.
fn prepare_level(path: &Path, flat: bool) -> Result<()> {
    let data = fs::read(path)
        .map_err(|error| Error::msg(format!("Cannot read {}: {error}", path.display())))?;
    let mut level = LevelDat::parse(&data)
        .map_err(|error| Error::msg(format!("{}: {error}", path.display())))?;
    let experiments = level.root.compound_mut("experiments");
    for key in BETA_EXPERIMENTS {
        experiments.set(key, Tag::Byte(1));
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
    /// ERROR entries as (area, message), continuation lines joined.
    errors: Vec<(String, String)>,
    pending: Option<(String, String)>,
    exited: bool,
    exited_cleanly: bool,
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
            && level == "ERROR"
        {
            let (area, message) = split_area(&message);
            self.errors.push((area.to_owned(), message.to_owned()));
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
        let errors_before = self.output.errors.len();
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
                if let Some((_, message)) = self.output.errors.get(errors_before) {
                    return Outcome::Answer(message.clone());
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
        assert_eq!(
            output.errors,
            vec![
                ("Scripting".into(), "boom\n    at main.js:1".into()),
                (String::new(), "No tests found for tag 'x'".into()),
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
        prepare_level(&path, true).unwrap();
        let level = LevelDat::parse(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(level.root.get("Generator"), Some(&Tag::Int(FLAT_GENERATOR)));
        let Some(Tag::Compound(experiments)) = level.root.get("experiments") else {
            panic!("experiments");
        };
        for key in BETA_EXPERIMENTS {
            assert_eq!(experiments.get(key), Some(&Tag::Byte(1)));
        }
    }
}
