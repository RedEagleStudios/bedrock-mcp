//! The GameTest driver: a simulated player, acted on through the GameTest pack's
//! `SimulatedPlayer` method calls. The method's result confirms the call; with the Bridge
//! pack the event the action causes must follow too, and without it vanilla command
//! readbacks stand in where they can.
//!
//! Every action sends its first method call and finishes in [`Pending`] checks, so a sequence
//! can start its next step while one still runs. With a scheduled tick the first call is sent
//! ahead and the pack runs it on that tick, so slow command round trips do not delay it; the
//! checks that need the call's reply (reach, readbacks) run in the pack or afterwards.

use std::{
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use super::{
    Aim, Confirmation, Destination, Expect, Game, Hand, Movement, Outcome, Pending, Quantity,
    UseMode, beside, block_of, block_vec3, confirm_after, entity_point, face_point, first,
    horizontal, point_of, rounded, vec3,
};
use crate::{
    bridge::{Actor, friendly, quoted},
    error::{Error, Result, bail},
    events::{EventCursor, HubEvent},
    gametest::{self, Action, Face, LookDuration, Reach},
    world::{World, direction, distance, facing},
};

const DEFAULT_ARRIVAL: f64 = 1.0;
const DEFAULT_TRAVEL_MS: u64 = 20_000;
/// How far above or below the target still counts as arrived, beyond arrival_radius.
const VERTICAL_SLACK: f64 = 1.0;
/// How long a travelling simulated player may stand still before it counts as stuck.
const STUCK: Duration = Duration::from_secs(2);
/// How far it has to move to count as moving on.
const STIRRED: f64 = 0.3;
/// How often the driver checks a running action while it waits for it.
const POLL: Duration = Duration::from_millis(25);
/// How long movement keeps going before it is checked when it has no duration.
const PROBE: Duration = Duration::from_millis(250);
const MOVED: f64 = 0.05;
/// Eye height of a standing player, for packs that do not report the eye.
const EYE_HEIGHT: f64 = 1.62;
/// How far a simulated player may reach an entity, in blocks; the pack measures from the
/// eyes to the entity's body when it runs the attack or interaction. SimulatedPlayer
/// attacks at any distance (Microsoft Learn's SimulatedPlayer reference), so the server
/// checks reach. The values match the Bedrock entity reach on
/// https://minecraft.wiki/w/Interaction_range.
const REACH: Reach = Reach {
    survival: 3.0,
    creative: 5.0,
};
/// How far the server takes a simulated player to reach a block, in survival and in creative.
/// These are our limits: an unconfirmed block action farther away gets a note that blames
/// reach. https://minecraft.wiki/w/Interaction_range gives 5 blocks for Bedrock.
const BLOCK_REACH: f64 = 4.5;
const CREATIVE_BLOCK_REACH: f64 = 6.0;
/// Reach counts to a block's nearest point: about half its diagonal off its centre.
const BLOCK_RADIUS: f64 = 0.87;
/// How far, in degrees, a look may read back from the angles toward a point, and toward an
/// entity (SimulatedPlayer.lookAtEntity may aim at another point than the eyes the driver
/// aims at).
const LOOK_TOLERANCE: f64 = 3.0;
const ENTITY_LOOK_TOLERANCE: f64 = 10.0;
/// How long a look must read back right to count as held.
const LOOK_HOLD: Duration = Duration::from_millis(150);
/// How far ahead of the eyes a look toward a yaw and pitch aims.
const LOOK_REACH: f64 = 16.0;
/// Ticks an entity ignores further hits after it was hurt.
const HURT_COOLDOWN: u64 = 10;
/// How long the end of a timed hold gets to show up after the stop.
const END_GRACE: Duration = Duration::from_millis(250);

const USING: [&str; 4] = [
    "item_use",
    "item_start_use",
    "item_complete_use",
    "item_release_use",
];
const ENDS: [&str; 3] = ["item_complete_use", "item_release_use", "item_stop_use"];

pub struct GameTestDriver {
    pub name: String,
    pub confirm: Duration,
    /// Hand back actions that still run as [`Outcome::pending`] instead of waiting.
    pub defer: bool,
    /// The game tick to act on and about how long until it: the first method call is sent
    /// ahead and the pack runs it on that tick. `None` acts on the next tick.
    pub at: Option<(u64, Duration)>,
}

/// The simulated player an action drives, owned so a running action can keep it.
#[derive(Clone)]
struct Sim {
    name: String,
    world: World,
}

impl Sim {
    fn outcome(&self) -> Outcome {
        Outcome::new(&Actor::Named(self.name.clone()), "gametest")
    }

    fn call(&self, action: &Action) -> Result<Value> {
        gametest::act(&self.world, &self.name, action)
    }

    /// The simulated player's state, as `gametest_list` reports it.
    fn player(&self) -> Result<Value> {
        gametest::list(&self.world)?
            .into_iter()
            .find(|player| player["name"] == self.name.as_str())
            .ok_or_else(|| {
                Error::msg(format!(
                    "No simulated player named {:?} is in the world; spawn one with gametest_spawn.",
                    self.name
                ))
            })
    }

    /// The state of a simulated player that can act: fails when it is dead.
    fn living(&self) -> Result<Value> {
        let player = self.player()?;
        if player["alive"] == false {
            bail!(
                "{}",
                friendly(
                    "gametest",
                    &format!("Simulated player \"{}\" is dead", self.name)
                )
            );
        }
        Ok(player)
    }
}

fn location_of(player: &Value) -> Result<[f64; 3]> {
    point_of(&player["location"]).ok_or_else(|| {
        Error::msg(format!(
            "Simulated player {} has no location.",
            player["name"]
        ))
    })
}

fn eye_of(player: &Value) -> Result<[f64; 3]> {
    point_of(&player["eye"])
        .or_else(|| point_of(&player["location"]).map(|[x, y, z]| [x, y + EYE_HEIGHT, z]))
        .ok_or_else(|| {
            Error::msg(format!(
                "Simulated player {} has no location.",
                player["name"]
            ))
        })
}

fn creative(player: &Value) -> bool {
    player["gameMode"]
        .as_str()
        .is_some_and(|mode| mode.eq_ignore_ascii_case("creative"))
}

fn mode_name(player: &Value) -> &'static str {
    if creative(player) {
        "creative"
    } else {
        "survival"
    }
}

fn selected_slot(player: &Value) -> Result<u8> {
    player["selectedSlot"]
        .as_u64()
        .and_then(|slot| u8::try_from(slot).ok())
        .ok_or_else(|| {
            Error::msg(format!(
                "Simulated player {} has no selected slot.",
                player["name"]
            ))
        })
}

/// An entity's eyes: from the bridge, or its querytarget position without it.
fn entity_eye(world: &World, bridge: bool, id: &str) -> Result<[f64; 3]> {
    if bridge {
        let entity = world.bridge_command(&format!("re_mcp:entity {}", quoted(id)?))?;
        return point_of(&entity["eye"])
            .or_else(|| point_of(&entity["location"]))
            .ok_or_else(|| Error::msg(format!("Entity {id} has no location.")));
    }
    entity_point(world, false, id)
}

/// A note when the block at `at` is beyond the reach of `player`, as an action's reply shows
/// the player.
fn block_reach_note(player: &Value, at: [i64; 3]) -> Option<String> {
    let eye = eye_of(player).ok()?;
    let gap = (distance(eye, face_point(at, None)) - BLOCK_RADIUS).max(0.0);
    let reach = if creative(player) {
        CREATIVE_BLOCK_REACH
    } else {
        BLOCK_REACH
    };
    let [x, y, z] = at;
    (gap > reach).then(|| {
        format!(
            "The block at {x} {y} {z} may be out of reach: {gap:.1} blocks from the simulated \
             player's eyes, beyond the {reach} blocks the server allows in {}, which may be why \
             nothing happened. movement_set with to brings the player closer.",
            mode_name(player)
        )
    })
}

/// Why an attack the game refused did not land, from what the pack read of the target.
fn refusal(target: &Value) -> String {
    if target["valid"] == false
        || target["health"]
            .as_f64()
            .is_some_and(|health| health <= 0.0)
    {
        return "The target was already dead or gone.".to_owned();
    }
    if let Some(ago) = target["hurtTicksAgo"].as_u64()
        && ago < HURT_COOLDOWN
    {
        return format!(
            "The target was hurt {ago} ticks before, and entities ignore hits for about \
             {HURT_COOLDOWN} ticks after one; attack again a few ticks later."
        );
    }
    "The target was within reach and the game gave no reason.".to_owned()
}

/// The difference between two yaws, in degrees, the short way round.
fn angle_gap(a: f64, b: f64) -> f64 {
    ((a - b + 180.0).rem_euclid(360.0) - 180.0).abs()
}

/// How the target's height compares with where the player stands, for notes.
fn height(rise: f64) -> String {
    if rise.abs() <= VERTICAL_SLACK / 2.0 {
        String::new()
    } else if rise > 0.0 {
        format!(" and {} blocks below it", rounded(rise))
    } else {
        format!(" and {} blocks above it", rounded(-rise))
    }
}

/// Adjusts an outcome once its action finished.
type Then = Box<dyn FnOnce(&mut Outcome) -> Result<()>>;

fn nothing() -> Then {
    Box::new(|_| Ok(()))
}

/// The notes on an attack or interaction with entity `id` the game refused: out of reach
/// (the pack measured it when the action ran), or why an attack did not land.
fn entity_notes(id: Option<String>, verb: &'static str, attack: bool) -> Then {
    Box::new(move |outcome| {
        let Some(id) = id else { return Ok(()) };
        let evidence = &outcome.evidence;
        if evidence["refused"] == "reach" {
            let note = format!(
                "Entity {id} is out of reach: {} blocks from the simulated player's eyes, beyond \
                 the {} block reach of a player in {}, so it was not {verb}. movement_set with \
                 to: the entity id closes the gap.",
                evidence["distance"].as_f64().unwrap_or_default(),
                evidence["reach"].as_f64().unwrap_or_default(),
                mode_name(&evidence["player"])
            );
            outcome.notes.push(note);
        } else if attack && evidence["result"] == false {
            let reason = refusal(&evidence["target"]);
            outcome.notes.push(reason);
        }
        Ok(())
    })
}

/// Simulated players walk with GameTest navigation, so they do not take `approach`.
fn refuse_approach(approach: Option<Duration>) -> Result<()> {
    if approach.is_some() {
        bail!(
            "approach walks the connected player (actor self); move a simulated player within \
             reach with movement_set to: the entity id first."
        );
    }
    Ok(())
}

/// How a started action goes on once the pack ran its first method call.
enum Next {
    Done(Outcome),
    Running(Box<dyn Pending>),
}

/// What an action does with the reply to its first method call.
type Continue = Box<dyn FnOnce(&mut dyn Game, Value) -> Result<Next>>;

/// The bridge events that confirm an action.
struct Watching {
    names: &'static [&'static str],
    matches: Box<dyn Fn(&HubEvent) -> bool>,
}

fn any(names: &'static [&'static str]) -> Watching {
    Watching {
        names,
        matches: Box::new(|_| true),
    }
}

/// A confirming event the driver is waiting for.
struct Watch {
    names: &'static [&'static str],
    player: String,
    matches: Box<dyn Fn(&HubEvent) -> bool>,
    cursor: EventCursor,
    /// Events from before this game tick (when the action ran) do not count: the cursor can
    /// be taken well before an action sent ahead runs.
    since: Option<u64>,
    started: Instant,
    timeout: Duration,
}

impl Watch {
    /// Watch from `cursor` for the action whose reply is `reply`, for up to `timeout` from now.
    fn new(
        watching: Watching,
        player: &str,
        cursor: EventCursor,
        reply: &Value,
        timeout: Duration,
    ) -> Self {
        Self {
            names: watching.names,
            player: player.to_owned(),
            matches: watching.matches,
            cursor,
            since: reply["tick"].as_u64(),
            started: Instant::now(),
            timeout,
        }
    }

    /// The confirmation once the event came or the time ran out; `None` while it may still
    /// come.
    fn check(&self, game: &mut dyn Game) -> Result<Option<Confirmation>> {
        let since = self.since;
        let matches = |event: &HubEvent| {
            since.is_none_or(|since| event.tick.is_none_or(|tick| tick >= since))
                && (self.matches)(event)
        };
        let expect = Expect {
            bridge: self.names,
            ws: &[],
            player: Some(&self.player),
            matches: &matches,
        };
        let last = self.started.elapsed() >= self.timeout;
        let found = confirm_after(
            game,
            true,
            &self.cursor,
            Duration::ZERO,
            self.timeout,
            &expect,
            &first,
        )?;
        Ok((last || !found.events.is_empty()).then_some(found))
    }
}

impl GameTestDriver {
    fn sim(&self, game: &mut dyn Game) -> Result<Sim> {
        Ok(Sim {
            name: self.name.clone(),
            world: game.world()?,
        })
    }

    /// Finish a started action: wait for it, or with `defer` hand it back to be polled.
    fn settle(&self, game: &mut dyn Game, mut pending: Box<dyn Pending>) -> Result<Outcome> {
        if self.defer {
            let mut outcome = Outcome::new(&Actor::Named(self.name.clone()), "gametest");
            outcome.pending = Some(pending);
            return Ok(outcome);
        }
        loop {
            if let Some(outcome) = pending.poll(game)? {
                return Ok(outcome);
            }
            thread::sleep(POLL);
        }
    }

    /// Finish an action that already has its reply.
    fn proceed(&self, game: &mut dyn Game, next: Next) -> Result<Outcome> {
        match next {
            Next::Done(outcome) => Ok(outcome),
            Next::Running(pending) => self.settle(game, pending),
        }
    }

    /// Send the action's first method call (ahead, for the scheduled tick, when there is one)
    /// and go on with `next` once the pack ran it. The outcome's evidence.acted_tick is the
    /// game tick the call ran on.
    fn first(
        &self,
        game: &mut dyn Game,
        sim: &Sim,
        action: &Action,
        next: Continue,
    ) -> Result<Outcome> {
        let ticket = gametest::send(&sim.world, &sim.name, action, self.at)?;
        let world = sim.world.clone();
        let mut waiting = Some((ticket, next));
        let mut running: Option<Box<dyn Pending>> = None;
        let mut acted = Value::Null;
        self.settle(
            game,
            Box::new(move |game: &mut dyn Game| {
                let finished = match running.as_mut() {
                    Some(pending) => pending.poll(game)?,
                    None => {
                        let (ticket, _) =
                            waiting.as_ref().expect("a finished action is not polled");
                        let Some(reply) = gametest::collect(&world, ticket)? else {
                            return Ok(None);
                        };
                        acted = reply["tick"].clone();
                        let (_, next) = waiting.take().expect("a finished action is not polled");
                        match next(game, reply)? {
                            Next::Done(outcome) => Some(outcome),
                            Next::Running(mut pending) => {
                                let finished = pending.poll(game)?;
                                if finished.is_none() {
                                    running = Some(pending);
                                }
                                finished
                            }
                        }
                    }
                };
                Ok(finished.map(|mut outcome| {
                    if !acted.is_null() {
                        outcome.evidence["acted_tick"] = acted.clone();
                    }
                    outcome
                }))
            }),
        )
    }

    /// Call one method. A `false` result leaves the outcome unconfirmed; otherwise, with the
    /// bridge, an event `watching` accepts must follow. `then` runs on the finished outcome.
    fn perform(
        &self,
        game: &mut dyn Game,
        sim: &Sim,
        action: &Action,
        watching: Option<Watching>,
        then: Then,
    ) -> Result<Outcome> {
        let bridge = watching.is_some() && sim.world.bridge(true)?;
        let cursor = if bridge { Some(game.cursor()?) } else { None };
        let mut outcome = sim.outcome();
        let name = sim.name.clone();
        let confirm = self.confirm;
        let next: Continue = Box::new(move |_, reply| {
            outcome.evidence = json!({
                "method": reply["action"], "result": reply["result"], "player": reply["player"],
            });
            for key in ["target", "distance", "reach", "refused"] {
                if !reply[key].is_null() {
                    outcome.evidence[key] = reply[key].clone();
                }
            }
            if reply["result"] == false {
                // A reach refusal gets its note from `then`.
                let method = reply["action"].as_str().unwrap_or("method");
                if method == "attack_entity" && reply["refused"].is_null() {
                    outcome.notes.push(
                        "SimulatedPlayer attack_entity returned false: the game refused the \
                         attack."
                            .to_owned(),
                    );
                } else if reply["refused"].is_null() {
                    outcome.notes.push(format!(
                        "SimulatedPlayer {method} returned false: the game refused it (nothing \
                         to use, the wrong item, or out of reach)."
                    ));
                }
                then(&mut outcome)?;
                return Ok(Next::Done(outcome));
            }
            outcome.confirmed = true;
            let watch = match (watching, cursor) {
                (Some(watching), Some(cursor)) => {
                    Watch::new(watching, &name, cursor, &reply, confirm)
                }
                (watching, _) => {
                    if watching.is_some() {
                        outcome.notes.push(
                            "Without the Bridge pack only the method result confirms this action."
                                .to_owned(),
                        );
                    }
                    then(&mut outcome)?;
                    return Ok(Next::Done(outcome));
                }
            };
            let mut done = Some((outcome, then));
            Ok(Next::Running(Box::new(move |game: &mut dyn Game| {
                let Some(found) = watch.check(game)? else {
                    return Ok(None);
                };
                let (mut outcome, then) = done.take().expect("a finished action is not polled");
                outcome.confirm_with(found);
                then(&mut outcome)?;
                Ok(Some(outcome))
            })))
        });
        self.first(game, sim, action, next)
    }

    fn travel(
        &self,
        game: &mut dyn Game,
        movement: &Movement,
        to: &Destination,
    ) -> Result<Outcome> {
        let sim = self.sim(game)?;
        let bridge = sim.world.bridge(true)?;
        let mut outcome = sim.outcome();
        outcome.source = "gametest";
        if movement.sprint.is_some() || movement.jump.is_some() {
            outcome.notes.push(
                "GameTest navigation sets its own speed and jumps by itself, so sprint and jump \
                 were ignored."
                    .to_owned(),
            );
        }
        let action = match to {
            Destination::Position(at) => Action::NavigateToLocation {
                location: *at,
                speed: None,
            },
            Destination::Entity(id) => Action::NavigateToEntity {
                entity: id.clone(),
                speed: None,
            },
        };
        let radius = movement.arrival_radius.unwrap_or(DEFAULT_ARRIVAL);
        let timeout = Duration::from_millis(movement.timeout_ms.unwrap_or(DEFAULT_TRAVEL_MS));
        let to = to.clone();
        let walker = sim.clone();
        let next: Continue = Box::new(move |_, mut reply| {
            let sim = walker;
            let from = location_of(&reply["player"])?;
            let path = reply["result"].take();
            if path["isFullPath"] == false {
                outcome
                    .notes
                    .push("Navigation found only a partial path to the target.".to_owned());
            }
            let no_path = path["path"].as_array().is_some_and(Vec::is_empty);
            let started = Instant::now();
            let deadline = started + timeout;
            let mut anchor = (from, started);
            let mut done = Some(outcome);
            Ok(Next::Running(Box::new(move |_: &mut dyn Game| {
                let player = sim.player()?;
                let at = location_of(&player)?;
                let target = match &to {
                    Destination::Position(at) => [at.x, at.y, at.z],
                    Destination::Entity(id) => entity_point(&sim.world, bridge, id)?,
                };
                let gap = horizontal(at, target);
                let rise = target[1] - at[1];
                let arrived = gap <= radius && rise.abs() <= radius + VERTICAL_SLACK;
                let now = Instant::now();
                if distance(at, anchor.0) > STIRRED {
                    anchor = (at, now);
                }
                let alive = player["alive"] != false;
                let failure = if arrived {
                    None
                } else if !alive {
                    Some("The simulated player died on the way.".to_owned())
                } else if no_path {
                    Some(
                        "Navigation found no path to the target, so the simulated player did not \
                         set off."
                            .to_owned(),
                    )
                } else if now - anchor.1 >= STUCK {
                    Some(format!(
                        "The simulated player has not moved for {} s; something blocks the way \
                         or the path ends here.",
                        STUCK.as_secs()
                    ))
                } else if now >= deadline {
                    Some("The time ran out.".to_owned())
                } else {
                    return Ok(None);
                };
                if failure.is_some() && alive {
                    sim.call(&Action::StopMoving)?;
                }
                let mut outcome = done.take().expect("a finished action is not polled");
                outcome.confirmed = arrived;
                outcome.evidence = json!({
                    "from": from, "at": at, "target": target, "distance": rounded(gap),
                    "vertical_gap": rounded(rise), "path": path,
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                });
                if let Some(failure) = failure {
                    outcome.notes.push(format!(
                        "{failure} It stopped {} blocks from the target horizontally{}.",
                        rounded(gap),
                        height(rise)
                    ));
                }
                Ok(Some(outcome))
            })))
        });
        self.first(game, &sim, &action, next)
    }

    fn walk(&self, game: &mut dyn Game, movement: &Movement) -> Result<Outcome> {
        if movement.sneak == Some(true) {
            bail!(
                "Simulated players cannot sneak through the GameTest pack; leave sneak out, or use \
                 the self actor."
            );
        }
        let sim = self.sim(game)?;
        let (forward, strafe) = (
            movement.forward.unwrap_or(0.0),
            movement.strafe.unwrap_or(0.0),
        );
        let walking = forward != 0.0 || strafe != 0.0;
        let jump = movement.jump == Some(true);
        // moveRelative takes left/right with left positive; strafe is positive to the right.
        let action = if walking {
            Some(Action::MoveRelative {
                left_right: -strafe,
                backward_forward: forward,
                speed: movement.sprint.filter(|sprint| *sprint).map(|_| 1.0),
            })
        } else if movement.forward.is_some() || movement.strafe.is_some() {
            Some(Action::StopMoving)
        } else if jump {
            Some(Action::Jump)
        } else {
            None
        };
        let jump_after = jump
            && action
                .as_ref()
                .is_some_and(|action| *action != Action::Jump);
        let timed = movement.duration_ms.map(Duration::from_millis);
        let moves = movement.moves();
        let walker = sim.clone();
        let next: Continue = Box::new(move |_, reply| {
            let sim = walker;
            let from = location_of(&reply["player"])?;
            if jump_after {
                sim.call(&Action::Jump)?;
            }
            let until = Instant::now() + timed.unwrap_or(PROBE);
            let mut done = Some(sim.outcome());
            Ok(Next::Running(Box::new(move |_: &mut dyn Game| {
                if Instant::now() < until {
                    return Ok(None);
                }
                if walking && timed.is_some() {
                    sim.call(&Action::StopMoving)?;
                }
                let player = sim.player()?;
                let to = location_of(&player)?;
                let moved = distance(from, to);
                let mut outcome = done.take().expect("a finished action is not polled");
                outcome.source = "gametest";
                outcome.evidence = json!({ "from": from, "to": to, "moved": rounded(moved) });
                outcome.confirmed = !moves || moved > MOVED;
                if !outcome.confirmed {
                    outcome.notes.push(if player["alive"] == false {
                        "The simulated player died and did not move.".to_owned()
                    } else {
                        "The simulated player did not move; something may block the way.".to_owned()
                    });
                }
                if walking && timed.is_none() {
                    outcome.notes.push(
                        "The simulated player keeps walking until movement_set with zeros or the \
                         stop action."
                            .to_owned(),
                    );
                }
                Ok(Some(outcome))
            })))
        });
        match action {
            Some(action) => self.first(game, &sim, &action, next),
            None => {
                let player = sim.living()?;
                let next = next(game, json!({ "player": player }))?;
                self.proceed(game, next)
            }
        }
    }

    /// Use the item in `slot` for `hold`, then stop; confirmed by the use starting, with the
    /// event that ended it (completed, released or stopped) as evidence.completed.
    fn hold(&self, game: &mut dyn Game, sim: Sim, slot: u8, hold: Duration) -> Result<Outcome> {
        let bridge = sim.world.bridge(true)?;
        let cursor = if bridge { Some(game.cursor()?) } else { None };
        let confirm = self.confirm;
        let user = sim.clone();
        let next: Continue = Box::new(move |_, reply| {
            let sim = user;
            let mut outcome = sim.outcome();
            outcome.evidence = json!({
                "method": reply["action"], "result": reply["result"], "player": reply["player"],
            });
            if reply["result"] == false {
                outcome.notes.push(
                    "SimulatedPlayer use_item_in_slot returned false: the game refused it \
                     (nothing to use or the wrong item)."
                        .to_owned(),
                );
                return Ok(Next::Done(outcome));
            }
            outcome.confirmed = true;
            let started = Instant::now();
            let watches = cursor.map(|cursor| {
                let start = Watch::new(
                    any(&USING),
                    &sim.name,
                    cursor.clone(),
                    &reply,
                    confirm.max(hold),
                );
                let end = Watch::new(any(&ENDS), &sim.name, cursor, &reply, hold + END_GRACE);
                (start, end)
            });
            if watches.is_none() {
                outcome.notes.push(
                    "Without the Bridge pack only the method result confirms this action."
                        .to_owned(),
                );
            }
            let mut stopped: Option<Value> = None;
            let mut began: Option<Confirmation> = None;
            let mut done = Some(outcome);
            Ok(Next::Running(Box::new(move |game: &mut dyn Game| {
                if started.elapsed() < hold {
                    return Ok(None);
                }
                if stopped.is_none() {
                    stopped = Some(sim.call(&Action::StopUsingItem)?);
                }
                let mut ended = None;
                if let Some((start, end)) = &watches {
                    if began.is_none() {
                        began = start.check(game)?;
                    }
                    ended = end.check(game)?;
                    if began.is_none() || ended.is_none() {
                        return Ok(None);
                    }
                }
                let mut outcome = done.take().expect("a finished action is not polled");
                let stopped = stopped.take().unwrap_or_default();
                outcome.evidence["stopped"] = stopped["result"].clone();
                outcome.evidence["player"] = stopped["player"].clone();
                if let Some(began) = began.take() {
                    outcome.confirm_with(began);
                }
                if let Some(ended) = ended {
                    outcome.evidence["completed"] =
                        ended.events.first().map_or(Value::Null, HubEvent::to_json);
                    if ended.events.is_empty() {
                        outcome.notes.extend(ended.notes);
                    }
                }
                Ok(Some(outcome))
            })))
        });
        self.first(game, &sim, &Action::UseItemInSlot { slot }, next)
    }

    /// Attack or use an entity by id, within reach, or whatever the view ray hits.
    fn at_entity(
        &self,
        game: &mut dyn Game,
        entity: Option<&str>,
        attack: bool,
    ) -> Result<Outcome> {
        let sim = self.sim(game)?;
        let id = entity.map(str::to_owned);
        let target = id.clone();
        let names: &'static [&'static str] = match (attack, entity.is_some()) {
            (true, true) => &["entity_hit_entity"],
            (true, false) => &["entity_hit_entity", "entity_hit_block"],
            (false, true) => &["player_interact_entity"],
            (false, false) => &["player_interact_entity", "player_interact_block"],
        };
        let watching = Watching {
            names,
            matches: Box::new(move |event| {
                target
                    .as_deref()
                    .is_none_or(|id| event.body["target"]["id"].as_str() == Some(id))
            }),
        };
        let reach = Some(REACH);
        let action = match (entity, attack) {
            (Some(id), true) => Action::AttackEntity {
                entity: id.to_owned(),
                reach,
            },
            (None, true) => Action::Attack,
            (Some(id), false) => Action::InteractWithEntity {
                entity: id.to_owned(),
                reach,
            },
            (None, false) => Action::Interact,
        };
        let verb = if attack { "attacked" } else { "used" };
        self.perform(
            game,
            &sim,
            &action,
            Some(watching),
            entity_notes(id, verb, attack),
        )
    }
}

/// Turn simulated player `name` to `yaw` and `pitch` with a continuous look at a point that
/// far along the direction, which the game keeps, and confirm by readback. `None` when `name`
/// is not a simulated player (or the GameTest pack is missing).
pub fn look_toward(
    game: &mut dyn Game,
    name: &str,
    confirm: Duration,
    yaw: f64,
    pitch: f64,
) -> Result<Option<Outcome>> {
    let world = game.world()?;
    let Some(player) = gametest::list(&world)
        .ok()
        .and_then(|players| players.into_iter().find(|player| player["name"] == name))
    else {
        return Ok(None);
    };
    let eye = eye_of(&player)?;
    let ahead = direction(yaw, pitch);
    let point = [0, 1, 2].map(|axis| eye[axis] + ahead[axis] * LOOK_REACH);
    let driver = GameTestDriver {
        name: name.to_owned(),
        confirm,
        defer: false,
        at: None,
    };
    super::Driver::look(&driver, game, &Aim::Position(point)).map(Some)
}

impl super::Driver for GameTestDriver {
    /// Turn with a continuous look, so the game keeps the pitch, and confirm by reading the
    /// rotation back until it holds.
    fn look(&self, game: &mut dyn Game, aim: &Aim) -> Result<Outcome> {
        let sim = self.sim(game)?;
        let bridge = sim.world.bridge(true)?;
        let duration = Some(LookDuration::Continuous);
        let (action, point, entity) = match aim {
            Aim::Position(point) => (
                Action::LookAtLocation {
                    location: vec3(*point),
                    duration,
                },
                *point,
                None,
            ),
            Aim::Block(at, face) => {
                let point = face_point(*at, *face);
                (
                    Action::LookAtLocation {
                        location: vec3(point),
                        duration,
                    },
                    point,
                    None,
                )
            }
            Aim::Entity(id) => (
                Action::LookAtEntity {
                    entity: id.clone(),
                    duration,
                },
                [0.0; 3],
                Some(id.clone()),
            ),
        };
        let tolerance = if entity.is_some() {
            ENTITY_LOOK_TOLERANCE
        } else {
            LOOK_TOLERANCE
        };
        // Without the bridge an entity's eyes are unknown, so only its yaw is checked.
        let check_pitch = entity.is_none() || bridge;
        let timeout = self.confirm;
        let looker = sim.clone();
        let next: Continue = Box::new(move |_, reply| {
            let sim = looker;
            let mut outcome = sim.outcome();
            outcome.source = "gametest";
            outcome.evidence = json!({ "method": reply["action"] });
            let started = Instant::now();
            let mut held: Option<Instant> = None;
            let mut done = Some(outcome);
            Ok(Next::Running(Box::new(move |_: &mut dyn Game| {
                let player = sim.player()?;
                let target = match &entity {
                    Some(id) => entity_eye(&sim.world, bridge, id)?,
                    None => point,
                };
                let (yaw, pitch) = facing(eye_of(&player)?, target);
                let pitch = pitch.clamp(-90.0, 90.0);
                let read = (player["yaw"].as_f64(), player["pitch"].as_f64());
                let on = match read {
                    (Some(read_yaw), Some(read_pitch)) => {
                        angle_gap(read_yaw, yaw) <= tolerance
                            && (!check_pitch || (read_pitch - pitch).abs() <= tolerance)
                    }
                    _ => false,
                };
                let now = Instant::now();
                let holds = if on {
                    now - *held.get_or_insert(now) >= LOOK_HOLD
                } else {
                    held = None;
                    false
                };
                if !holds && started.elapsed() < timeout {
                    return Ok(None);
                }
                let mut outcome = done.take().expect("a finished action is not polled");
                outcome.confirmed = holds;
                outcome.evidence["requested"] =
                    json!({ "yaw": rounded(yaw), "pitch": rounded(pitch) });
                outcome.evidence["yaw"] = player["yaw"].clone();
                outcome.evidence["pitch"] = player["pitch"].clone();
                outcome.evidence["target"] = json!(target);
                if !holds {
                    outcome.notes.push(format!(
                        "After {} ms the simulated player reads yaw {} pitch {}, not within \
                         {tolerance} degrees of yaw {} pitch {}.",
                        timeout.as_millis(),
                        player["yaw"],
                        player["pitch"],
                        rounded(yaw),
                        rounded(pitch)
                    ));
                }
                if !check_pitch {
                    outcome.notes.push(
                        "Without the Bridge pack the entity's eyes are unknown, so only the yaw \
                         was checked."
                            .to_owned(),
                    );
                }
                Ok(Some(outcome))
            })))
        });
        self.first(game, &sim, &action, next)
    }

    fn move_to(&self, game: &mut dyn Game, movement: &Movement) -> Result<Outcome> {
        match &movement.to {
            Some(to) => self.travel(game, movement, to),
            None => self.walk(game, movement),
        }
    }

    fn select_slot(&self, game: &mut dyn Game, slot: u8) -> Result<Outcome> {
        let sim = self.sim(game)?;
        self.perform(
            game,
            &sim,
            &Action::SelectSlot { slot },
            None,
            Box::new(move |outcome| {
                outcome.confirmed = outcome.evidence["player"]["selectedSlot"] == slot;
                if !outcome.confirmed {
                    let note = format!(
                        "The simulated player reports slot {} selected, not {slot}.",
                        outcome.evidence["player"]["selectedSlot"]
                    );
                    outcome.notes.push(note);
                }
                Ok(())
            }),
        )
    }

    fn use_item(
        &self,
        game: &mut dyn Game,
        mode: UseMode,
        hold_ms: Option<u64>,
        hand: Hand,
    ) -> Result<Outcome> {
        if hand == Hand::Offhand {
            bail!(
                "Simulated players cannot use the offhand item: SimulatedPlayer only uses hotbar \
                 slots. Use hand mainhand, or the self actor."
            );
        }
        let sim = self.sim(game)?;
        if mode == UseMode::Release {
            return self.perform(
                game,
                &sim,
                &Action::StopUsingItem,
                Some(any(&ENDS)),
                nothing(),
            );
        }
        let slot = selected_slot(&sim.living()?)?;
        match (mode, hold_ms) {
            (UseMode::Hold, Some(ms)) => self.hold(game, sim, slot, Duration::from_millis(ms)),
            (UseMode::Hold, None) => self.perform(
                game,
                &sim,
                &Action::UseItemInSlot { slot },
                Some(any(&USING)),
                Box::new(|outcome| {
                    outcome.notes.push(
                        "The item stays in use until item_use with mode release (or the stop \
                         action)."
                            .to_owned(),
                    );
                    Ok(())
                }),
            ),
            _ => self.perform(
                game,
                &sim,
                &Action::UseItemInSlot { slot },
                Some(any(&USING)),
                nothing(),
            ),
        }
    }

    fn use_on(&self, game: &mut dyn Game, at: [i64; 3], face: Option<Face>) -> Result<Outcome> {
        let sim = self.sim(game)?;
        let slot = selected_slot(&sim.living()?)?;
        let placed = face.map(|face| beside(at, face));
        let watching = Watching {
            names: &[
                "player_interact_block",
                "item_start_use_on",
                "block_place",
                "item_use",
            ],
            matches: Box::new(move |event| match event.name.as_str() {
                "item_use" => true,
                "block_place" => {
                    placed.is_some() && block_of(&event.body["block"]["location"]) == placed
                }
                _ => block_of(&event.body["block"]["location"]) == Some(at),
            }),
        };
        let action = Action::UseItemInSlotOnBlock {
            slot,
            block: block_vec3(at),
            face,
            face_location: None,
        };
        self.perform(
            game,
            &sim,
            &action,
            Some(watching),
            Box::new(move |outcome| {
                if !outcome.confirmed {
                    let note = block_reach_note(&outcome.evidence["player"], at);
                    outcome.notes.extend(note);
                }
                Ok(())
            }),
        )
    }

    fn attack(
        &self,
        game: &mut dyn Game,
        entity: Option<&str>,
        approach: Option<Duration>,
    ) -> Result<Outcome> {
        refuse_approach(approach)?;
        self.at_entity(game, entity, true)
    }

    fn interact(
        &self,
        game: &mut dyn Game,
        entity: Option<&str>,
        approach: Option<Duration>,
    ) -> Result<Outcome> {
        refuse_approach(approach)?;
        self.at_entity(game, entity, false)
    }

    fn break_block(
        &self,
        game: &mut dyn Game,
        at: Option<[i64; 3]>,
        timeout: Duration,
    ) -> Result<Outcome> {
        let Some(at) = at else {
            bail!("Simulated players break a given block; pass location.");
        };
        let sim = self.sim(game)?;
        let bridge = sim.world.bridge(true)?;
        let cursor = if bridge { Some(game.cursor()?) } else { None };
        let miner = sim.clone();
        let next: Continue = Box::new(move |_, reply| {
            let sim = miner;
            let [x, y, z] = at;
            let mut outcome = sim.outcome();
            outcome.evidence = json!({
                "method": reply["action"], "result": reply["result"], "player": reply["player"],
                "location": at,
            });
            let mut reach_note = block_reach_note(&reply["player"], at);
            if reply["result"] == false {
                outcome.notes.push(format!(
                    "SimulatedPlayer break_block returned false: the block at {x} {y} {z} is not \
                     solid."
                ));
                outcome.notes.extend(reach_note);
                return Ok(Next::Done(outcome));
            }
            let watching = Watching {
                names: &["block_break"],
                matches: Box::new(move |event| block_of(&event.body["location"]) == Some(at)),
            };
            let watch =
                cursor.map(|cursor| Watch::new(watching, &sim.name, cursor, &reply, timeout));
            let air = format!("testforblock {x} {y} {z} air");
            let started = Instant::now();
            let mut done = Some(outcome);
            Ok(Next::Running(Box::new(move |game: &mut dyn Game| {
                let mut outcome = match &watch {
                    Some(watch) => {
                        let Some(found) = watch.check(game)? else {
                            return Ok(None);
                        };
                        let mut outcome = done.take().expect("a finished action is not polled");
                        outcome.confirm_with(found);
                        outcome
                    }
                    None => {
                        let broken = sim.world.test(&air)?;
                        if !broken && started.elapsed() < timeout {
                            return Ok(None);
                        }
                        let mut outcome = done.take().expect("a finished action is not polled");
                        outcome.confirmed = broken;
                        outcome.source = "command";
                        outcome.evidence["air"] = broken.into();
                        outcome
                    }
                };
                outcome.evidence["elapsed_ms"] = (started.elapsed().as_millis() as u64).into();
                if !outcome.confirmed {
                    sim.call(&Action::StopBreakingBlock)?;
                    let block = if bridge {
                        sim.world
                            .bridge_command(&format!("re_mcp:block {x} {y} {z}"))?
                    } else {
                        json!({ "air": false })
                    };
                    outcome.notes.push(format!(
                        "The block at {x} {y} {z} is still {} after {} ms; it may need a better \
                         tool or more time (timeout_ms).",
                        block["id"].as_str().unwrap_or("there"),
                        timeout.as_millis()
                    ));
                    outcome.evidence["block"] = block;
                    outcome.notes.extend(reach_note.take());
                }
                Ok(Some(outcome))
            })))
        });
        self.first(
            game,
            &sim,
            &Action::BreakBlock {
                block: block_vec3(at),
                face: None,
            },
            next,
        )
    }

    fn place(
        &self,
        game: &mut dyn Game,
        at: [i64; 3],
        face: Face,
        slot: Option<u8>,
    ) -> Result<Outcome> {
        let sim = self.sim(game)?;
        let slot = match slot {
            Some(slot) => slot,
            None => selected_slot(&sim.living()?)?,
        };
        let placed = beside(at, face);
        let watching = Watching {
            names: &["block_place"],
            matches: Box::new(move |event| {
                block_of(&event.body["block"]["location"]) == Some(placed)
            }),
        };
        let action = Action::UseItemInSlotOnBlock {
            slot,
            block: block_vec3(at),
            face: Some(face),
            face_location: None,
        };
        let world = sim.world.clone();
        self.perform(
            game,
            &sim,
            &action,
            Some(watching),
            Box::new(move |outcome| {
                outcome.evidence["placed_at"] = json!(placed);
                if outcome.confirmed && outcome.source == "gametest" {
                    let [x, y, z] = placed;
                    let filled = !world.test(&format!("testforblock {x} {y} {z} air"))?;
                    outcome.confirmed = filled;
                    outcome.source = "command";
                    if !filled {
                        outcome
                            .notes
                            .push(format!("testforblock finds air at {x} {y} {z}."));
                    }
                }
                if !outcome.confirmed {
                    let note = block_reach_note(&outcome.evidence["player"], at);
                    outcome.notes.extend(note);
                }
                Ok(())
            }),
        )
    }

    /// Drop `quantity` from the selected stack in one call: the pack splits the stack so
    /// dropSelectedItem drops only that many, and the slot read in the same tick shows it.
    fn drop_item(&self, game: &mut dyn Game, quantity: Quantity) -> Result<Outcome> {
        let sim = self.sim(game)?;
        let mut outcome = sim.outcome();
        let held = |player: &Value| player["selectedItem"]["amount"].as_u64().unwrap_or(0);
        let before = held(&sim.living()?);
        if before == 0 {
            outcome
                .notes
                .push("The selected slot is empty, so nothing was dropped.".to_owned());
            return Ok(outcome);
        }
        let wanted = match quantity {
            Quantity::Whole(_) => before,
            Quantity::Count(count) => u64::from(count),
        };
        let amount = u8::try_from(wanted.min(before)).unwrap_or(u8::MAX);
        let next: Continue = Box::new(move |_, reply| {
            let player = reply["player"].clone();
            let dropped = before.saturating_sub(held(&player));
            outcome.source = "gametest";
            outcome.confirmed = dropped == wanted;
            outcome.evidence = json!({ "dropped": dropped, "player": player });
            if reply["result"] == false {
                outcome
                    .notes
                    .push("SimulatedPlayer dropSelectedItem returned false.".to_owned());
            }
            if dropped > wanted {
                outcome.notes.push(format!(
                    "{dropped} items dropped, more than the {wanted} asked for."
                ));
            } else if dropped < wanted {
                outcome.notes.push(format!(
                    "Only {dropped} of {wanted} items dropped; the slot held {before}."
                ));
            }
            Ok(Next::Done(outcome))
        });
        self.first(game, &sim, &Action::DropAmount { amount }, next)
    }

    fn stop(&self, game: &mut dyn Game) -> Result<Outcome> {
        let sim = self.sim(game)?;
        let stopper = sim.clone();
        let next: Continue = Box::new(move |_, _| {
            let sim = stopper;
            sim.call(&Action::StopUsingItem)?;
            sim.call(&Action::StopBreakingBlock)?;
            let mut outcome = sim.outcome();
            outcome.confirmed = true;
            outcome.evidence["player"] = sim.player()?;
            Ok(Next::Done(outcome))
        });
        self.first(game, &sim, &Action::StopMoving, next)
    }

    fn fight(&self, _game: &mut dyn Game, _args: &super::fight::Fight) -> Result<Outcome> {
        bail!(
            "fight plays the connected player (actor self) only; {} is a simulated player, so \
             use attack steps in action_sequence for it.",
            self.name
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaw_gaps_wrap() {
        assert!((angle_gap(179.0, -179.0) - 2.0).abs() < 1e-9);
        assert!((angle_gap(10.0, 350.0) - 20.0).abs() < 1e-9);
    }
}
