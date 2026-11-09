//! Action sequence runs. The session cannot move to another thread, so a run shares the
//! request thread: the server starts it after answering (or parking) the call that queued it
//! and answers other tool calls at the run's wait points ([`Game::pause`]). Every run gets a
//! run id and a [`RunLog`]. A background call answers at once with the run id; a blocking
//! call parks ([`Error::Waiting`]) and is answered with the whole result once the run ended,
//! or with the run id once its blocking limit passed while the run goes on in the
//! background. `sequence_status` and `sequence_stop` read the run's log; when they would wait
//! for news they return [`Error::Waiting`], and the server calls them again until they answer.

use std::{
    cell::RefCell,
    collections::VecDeque,
    rc::Rc,
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    ActorOnly, CONFIRM_TIMEOUT, Game, Prepared, Request, actor_name, execute, perform, summary,
    warm_summary,
};
use crate::{
    bridge::Actor,
    error::{Error, Result, bail},
};

/// How many ended runs `sequence_status` still knows.
const KEEP_ENDED: usize = 8;
/// The longest `sequence_status` wait.
const MAX_STATUS_WAIT_MS: u64 = 30_000;

/// Tools that act as one actor, named by their `actor` argument (the connected player
/// without one).
const ACTOR_TOOLS: [&str; 11] = [
    "item_use",
    "item_use_on",
    "attack",
    "entity_interact",
    "block_break",
    "block_place",
    "item_drop",
    "movement_set",
    "look_at",
    "hotbar_select",
    "player_set",
];

/// What a background run has done so far.
#[derive(Debug)]
pub struct RunLog {
    pub id: String,
    /// Every actor the steps name.
    pub actors: Vec<Actor>,
    clock: &'static str,
    /// running, finished, stopped or failed.
    state: &'static str,
    /// Settled step results in the order they settled; status cursors index it.
    pub(super) results: Vec<Value>,
    /// The action that started last and still runs, or null.
    pub(super) current: Value,
    pub(super) failures: u32,
    /// sequence_stop asked the run to stop.
    pub(super) stop_requested: bool,
    /// The whole result once the run ended, as the blocking form returns it.
    result: Option<Value>,
}

impl RunLog {
    pub fn running(&self) -> bool {
        self.state == "running"
    }

    fn actor_names(&self) -> String {
        self.actors
            .iter()
            .map(actor_name)
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The run's end, with the run id and state first; see [`reply`].
    fn ended(&self, verbose: bool) -> Value {
        let mut answer = json!({ "run_id": self.id, "state": self.state });
        if let Some(Value::Object(result)) = &self.result {
            answer
                .as_object_mut()
                .expect("an object")
                .extend(result.clone());
        }
        reply(answer, verbose)
    }

    /// Record how the run ended.
    fn end(&mut self, result: Result<Value>) {
        self.current = Value::Null;
        match result {
            Ok(result) => {
                self.state = match result["stopped"]["by"].as_str() {
                    _ if result["stopped"].is_null() => "finished",
                    Some("fail_fast" | "max_failures") => "failed",
                    _ => "stopped",
                };
                self.result = Some(result);
            }
            Err(error) => {
                self.state = "failed";
                self.result = Some(json!({
                    "clock": self.clock,
                    "error": error.to_string(),
                    "steps": self.results,
                    "stopped": null,
                    "notes": [],
                }));
            }
        }
    }
}

/// A run queued to start once its call is answered.
pub struct Queued {
    log: Rc<RefCell<RunLog>>,
    prepared: Prepared,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusArgs {
    run_id: Option<String>,
    since: Option<usize>,
    wait_ms: Option<u64>,
    #[serde(default)]
    verbose: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopArgs {
    run_id: String,
    #[serde(default)]
    verbose: bool,
}

/// A blocking action_sequence call that waits for its run: the call's arguments (the server
/// calls again with the same ones), its run, when it gives up waiting, and whether it wants
/// the verbose result.
struct Claim {
    arguments: Value,
    log: Rc<RefCell<RunLog>>,
    until: Option<Instant>,
    verbose: bool,
}

/// The session's runs: the one that runs, the one queued to start, the last few that ended,
/// and the blocking call that waits for one.
#[derive(Default)]
pub struct Background {
    next: u32,
    runs: VecDeque<Rc<RefCell<RunLog>>>,
    queued: Option<Queued>,
    claim: Option<Claim>,
    /// Set while the session answers a call at a run's wait point.
    pub nested: bool,
    /// The world key of the connected player's last warm-up; see [`super::warm`].
    pub warm_key: Option<String>,
}

impl Background {
    /// The run that has not ended, if any.
    pub fn active(&self) -> Option<&Rc<RefCell<RunLog>>> {
        self.runs.iter().find(|log| log.borrow().running())
    }

    /// Queue `prepared` to start once this call is answered; the reply names the run and
    /// its plan.
    pub fn queue(&mut self, mut prepared: Prepared) -> Result<Value> {
        if let Some(active) = self.active() {
            let active = active.borrow();
            bail!(
                "Background run {} ({}) is still running. Watch it with sequence_status or end \
                 it with sequence_stop first.",
                active.id,
                active.actor_names()
            );
        }
        self.next += 1;
        let log = Rc::new(RefCell::new(RunLog {
            id: format!("run-{}", self.next),
            actors: prepared.actors(),
            clock: prepared.clock(),
            state: "running",
            results: Vec::new(),
            current: Value::Null,
            failures: 0,
            stop_requested: false,
            result: None,
        }));
        prepared.attach(Rc::clone(&log));
        let reply = json!({
            "run_id": log.borrow().id,
            "clock": prepared.clock(),
            "planned": prepared.plan(),
        });
        while self.runs.len() >= KEEP_ENDED {
            self.runs.pop_front();
        }
        self.runs.push_back(Rc::clone(&log));
        self.queued = Some(Queued { log, prepared });
        Ok(reply)
    }

    pub fn take_queued(&mut self) -> Option<Queued> {
        self.queued.take()
    }

    /// Park a blocking call with `arguments` on the run just queued, for at most `limit`;
    /// `verbose` picks the reply it gets once the run ended (see [`reply`]).
    pub fn claim(&mut self, arguments: Value, limit: Option<Duration>, verbose: bool) {
        if let Some(log) = self.runs.back() {
            self.claim = Some(Claim {
                arguments,
                log: Rc::clone(log),
                until: limit.map(|limit| Instant::now() + limit),
                verbose,
            });
        }
    }

    /// The answer to a blocking call parked with `arguments`: the run's whole result once it
    /// ended, its id once the blocking limit passed, [`Error::Waiting`] meanwhile; `None` when
    /// no call with these arguments is parked.
    pub fn claimed(&mut self, arguments: &Value) -> Option<Result<Value>> {
        let claim = self
            .claim
            .as_ref()
            .filter(|claim| claim.arguments == *arguments)?;
        let (log, until, verbose) = (Rc::clone(&claim.log), claim.until, claim.verbose);
        let log = log.borrow();
        if !log.running() {
            self.claim = None;
            return Some(Ok(log.ended(verbose)));
        }
        if until.is_some_and(|until| Instant::now() >= until) {
            self.claim = None;
            return Some(Ok(json!({
                "run_id": log.id,
                "state": "running",
                "cursor": log.results.len(),
                "note": "The run reached blocking_limit_ms and goes on in the background. Read \
                         its result with sequence_status (wait_ms waits for its end), or end it \
                         with sequence_stop.",
            })));
        }
        Some(Err(Error::Waiting))
    }

    fn find(&self, id: Option<&str>) -> Result<&Rc<RefCell<RunLog>>> {
        let found = match id {
            Some(id) => self.runs.iter().find(|log| log.borrow().id == id),
            None => self.runs.back(),
        };
        found.ok_or_else(|| {
            let known: Vec<String> = self
                .runs
                .iter()
                .map(|log| log.borrow().id.clone())
                .collect();
            Error::msg(format!(
                "No run {}; the server knows {}.",
                id.unwrap_or("yet"),
                match known.is_empty() {
                    true => "none".to_owned(),
                    false => known.join(", "),
                }
            ))
        })
    }

    /// The step results a run (the latest without `run_id`) settled since cursor `since`,
    /// shaped as [`reply`] says; waits (returns [`Error::Waiting`]) while there are none, the
    /// run still runs and `wait_ms` is not 0.
    pub fn status(&self, args: StatusArgs) -> Result<Value> {
        let wait_ms = args.wait_ms.unwrap_or(0);
        if wait_ms > MAX_STATUS_WAIT_MS {
            bail!("wait_ms is at most {MAX_STATUS_WAIT_MS}.");
        }
        let log = self.find(args.run_id.as_deref())?.borrow();
        let since = args.since.unwrap_or(0).min(log.results.len());
        if log.running() && since == log.results.len() && wait_ms > 0 {
            return Err(Error::Waiting);
        }
        let result = log.result.as_ref();
        let mut answer = json!({
            "run_id": log.id,
            "state": log.state,
            "cursor": log.results.len(),
            "steps": log.results[since..],
            "current": log.current,
            "failures": log.failures,
            "stopped": result.map_or(&Value::Null, |result| &result["stopped"]),
            "notes": result.map_or(json!([]), |result| result["notes"].clone()),
        });
        if let Some(result) = result {
            for field in [
                "confirmed",
                "error",
                "summary",
                "recording",
                "timing",
                "warm_up",
            ] {
                if let Some(value) = result.get(field) {
                    answer[field] = value.clone();
                }
            }
        }
        answer["settled"] = (log.results.len() - since).into();
        Ok(reply(answer, args.verbose))
    }

    /// Stop a run as stop_when does and return its whole result; waits (returns
    /// [`Error::Waiting`]) until the run has wound down.
    pub fn stop(&self, args: StopArgs) -> Result<Value> {
        let mut log = self.find(Some(&args.run_id))?.borrow_mut();
        if log.running() {
            log.stop_requested = true;
            return Err(Error::Waiting);
        }
        Ok(log.ended(args.verbose))
    }

    /// Refuse a call made while a run waits that would fight it over an actor's input, or
    /// would hold the run up: another sequence.
    pub fn conflict(&self, tool: &str, arguments: &Value) -> Result<()> {
        let Some(active) = self.active() else {
            return Ok(());
        };
        let active = active.borrow();
        let parked = self
            .claim
            .as_ref()
            .is_some_and(|claim| claim.arguments == *arguments);
        if tool == "action_sequence" && !parked {
            bail!(
                "Background run {} ({}) is still running, and one sequence runs at a time: the \
                 server runs it between other calls. Watch it with sequence_status, end it with \
                 sequence_stop, or put every actor's steps in one sequence.",
                active.id,
                active.actor_names()
            );
        }
        if ACTOR_TOOLS.contains(&tool) {
            let actor = match arguments.get("actor").and_then(Value::as_str) {
                Some(name) => Actor::try_from(name.to_owned()).map_err(Error::msg)?,
                None => Actor::Me,
            };
            if active.actors.contains(&actor) {
                bail!(
                    "{tool} would fight background run {} over {}'s input. Wait for it with \
                     sequence_status or end it with sequence_stop.",
                    active.id,
                    actor_name(&actor)
                );
            }
        }
        Ok(())
    }

    /// The running run that plays the connected player, if any.
    pub fn driving_self(&self) -> Option<String> {
        self.active()
            .map(|log| log.borrow())
            .filter(|log| log.actors.contains(&Actor::Me))
            .map(|log| log.id.clone())
    }
}

/// Run a queued sequence to its end, publishing to its log. When it fails part way, every
/// actor it names gets the stop action, so no control stays held.
pub fn run_queued(game: &mut dyn Game, queued: Queued) {
    let Queued { log, prepared } = queued;
    let result = execute(game, prepared);
    if result.is_err() {
        let actors = log.borrow().actors.clone();
        for actor in actors {
            let _ = perform(game, &Request::Stop(ActorOnly { actor }), CONFIRM_TIMEOUT);
        }
    }
    log.borrow_mut().end(result);
}

/// What a run's reply holds. Both forms add `failed_steps`, one line per step that went wrong
/// (see [`summary::failed_steps`]). The verbose form keeps everything else. The default form
/// leaves out the step entries, `clock` and `confirmed`, keeps the summary's targets and
/// totals, gives the warm-up as `{done, detail}`, and gives each actor `stopped` stopped as
/// `{actor, confirmed, error}`.
fn reply(mut answer: Value, verbose: bool) -> Value {
    answer["failed_steps"] = summary::failed_steps(&answer["steps"]).into();
    if verbose {
        return answer;
    }
    let Value::Object(fields) = &mut answer else {
        return answer;
    };
    for field in ["steps", "clock", "confirmed"] {
        fields.remove(field);
    }
    if let Some(summary) = fields.get_mut("summary") {
        *summary = summary::brief(summary);
    }
    if let Some(warm) = fields.get_mut("warm_up") {
        *warm = warm_summary(warm);
    }
    if let Some(Value::Array(actors)) = fields
        .get_mut("stopped")
        .and_then(|stopped| stopped.get_mut("actors_stopped"))
    {
        for actor in actors {
            let mut line =
                json!({ "actor": actor["actor"], "confirmed": actor["confirmed"] == true });
            if let Some(error) = actor.get("error") {
                line["error"] = error.clone();
            }
            *actor = line;
        }
    }
    answer
}
