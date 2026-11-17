//! GameTests on a Bedrock Dedicated Server (BDS) with no client: a scratch server folder that
//! links the user's server files, a fresh or copied world with the Beta APIs experiment, and
//! a console driver that runs `/gametest` and reads the results from the server's output.
//! A call can hold several runs, and a run can split its tests across shards; every run and
//! shard gets its own server, started in parallel up to what the machine can hold.

mod dirs;
mod discover;
mod plan;

use std::{
    collections::VecDeque,
    env, fs,
    io::{self, BufRead, BufReader, BufWriter, Read, Write},
    net::UdpSocket,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Mutex,
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use self::dirs::RunDir;
pub use self::{
    dirs::{Kind as RunKind, RunDirs},
    plan::Capacity,
};
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
/// Between a shard's steps: long enough for a last result line, short enough to keep pace
/// with `runset`, which starts the next batch at once.
const STEP_SETTLE: Duration = Duration::from_secs(1);
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

/// One `gametest_server` run: a world with its packs and the tests to run in it.
pub struct RunSpec {
    pub label: Option<String>,
    pub packs: Vec<PathBuf>,
    /// A client world to copy, and the client's `com.mojang` folder its packs come from.
    pub world: Option<(WorldFolder, PathBuf)>,
    pub commands: Vec<String>,
    pub run: Run,
    /// Servers to split the tests across; 1 runs them on one server.
    pub shards: usize,
}

/// A `gametest_server` call.
pub struct Options {
    pub server: PathBuf,
    pub dirs: RunDirs,
    pub runs: Vec<RunSpec>,
    /// The call used the single-run arguments, so the reply is that run's own.
    pub single: bool,
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

/// Run every run's tests, each run or shard on its own server, and report; see the
/// `gametest_server` tool for the reply.
pub fn run(options: &Options) -> Result<Value> {
    let began = Instant::now();
    let capacity = Capacity::read();
    for spec in &options.runs {
        gametest::run_command(&spec.run)?;
        if let Some(bad) = spec
            .commands
            .iter()
            .find(|line| line.contains(['\n', '\r']))
        {
            bail!("Console command {bad:?} spans several lines; give one command per entry.");
        }
        if spec.shards == 0 {
            bail!("shards must be at least 1.");
        }
    }
    let plans: Vec<RunPlan> = options.runs.iter().map(plan_run).collect();
    let jobs: Vec<(usize, usize)> = plans
        .iter()
        .enumerate()
        .flat_map(|(run, plan)| (0..plan.servers()).map(move |shard| (run, shard)))
        .collect();
    let mut notes = request_notes(jobs.len(), capacity)?;
    let mut served: Vec<Vec<Option<Result<Served>>>> = plans
        .iter()
        .map(|plan| (0..plan.servers()).map(|_| None).collect())
        .collect();
    let finished = parallel(jobs, capacity.max_servers, |(run, shard)| {
        let spec = &options.runs[run];
        let tests = plans[run]
            .shards
            .as_ref()
            .map(|sharded| (&sharded.plans[shard], sharded.cell));
        let dir = options.dirs.next(RunKind::Gametest);
        ((run, shard), serve(options, spec, tests, &dir))
    });
    for ((run, shard), result) in finished {
        served[run][shard] = Some(result);
    }
    options.dirs.tidy(RunKind::Gametest);
    let mut replies = Vec::new();
    for ((spec, plan), shards) in options.runs.iter().zip(&plans).zip(served) {
        let shards: Vec<Result<Served>> = shards
            .into_iter()
            .map(|result| result.expect("every job ran"))
            .collect();
        let reply = match &plan.shards {
            Some(sharded) => Ok(sharded_reply(options, spec, plan, sharded, shards, began)),
            None => {
                let served = shards.into_iter().next().expect("one server");
                served.map(|served| run_reply(options, spec, plan, &served, began))
            }
        };
        replies.push(reply);
    }
    if options.single {
        let mut reply = replies.pop().expect("one run")?;
        reply["capacity"] = capacity.to_json();
        if let Some(run_notes) = reply["notes"].as_array_mut() {
            run_notes.extend(notes.into_iter().map(Value::String));
        }
        return Ok(reply);
    }
    let runs: Vec<Value> = options
        .runs
        .iter()
        .zip(replies)
        .map(|(spec, reply)| labeled(&spec.label, reply))
        .collect();
    notes.extend(failed_runs(&runs));
    Ok(json!({
        "server": { "path": options.server },
        "runs": runs,
        "elapsed_ms": began.elapsed().as_millis() as u64,
        "capacity": capacity.to_json(),
        "notes": notes,
    }))
}

/// Refuse a call that asks for more servers than [`plan::MAX_REQUESTED`], and say when the
/// servers run in waves.
fn request_notes(servers: usize, capacity: Capacity) -> Result<Vec<String>> {
    if servers > plan::MAX_REQUESTED {
        bail!(
            "The call asks for {servers} servers; one call may start at most {}, and this \
             machine runs {} at a time. Split it into smaller calls or use fewer shards. \
             capacity: {}",
            plan::MAX_REQUESTED,
            capacity.max_servers,
            capacity.to_json()
        );
    }
    Ok(if servers > capacity.max_servers {
        vec![format!(
            "The call needs {servers} servers but this machine runs at most {} at a time \
             (capacity), so they ran in waves; later servers started when earlier ones stopped.",
            capacity.max_servers
        )]
    } else {
        Vec::new()
    })
}

/// A run's reply with its label first, or its error.
fn labeled(label: &Option<String>, reply: Result<Value>) -> Value {
    let mut entry = json!({ "label": label });
    match reply {
        Ok(Value::Object(fields)) => {
            if let Value::Object(entry) = &mut entry {
                entry.extend(fields);
            }
        }
        Ok(other) => entry["reply"] = other,
        Err(error) => entry["error"] = json!(error.to_string()),
    }
    entry
}

fn failed_runs(runs: &[Value]) -> Vec<String> {
    let failed = runs.iter().filter(|run| run.get("error").is_some()).count();
    if failed == 0 {
        return Vec::new();
    }
    vec![format!(
        "{failed} of {} runs could not run; see their error.",
        runs.len()
    )]
}

/// Run `work` on every item, at most `limit` at a time, and return the results in item order.
fn parallel<T: Send, R: Send>(items: Vec<T>, limit: usize, work: impl Fn(T) -> R + Sync) -> Vec<R> {
    let workers = limit.clamp(1, items.len().max(1));
    let count = items.len();
    let queue = Mutex::new(items.into_iter().enumerate().collect::<VecDeque<_>>());
    let results = Mutex::new((0..count).map(|_| None).collect::<Vec<Option<R>>>());
    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let next = queue.lock().expect("queue").pop_front();
                    let Some((index, item)) = next else {
                        break;
                    };
                    let result = work(item);
                    results.lock().expect("results")[index] = Some(result);
                }
            });
        }
    });
    results
        .into_inner()
        .expect("results")
        .into_iter()
        .map(|result| result.expect("every item ran"))
        .collect()
}

/// How a run uses its servers.
struct RunPlan {
    /// Set when the run's tests split across shards.
    shards: Option<Sharded>,
    /// How the tests were found, for a run that asked for shards.
    discovery: Option<Value>,
    notes: Vec<String>,
}

impl RunPlan {
    fn servers(&self) -> usize {
        self.shards
            .as_ref()
            .map_or(1, |sharded| sharded.tests.len())
    }
}

struct Sharded {
    /// The selected tests in registration order.
    order: Vec<String>,
    /// Each shard's test ids.
    tests: Vec<Vec<String>>,
    /// Each shard's steps.
    plans: Vec<plan::ShardPlan>,
    /// Blocks between neighbouring test structures.
    cell: i32,
}

/// Find the run's tests and split them when it asks for shards; otherwise, or when the tests
/// cannot all be known, one server runs the usual `/gametest` command.
fn plan_run(spec: &RunSpec) -> RunPlan {
    let whole = |discovery: Option<Value>, note: String| RunPlan {
        shards: None,
        discovery,
        notes: vec![note],
    };
    if spec.shards <= 1 {
        return RunPlan {
            shards: None,
            discovery: None,
            notes: Vec::new(),
        };
    }
    if spec.run.test.is_some() {
        return whole(
            None,
            "shards splits a set of tests; a single test ran on one server.".into(),
        );
    }
    if spec.run.until_fail {
        return whole(
            None,
            "until_fail stops the set at its first failure, which needs one server, so shards \
             was ignored."
                .into(),
        );
    }
    let packs = behavior_pack_folders(spec);
    let found = discover::discover(&packs);
    let tag = spec.run.tag.as_deref().unwrap_or("suite:all");
    let selected: Vec<discover::Registered> = found
        .tests
        .iter()
        .filter(|test| test.matches(tag))
        .cloned()
        .collect();
    let mut discovery = json!({
        "method": "static",
        "files": found.files,
        "registered": found.tests.len(),
        "selected": selected.len(),
        "unresolved": found.unresolved,
    });
    let unknown_tags = tag != "suite:all" && found.tests.iter().any(|test| test.unknown_chain);
    if found.truncated || !found.unresolved.is_empty() || unknown_tags {
        discovery["complete"] = json!(false);
        return whole(
            Some(discovery),
            format!(
                "The pack scripts register tests whose {} not string literals, so the full list \
                 is unknown; the run used one server instead of {} shards.",
                if unknown_tags {
                    "tags are"
                } else if found.truncated {
                    "files exceed the scan's bounds or whose names are"
                } else {
                    "names are"
                },
                spec.shards
            ),
        );
    }
    discovery["complete"] = json!(true);
    if selected.len() < 2 {
        return whole(
            Some(discovery),
            format!(
                "The pack scripts register {} matching test{}, so the run used one server.",
                selected.len(),
                if selected.len() == 1 { "" } else { "s" }
            ),
        );
    }
    let plans = plan::plan_shards(&selected, spec.shards);
    let tests: Vec<Vec<String>> = plans.iter().map(plan::ShardPlan::tests).collect();
    let mut notes = Vec::new();
    if tests.len() < spec.shards {
        notes.push(format!(
            "{} shards were asked for but the tests form only {} groups (tests of one batch \
             stay together), so {} servers ran.",
            spec.shards,
            tests.len(),
            tests.len()
        ));
    }
    RunPlan {
        shards: Some(Sharded {
            order: selected.into_iter().map(|test| test.id).collect(),
            tests,
            plans,
            cell: plan::cell_size(&packs),
        }),
        discovery: Some(discovery),
        notes,
    }
}

/// The behavior pack folders a run loads: the given ones and those its copied world applies.
fn behavior_pack_folders(spec: &RunSpec) -> Vec<PathBuf> {
    let mut folders: Vec<PathBuf> = user_packs(&spec.packs)
        .unwrap_or_default()
        .into_iter()
        .filter(|(kind, _, _)| *kind == Kind::Behavior)
        .map(|(_, _, path)| path)
        .collect();
    if let Some((world, shared)) = &spec.world {
        let kind = Kind::Behavior;
        let own_dir = world.path.join(kind.world_dir());
        let own = packs::packs_in(&own_dir, "world", kind);
        let installed = packs::installed_packs(shared, kind);
        for entry in packs::read_pack_list(&world.path.join(kind.world_list())).unwrap_or_default()
        {
            let matches = |pack: &&Pack| pack.uuid.eq_ignore_ascii_case(&entry.pack_id);
            if let Some(pack) = own.iter().find(matches) {
                folders.push(own_dir.join(&pack.folder));
            } else if let Some(pack) = installed.iter().find(matches) {
                folders.push(shared.join(&pack.location).join(&pack.folder));
            }
        }
    }
    folders
}

/// What one server printed and did.
struct Served {
    output: Output,
    outcome: Outcome,
    command: String,
    command_replies: Vec<Value>,
    notes: Vec<String>,
    log: PathBuf,
    started: Instant,
    ended: Instant,
}

/// Start one server in `dir`, run the commands and the tests (all of the run's, or one shard's
/// share placed on a grid of `cell` blocks), stop it and remove its scratch folder.
fn serve(
    options: &Options,
    spec: &RunSpec,
    shard: Option<(&plan::ShardPlan, i32)>,
    dir: &RunDir,
) -> Result<Served> {
    let result = serve_in(options, spec, shard, dir);
    let _ = fs::remove_dir_all(&dir.scratch);
    result
}

fn serve_in(
    options: &Options,
    spec: &RunSpec,
    shard: Option<(&plan::ShardPlan, i32)>,
    dir: &RunDir,
) -> Result<Served> {
    let started = Instant::now();
    let Scratch {
        log,
        user: _,
        notes,
    } = prepare_scratch(&ScratchSpec {
        server: &options.server,
        dir: &dir.scratch,
        log_path: &dir.log,
        packs: &spec.packs,
        world: spec.world.as_ref(),
        beta_apis: true,
        timeout: START_TIMEOUT,
    })?;
    let mut server = Server::start(&dir.scratch, log)?;
    if !server.wait_started(START_TIMEOUT) {
        let output = server.stop();
        bail!(
            "The server {} before \"Server started.\" (log {}). Last lines:\n{}",
            if output.exited {
                "exited"
            } else {
                "did not start"
            },
            dir.log.display(),
            log_tail(&dir.log)
        );
    }
    for rule in GAMERULES {
        server.command(rule);
    }
    let command_replies: Vec<Value> = spec
        .commands
        .iter()
        .map(|line| json!({ "command": line, "reply": server.command(line) }))
        .collect();
    let limits = Limits {
        timeout: options.timeout,
        idle: options.idle,
        stop_on_failure: spec.run.stop_on_failure == Some(true),
        settle: SETTLE,
    };
    let (command, outcome) = match shard {
        Some((shard, cell)) => {
            let steps = shard_commands(shard, cell, spec.run.rotation);
            let tests_started = Instant::now();
            let mut outcome = Outcome::Done;
            let mut expected = 0;
            for (index, step) in steps.iter().enumerate() {
                expected += step.len();
                let last = index + 1 == steps.len();
                let step_limits = Limits {
                    timeout: options.timeout.saturating_sub(tests_started.elapsed()),
                    settle: if last { SETTLE } else { STEP_SETTLE },
                    ..limits
                };
                outcome = server.run_tests(step, Expect::Tests(expected), &step_limits);
                if !matches!(outcome, Outcome::Done) {
                    break;
                }
            }
            (
                "gametest run <test> for each test of the shard, each structure in its own grid \
                 cell; tests without a batch together, then each batch in turn"
                    .to_owned(),
                outcome,
            )
        }
        None => {
            let command = gametest::run_command(&spec.run)?;
            let expect = match single_test_runs(&spec.run) {
                Some(runs) => Expect::Lines(runs),
                None => Expect::FromServer,
            };
            let outcome = server.run_tests(std::slice::from_ref(&command), expect, &limits);
            (command, outcome)
        }
    };
    let output = server.stop();
    Ok(Served {
        output,
        outcome,
        command,
        command_replies,
        notes,
        log: dir.log.clone(),
        started,
        ended: Instant::now(),
    })
}

/// `/gametest run` places every test at the same spot next to the command's origin, so each
/// test of a shard gets its own grid cell through `execute positioned`. One list per step.
fn shard_commands(shard: &plan::ShardPlan, cell: i32, rotation: Option<u8>) -> Vec<Vec<String>> {
    let rotation = rotation
        .map(|turns| format!(" {turns}"))
        .unwrap_or_default();
    let mut cells = plan::grid(shard.steps.iter().map(Vec::len).sum(), cell).into_iter();
    shard
        .steps
        .iter()
        .map(|step| {
            step.iter()
                .zip(cells.by_ref())
                .map(|(test, (x, z))| {
                    format!("execute positioned ~{x} ~ ~{z} run gametest run {test}{rotation}")
                })
                .collect()
        })
        .collect()
}

fn outcome_note(outcome: &Outcome, command: &str, options: &Options) -> Option<String> {
    match outcome {
        Outcome::Done => None,
        Outcome::Answer(message) => {
            Some(format!("The server answered {command:?} with: {message}"))
        }
        Outcome::Idle => Some(format!(
            "No test made progress for {} ms, so the run stopped early (idle_ms).",
            options.idle.as_millis()
        )),
        Outcome::Timeout => Some(format!(
            "The tests did not finish within {} ms (timeout_ms).",
            options.timeout.as_millis()
        )),
        Outcome::Exited => Some("The server exited during the run; see log.".into()),
    }
}

fn error_groups<'a>(problems: impl IntoIterator<Item = &'a Problem>) -> Vec<Value> {
    content_log::summarize(
        problems
            .into_iter()
            .filter(|problem| problem.level == "ERROR")
            .map(|problem| (problem.area.as_str(), "ERROR", problem.message.as_str())),
    )
}

fn result_json(result: &TestResult) -> Value {
    json!({ "test": result.test, "passed": result.passed, "message": result.message })
}

fn failed_json(results: &[Value]) -> Vec<Value> {
    results
        .iter()
        .filter(|result| result["passed"] == false)
        .map(|result| json!({ "test": result["test"], "message": result["message"] }))
        .collect()
}

fn empty_stack_note(pack_stack: &[String], spec: &RunSpec) -> Option<String> {
    (pack_stack.iter().any(|line| line.ends_with("None")) && !spec.packs.is_empty()).then(|| {
        "The pack stack is empty although packs were given: the server may be older than the \
         packs' min_engine_version."
            .into()
    })
}

const NO_TESTS: &str = "No test ran. Check that the pack registers tests with that id or tag and \
                        that the pack stack lists the pack; errors show why a pack did not load.";

/// The reply for a run on one server.
fn run_reply(
    options: &Options,
    spec: &RunSpec,
    plan: &RunPlan,
    served: &Served,
    began: Instant,
) -> Value {
    let output = &served.output;
    let mut notes = served.notes.clone();
    notes.extend(plan.notes.iter().cloned());
    let expected = match served.outcome {
        Outcome::Answer(_) => 0,
        _ => single_test_runs(&spec.run).or(output.expected).unwrap_or(0),
    };
    let results: Vec<Value> = output.results.iter().map(result_json).collect();
    let failed = failed_json(&results);
    let passed = output.results.iter().filter(|result| result.passed).count();
    notes.extend(outcome_note(&served.outcome, &served.command, options));
    if expected == 0 {
        notes.push(NO_TESTS.into());
    } else {
        let got = match single_test_runs(&spec.run) {
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
    notes.extend(empty_stack_note(&output.pack_stack, spec));
    if !output.exited_cleanly {
        notes.push("The server did not quit after stop and was killed.".into());
    }
    let mut reply = json!({
        "server": { "path": options.server, "version": output.version },
        "command": served.command,
        "expected": expected,
        "passed": passed,
        "failed": failed,
        "results": results,
        "timed_out": matches!(served.outcome, Outcome::Idle | Outcome::Timeout),
        "errors": error_groups(&output.problems),
        "pack_stack": output.pack_stack,
        "command_replies": served.command_replies,
        "log": served.log,
        "elapsed_ms": served.ended.duration_since(began).as_millis() as u64,
        "notes": notes,
    });
    if let Some(discovery) = &plan.discovery {
        reply["discovery"] = discovery.clone();
    }
    reply
}

/// Why a shard left tests without a result, if it did.
fn shard_failure(served: &Result<Served>, options: &Options) -> Option<String> {
    match served {
        Err(error) => Some(error.to_string()),
        Ok(served) => outcome_note(&served.outcome, &served.command, options),
    }
}

/// One shard's share and what came of it.
struct ShardResults<'a> {
    tests: &'a [String],
    results: &'a [TestResult],
    failure: Option<&'a str>,
}

/// Every test's result in `order`, from the shard that ran it; a test its shard left without
/// a result fails with the shard's reason and `errored: true`. Results for tests outside the
/// plan come last.
fn merge_results(order: &[String], shards: &[ShardResults]) -> Vec<Value> {
    let mut merged = Vec::new();
    for test in order {
        let Some((index, shard)) = shards
            .iter()
            .enumerate()
            .find(|(_, shard)| shard.tests.contains(test))
        else {
            continue;
        };
        match shard.results.iter().find(|result| &result.test == test) {
            Some(result) => merged.push(result_json(result)),
            None => merged.push(json!({
                "test": test,
                "passed": false,
                "message": format!(
                    "No result from shard {}: {}",
                    index + 1,
                    shard.failure.unwrap_or("the server reported none for this test")
                ),
                "errored": true,
            })),
        }
    }
    for shard in shards {
        for result in shard.results {
            if !order.contains(&result.test) {
                merged.push(result_json(result));
            }
        }
    }
    merged
}

/// The reply for a run split across shards, in the one-server shape plus `shards`.
fn sharded_reply(
    options: &Options,
    spec: &RunSpec,
    plan: &RunPlan,
    sharded: &Sharded,
    shards: Vec<Result<Served>>,
    began: Instant,
) -> Value {
    let failures: Vec<Option<String>> = shards
        .iter()
        .map(|served| shard_failure(served, options))
        .collect();
    let shard_results: Vec<ShardResults> = shards
        .iter()
        .zip(&sharded.tests)
        .zip(&failures)
        .map(|((served, tests), failure)| ShardResults {
            tests,
            results: served
                .as_ref()
                .map_or(&[][..], |served| &served.output.results),
            failure: failure.as_deref(),
        })
        .collect();
    let results = merge_results(&sharded.order, &shard_results);
    let failed = failed_json(&results);
    let passed = results
        .iter()
        .filter(|result| result["passed"] == true)
        .count();
    let ok: Vec<&Served> = shards
        .iter()
        .filter_map(|served| served.as_ref().ok())
        .collect();
    let first = ok.first();
    let mut notes = plan.notes.clone();
    if let Some(served) = first {
        notes.extend(served.notes.iter().cloned());
    }
    let missing = results
        .iter()
        .filter(|result| result["errored"] == true)
        .count();
    if missing > 0 {
        notes.push(format!(
            "{missing} of {} tests have no result; see their message and shards.",
            sharded.order.len()
        ));
    }
    if !failed.is_empty() {
        notes.push(format!("{} tests failed.", failed.len()));
    }
    if let Some(served) = first {
        notes.extend(empty_stack_note(&served.output.pack_stack, spec));
    }
    if ok.iter().any(|served| !served.output.exited_cleanly) {
        notes.push("A shard's server did not quit after stop and was killed.".into());
    }
    let shard_list: Vec<Value> = shards
        .iter()
        .zip(&sharded.tests)
        .zip(&failures)
        .enumerate()
        .map(|(index, ((served, tests), failure))| {
            let (duration, log, errors) = match served {
                Ok(served) => (
                    Some(served.ended.duration_since(served.started).as_millis() as u64),
                    Some(served.log.clone()),
                    served
                        .output
                        .problems
                        .iter()
                        .filter(|problem| problem.level == "ERROR")
                        .count(),
                ),
                Err(_) => (None, None, 0),
            };
            json!({
                "index": index + 1,
                "tests": tests,
                "duration_ms": duration,
                "log": log,
                "errors": errors,
                "failure": failure,
            })
        })
        .collect();
    let ended = ok.iter().map(|served| served.ended).max().unwrap_or(began);
    json!({
        "server": {
            "path": options.server,
            "version": first.and_then(|served| served.output.version.clone()),
        },
        "command": first.map_or_else(String::new, |served| served.command.clone()),
        "expected": sharded.order.len(),
        "passed": passed,
        "failed": failed,
        "results": results,
        "timed_out": ok.iter().any(|served| matches!(served.outcome, Outcome::Idle | Outcome::Timeout)),
        "errors": error_groups(ok.iter().flat_map(|served| &served.output.problems)),
        "pack_stack": first.map_or(&[][..], |served| &served.output.pack_stack),
        "command_replies": first.map_or(&[][..], |served| &served.command_replies),
        "log": Value::Null,
        "elapsed_ms": ended.duration_since(began).as_millis() as u64,
        "notes": notes,
        "discovery": plan.discovery,
        "shards": shard_list,
    })
}

/// One `bds_check` run: packs to load in a fresh flat world.
pub struct CheckSpec {
    pub label: Option<String>,
    pub packs: Vec<PathBuf>,
    pub beta_apis: bool,
}

/// A `bds_check` call.
pub struct CheckOptions {
    pub server: PathBuf,
    pub dirs: RunDirs,
    pub runs: Vec<CheckSpec>,
    /// The call used the single-run arguments, so the reply is that run's own.
    pub single: bool,
    /// Each check: world creation, start and the quiet wait.
    pub timeout: Duration,
}

/// Check every run's packs, each on its own server started in parallel; see the `bds_check`
/// tool for the reply.
pub fn check(options: &CheckOptions) -> Result<Value> {
    let began = Instant::now();
    let capacity = Capacity::read();
    if options.runs.iter().any(|spec| spec.packs.is_empty()) {
        bail!("Give at least one pack folder to check in every run.");
    }
    let mut notes = request_notes(options.runs.len(), capacity)?;
    let mut replies = parallel(
        options.runs.iter().collect(),
        capacity.max_servers,
        |spec| {
            let started = Instant::now();
            let dir = options.dirs.next(RunKind::Check);
            let prepared = prepare_scratch(&ScratchSpec {
                server: &options.server,
                dir: &dir.scratch,
                log_path: &dir.log,
                packs: &spec.packs,
                world: None,
                beta_apis: spec.beta_apis,
                timeout: options.timeout,
            });
            let job = CheckJob {
                server: &options.server,
                dir: &dir,
                timeout: options.timeout,
            };
            let result = prepared.and_then(|scratch| check_loaded(&job, scratch, started));
            let _ = fs::remove_dir_all(&dir.scratch);
            result
        },
    );
    options.dirs.tidy(RunKind::Check);
    if options.single {
        let mut reply = replies.pop().expect("one run")?;
        reply["capacity"] = capacity.to_json();
        if let Some(run_notes) = reply["notes"].as_array_mut() {
            run_notes.extend(notes.into_iter().map(Value::String));
        }
        return Ok(reply);
    }
    let runs: Vec<Value> = options
        .runs
        .iter()
        .zip(replies)
        .map(|(spec, reply)| labeled(&spec.label, reply))
        .collect();
    notes.extend(failed_runs(&runs));
    Ok(json!({
        "server": { "path": options.server },
        "runs": runs,
        "elapsed_ms": began.elapsed().as_millis() as u64,
        "capacity": capacity.to_json(),
        "notes": notes,
    }))
}

/// One load check's server.
struct CheckJob<'a> {
    server: &'a Path,
    dir: &'a RunDir,
    timeout: Duration,
}

fn check_loaded(job: &CheckJob, scratch: Scratch, began: Instant) -> Result<Value> {
    let Scratch {
        log,
        user,
        mut notes,
    } = scratch;
    let mut server = Server::start(&job.dir.scratch, log)?;
    let remaining = job.timeout.saturating_sub(began.elapsed());
    let started = server.wait_started(remaining);
    let timed_out = if started {
        !server.wait_quiet(
            CHECK_QUIET,
            CHECK_AFTER_START.min(job.timeout.saturating_sub(began.elapsed())),
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
            log_tail(&job.dir.log)
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
        "server": { "path": job.server, "version": output.version },
        "started": started,
        "timed_out": timed_out,
        "diagnostics": diagnostics,
        "counts": log_diagnostics::counts(&diagnostics),
        "pack_stack": output.pack_stack,
        "log": job.dir.log,
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

/// When a run's results are complete.
#[derive(Clone, Copy)]
enum Expect {
    /// `gametest run` of one test: this many result lines, one per repeat.
    Lines(usize),
    /// One `gametest run` per test: a result for this many distinct tests.
    Tests(usize),
    /// `runset`: as many tests as the server says it runs.
    FromServer,
}

/// How long tests may take, and whether a repeated test stops at its first failure.
struct Limits {
    timeout: Duration,
    idle: Duration,
    stop_on_failure: bool,
    /// How long output must stay quiet after the last result.
    settle: Duration,
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

    /// Send the gametest commands and read until the results are complete, the tests stall or
    /// the time is up.
    fn run_tests(&mut self, commands: &[String], expect: Expect, limits: &Limits) -> Outcome {
        let problems_before = self.output.problems.len();
        for command in commands {
            self.send(command);
        }
        let sent = Instant::now();
        let deadline = sent + limits.timeout;
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
            // With one command per test, an error answers only one of them.
            if !matches!(expect, Expect::Tests(_))
                && self.output.activity == 0
                && now - sent <= ANSWER_WINDOW + TICK
            {
                self.output.flush();
                if let Some(problem) = self.output.problems[problems_before..]
                    .iter()
                    .find(|problem| problem.level == "ERROR")
                {
                    return Outcome::Answer(problem.message.clone());
                }
            }
            let finished = match expect {
                Expect::Lines(runs) => self.output.result_lines >= runs,
                Expect::Tests(tests) => self.output.results.len() >= tests,
                Expect::FromServer => self
                    .output
                    .expected
                    .is_some_and(|expected| self.output.results.len() >= expected),
            };
            let stopped = matches!(expect, Expect::Lines(_))
                && limits.stop_on_failure
                && self.output.results.iter().any(|result| !result.passed);
            if (finished || stopped) && !results_changed && now - progress >= limits.settle {
                return Outcome::Done;
            }
            if now - progress >= limits.idle {
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
    fn shard_results_merge_in_registration_order() {
        let ids = |names: &[&str]| -> Vec<String> { names.iter().map(|n| n.to_string()).collect() };
        let result = |test: &str, passed: bool| TestResult {
            test: test.into(),
            passed,
            message: if passed {
                String::new()
            } else {
                "on purpose".into()
            },
        };
        let order = ids(&["demo:a", "demo:b", "demo:c", "demo:d"]);
        let first = ids(&["demo:a", "demo:c"]);
        let second = ids(&["demo:b", "demo:d"]);
        let first_results = [result("demo:c", false), result("demo:a", true)];
        let second_results = [result("demo:b", true), result("demo:extra", true)];
        let merged = merge_results(
            &order,
            &[
                ShardResults {
                    tests: &first,
                    results: &first_results,
                    failure: None,
                },
                ShardResults {
                    tests: &second,
                    results: &second_results,
                    failure: Some("The tests did not finish within 10 ms (timeout_ms)."),
                },
            ],
        );
        let tests: Vec<&str> = merged.iter().map(|r| r["test"].as_str().unwrap()).collect();
        assert_eq!(
            tests,
            ["demo:a", "demo:b", "demo:c", "demo:d", "demo:extra"]
        );
        assert_eq!(merged[2]["passed"], false);
        assert_eq!(merged[3]["errored"], true);
        assert!(
            merged[3]["message"]
                .as_str()
                .unwrap()
                .starts_with("No result from shard 2: The tests did not finish")
        );
        assert_eq!(failed_json(&merged).len(), 2);
    }

    #[test]
    fn requests_beyond_capacity_run_in_waves_or_are_refused() {
        let capacity = Capacity::from_parts(8, None, Some(16_000));
        assert_eq!(capacity.max_servers, 4);
        assert!(request_notes(4, capacity).unwrap().is_empty());
        assert!(request_notes(6, capacity).unwrap()[0].contains("waves"));
        let refused = request_notes(plan::MAX_REQUESTED + 1, capacity).unwrap_err();
        assert!(refused.to_string().contains("max_servers"));
    }

    #[test]
    fn shard_commands_place_tests_on_a_grid() {
        let shard = plan::ShardPlan {
            steps: vec![
                vec!["demo:a".to_owned(), "demo:b".to_owned()],
                vec!["demo:n".to_owned()],
            ],
        };
        assert_eq!(
            shard_commands(&shard, 24, Some(1)),
            [
                vec![
                    "execute positioned ~0 ~ ~0 run gametest run demo:a 1",
                    "execute positioned ~24 ~ ~0 run gametest run demo:b 1",
                ],
                vec!["execute positioned ~0 ~ ~24 run gametest run demo:n 1"],
            ]
        );
    }

    #[test]
    fn parallel_keeps_item_order() {
        let doubled = parallel((1..=9).collect(), 3, |n: u64| {
            thread::sleep(Duration::from_millis(10 - n));
            n * 2
        });
        assert_eq!(doubled, (1..=9).map(|n| n * 2).collect::<Vec<_>>());
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
