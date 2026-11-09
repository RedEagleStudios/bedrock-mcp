//! The input driver: the player who ran /connect, played with synthetic keyboard and mouse
//! input. With the Bridge pack it aims with `re_mcp:aim`, which turns the player toward the
//! target on a game tick and answers with what then lies along its sight, so one write both
//! turns and checks the crosshair; without the pack it turns with /tp in a world with cheats.
//! It confirms with the events the action causes: Bridge script events, or legacy WebSocket
//! events without the pack.
//!
//! Every action sends its input and then finishes as an [`InputPending`]: it waits for its
//! confirming events, ends a timed hold or walks. Outside a sequence the driver polls it until
//! it finished; in a sequence (`defer`) it runs while later steps start, and keeps the
//! [`Controls`] it holds until it lets go of them.

use std::{
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use super::{
    AIM_LEAD, Aim, Confirmation, Controls, Expect, Game, Hand, Movement, Outcome, Pending, Presses,
    Quantity, TICK, TICK_POLL, UseMode, beside, block_of, confirm_after, current_tick,
    entity_point, face_named, face_point,
    fight::{Fight, Fighter},
    first, horizontal, listen,
    native::Readback,
    point_of, rounded,
    walk::{EYE_HEIGHT, Gait, Walked, Walker},
};
use crate::{
    bridge::{Actor, quoted},
    error::{Error, Result, bail},
    events::{EventCursor, HubEvent, Source},
    gametest::Face,
    input::{InputRequest, Movement as Axes},
    world::{World, distance, facing, number, shortest_turn},
};

/// Dropped items spawn this close to the player.
const DROP_RADIUS: f64 = 4.0;
const DEFAULT_ARRIVAL: f64 = 1.0;
const DEFAULT_TRAVEL_MS: u64 = 20_000;
/// How often walking turns toward the destination.
const STEER_INTERVAL: Duration = Duration::from_millis(150);
/// How long movement keys are watched when they stay held.
const PROBE: Duration = Duration::from_millis(250);
/// Less movement than this counts as standing still.
const MOVED: f64 = 0.05;
/// How close a turn has to land, in degrees.
const LOOK_TOLERANCE: f64 = 1.0;
/// How far a player reaches an entity, in blocks along its sight to the entity's body: the
/// Bedrock entity reach on https://minecraft.wiki/w/Interaction_range.
const SURVIVAL_REACH: f64 = 3.0;
const CREATIVE_REACH: f64 = 5.0;
/// How far inside reach `approach` walks before it clicks, so a step or a push does not put
/// the entity back out of reach by the time the click lands.
const APPROACH_MARGIN: f64 = 0.5;
/// How far, in degrees, the player may face off the entity while `approach` walks before it
/// turns again; each turn is a teleport, which also stops the walk for a moment.
const STEER_TOLERANCE: f64 = 15.0;
/// How long `approach` walks without getting [`STALL_PROGRESS`] blocks closer before it gives
/// up as blocked.
const STALL_WINDOW: Duration = Duration::from_millis(700);
const STALL_PROGRESS: f64 = 0.3;
/// How often the driver checks its own action when it waits for it.
const POLL: Duration = Duration::from_millis(10);
/// The events that end a use of an item.
const USE_ENDS: [&str; 3] = ["item_release_use", "item_complete_use", "item_stop_use"];

#[derive(Clone)]
pub struct InputDriver {
    pub confirm: Duration,
    /// Hand back actions that still run as [`Outcome::pending`] instead of waiting.
    pub defer: bool,
    /// The game tick the input is due on, read just before the action started. An action
    /// that aims first and finishes aiming early waits for it.
    pub tick: Option<u64>,
    /// The sequence's inputs so far, so each action counts only its own events.
    pub presses: Presses,
    /// A recent end of the event stream that the sequence read while it had time, used with
    /// a due tick instead of reading one: the tick window keeps older events out.
    pub cursor: Option<EventCursor>,
}

/// What one action knows about the world.
#[derive(Clone)]
struct Scene {
    world: World,
    bridge: bool,
    /// The player's name, from the bridge.
    name: Option<String>,
}

/// What a started action still does: poll it, with the controls it holds, until it gives
/// its outcome. It sets the controls to [`Controls::NONE`] once it let go of them.
type Step = Box<dyn FnMut(&mut dyn Game, &mut Controls) -> Result<Option<Outcome>>>;

/// How an action finishes its outcome with the events that confirmed it (or the notes on
/// why none came).
type Then = Box<dyn FnOnce(&mut dyn Game, &mut Outcome, Confirmation) -> Result<()>>;

/// A started input action and the controls it keeps meanwhile.
struct InputPending {
    holds: Controls,
    step: Step,
}

impl Pending for InputPending {
    fn poll(&mut self, game: &mut dyn Game) -> Result<Option<Outcome>> {
        (self.step)(game, &mut self.holds)
    }

    fn holds(&self) -> Controls {
        self.holds
    }
}

/// Whether the events found so far confirm an input.
type Enough = Box<dyn Fn(&[HubEvent]) -> bool>;

/// How a walk ended (`None`: the time ran out), with the last position and the goal.
type Travelled = (Option<Walked>, [f64; 3], [f64; 3]);

/// What confirms an input.
struct Confirm {
    /// Bridge event types.
    names: &'static [&'static str],
    /// Legacy WebSocket events, used when the bridge is absent.
    ws: &'static [&'static str],
    matches: Box<dyn Fn(&HubEvent) -> bool>,
    enough: Enough,
    timeout: Duration,
}

/// The events that confirm one input, collected while other work goes on.
struct Watch {
    bridge: bool,
    player: Option<String>,
    confirm: Confirm,
    cursor: EventCursor,
    /// The control pressed and the game tick it went out on: events from before that tick,
    /// or from the next input of the same control on, belong to other inputs.
    press: Option<(&'static str, u64)>,
    presses: Presses,
    started: Instant,
}

impl Watch {
    /// The confirmation once enough events came or the time ran out; `None` while they may
    /// still come.
    fn check(&self, game: &mut dyn Game) -> Result<Option<Confirmation>> {
        let window = self
            .press
            .map(|(control, from)| (from, self.presses.next_after(control, from)));
        let matches = |event: &HubEvent| {
            let ours = match (window, event.tick) {
                (Some((from, until)), Some(tick)) => {
                    tick >= from && until.is_none_or(|until| tick < until)
                }
                _ => true,
            };
            ours && (self.confirm.matches)(event)
        };
        let expect = Expect {
            bridge: self.confirm.names,
            ws: self.confirm.ws,
            player: self.player.as_deref(),
            matches: &matches,
        };
        let last = self.started.elapsed() >= self.confirm.timeout;
        let found = confirm_after(
            game,
            self.bridge,
            &self.cursor,
            Duration::ZERO,
            self.confirm.timeout,
            &expect,
            &*self.confirm.enough,
        )?;
        Ok((last || (self.confirm.enough)(&found.events)).then_some(found))
    }
}

/// What a turn did: the angles it aimed at and, with the bridge, the `re_mcp:aim` reply
/// (the tick it ran on, the player's angles and its new sight).
struct Turned {
    yaw: f64,
    pitch: f64,
    reply: Option<Value>,
}

impl Turned {
    fn angles(&self) -> Value {
        json!({ "yaw": rounded(self.yaw), "pitch": rounded(self.pitch) })
    }

    fn tick(&self) -> Option<u64> {
        self.reply.as_ref().and_then(|reply| reply["tick"].as_u64())
    }

    /// The block and entity along the new sight, as `re_mcp:target` reports them.
    fn sight(&self) -> Option<Value> {
        self.reply
            .as_ref()
            .map(|reply| json!({ "block": reply["block"], "entity": reply["entity"] }))
    }
}

/// Whether an entity can be clicked from where the player stands.
enum Reach {
    /// Within reach, or nothing tells (no bridge).
    Within,
    /// Something else is in the crosshair; the note says what.
    Blocked(String),
    /// On the crosshair but beyond reach.
    Far { distance: f64, reach: f64 },
}

/// Whether the entity `id` lies on `turned`'s new sight and within the reach of the player's
/// game mode, less `margin` blocks.
fn reach_of(turned: &Turned, id: &str, margin: f64) -> Reach {
    let Some(reply) = &turned.reply else {
        return Reach::Within;
    };
    if reply["entity"]["id"].as_str() != Some(id) {
        return Reach::Blocked(format!(
            "The crosshair is not on entity {id}, so nothing was pressed: {}. Something is in \
             the way.",
            under_crosshair(reply)
        ));
    }
    let creative = reply["gameMode"]
        .as_str()
        .is_some_and(|mode| mode.eq_ignore_ascii_case("creative"));
    let reach = if creative {
        CREATIVE_REACH
    } else {
        SURVIVAL_REACH
    } - margin;
    match reply["entity"]["distance"].as_f64() {
        Some(distance) if distance > reach => Reach::Far { distance, reach },
        _ => Reach::Within,
    }
}

/// How far the measured point is from the player's eyes: from an aim reply's eye and point,
/// else the entity distance on the sight.
fn gap_of(turned: &Turned) -> f64 {
    let Some(reply) = &turned.reply else {
        return f64::NAN;
    };
    match (point_of(&reply["eye"]), point_of(&reply["point"])) {
        (Some(eye), Some(point)) => distance(eye, point),
        _ => reply["entity"]["distance"].as_f64().unwrap_or(f64::NAN),
    }
}

/// What stands between the player and the point it walks to, `gap` blocks away: the block on
/// its sight, else a block a feet-level ray toward the point meets.
fn obstacle(scene: &Scene, measured: &Turned, gap: f64) -> String {
    let Some(reply) = &measured.reply else {
        return "nothing tells what is in the way without the Bridge pack".to_owned();
    };
    let at = |block: &Value| {
        block_of(&block["location"])
            .map_or_else(String::new, |[x, y, z]| format!(" at {x} {y} {z}"))
    };
    if let Some(id) = reply["block"]["id"].as_str()
        && reply["block"]["distance"]
            .as_f64()
            .is_some_and(|ahead| ahead < gap)
    {
        return format!("{id}{} is on the sight to it", at(&reply["block"]));
    }
    if let (Some(eye), Some(point)) = (point_of(&reply["eye"]), point_of(&reply["point"])) {
        let flat = horizontal(eye, point).max(f64::EPSILON);
        let spec = format!(
            "x={};y={};z={};dx={};dy=0;dz={};max=1.5;entities=0",
            number(eye[0]),
            number(eye[1] - EYE_HEIGHT + 0.5),
            number(eye[2]),
            number((point[0] - eye[0]) / flat),
            number((point[2] - eye[2]) / flat)
        );
        if let Ok(ray) = quoted(&spec).and_then(|spec| {
            scene
                .world
                .bridge_command(&format!("re_mcp:raycast {spec}"))
        }) && let Some(id) = ray["block"]["id"].as_str()
        {
            return format!(
                "{id}{} stands in front of the player's feet",
                at(&ray["block"])
            );
        }
    }
    "neither the sight nor a ray at the feet shows a block, so the player may be stuck on an \
     edge, in a hole or against an entity"
        .to_owned()
}

fn press(control: &str) -> InputRequest {
    InputRequest {
        press: vec![control.to_owned()],
        ..InputRequest::default()
    }
}

fn hold(control: &str) -> InputRequest {
    InputRequest {
        hold: vec![control.to_owned()],
        ..InputRequest::default()
    }
}

fn release(control: &str) -> InputRequest {
    InputRequest {
        release: vec![control.to_owned()],
        ..InputRequest::default()
    }
}

/// Hold or release key.forward.
fn forward(down: bool) -> InputRequest {
    InputRequest {
        r#move: Some(Axes {
            forward: Some(if down { 1.0 } else { 0.0 }),
            strafe: Some(0.0),
        }),
        ..InputRequest::default()
    }
}

fn cannot_aim(error: &Error) -> String {
    format!(
        "The player cannot be aimed ({error}). Aiming needs the Bridge pack (exact look) or a \
         world with cheats (/tp). Without either, leave the target out to act on whatever is \
         under the crosshair."
    )
}

/// Whether `target` (a `re_mcp:target` reply) shows the block at `at`, on `face` when given.
fn on_block(target: &Value, at: [i64; 3], face: Option<Face>) -> bool {
    block_of(&target["block"]["location"]) == Some(at)
        && face
            .is_none_or(|face| target["block"]["face"].as_str().and_then(face_named) == Some(face))
}

/// What the crosshair shows, for notes.
fn under_crosshair(target: &Value) -> String {
    if let Some(id) = target["entity"]["id"].as_str() {
        return format!("it shows entity {id} ({})", target["entity"]["type"]);
    }
    match block_of(&target["block"]["location"]) {
        Some([x, y, z]) => format!(
            "it shows {} at {x} {y} {z}, face {}",
            target["block"]["id"], target["block"]["face"]
        ),
        None => "nothing is within reach".to_owned(),
    }
}

/// The `re_mcp:aim` options that aim at `aim`.
fn aim_spec(aim: &Aim) -> String {
    let point = |[x, y, z]: [f64; 3]| format!("x={};y={};z={}", number(x), number(y), number(z));
    match aim {
        Aim::Entity(id) => format!("entity={id}"),
        Aim::Position(at) => point(*at),
        Aim::Block(at, face) => point(face_point(*at, *face)),
    }
}

/// Finish with the events as they are.
fn confirmed() -> Then {
    Box::new(|_, outcome, found| {
        outcome.confirm_with(found);
        Ok(())
    })
}

/// Wait for `watch`, then finish `outcome` with `then`.
fn awaiting(outcome: Outcome, watch: Watch, then: Then) -> Step {
    let mut done = Some((outcome, then));
    Box::new(move |game, _| {
        let Some(found) = watch.check(game)? else {
            return Ok(None);
        };
        let (mut outcome, then) = done.take().expect("a finished action is not polled");
        then(game, &mut outcome, found)?;
        Ok(Some(outcome))
    })
}

/// Poll an attack's health readback until it finishes `outcome`.
fn read_back(outcome: Outcome, mut readback: Readback) -> Step {
    let mut done = Some(outcome);
    Box::new(move |game, _| {
        let outcome = done.as_mut().expect("a finished action is not polled");
        Ok(readback.poll(game, outcome).and_then(|()| done.take()))
    })
}

/// The clicks an entity action makes, and the events that confirm them.
#[derive(Clone, Copy)]
struct Click {
    control: &'static str,
    /// What the action does, for notes: "attacked" or "interacted with".
    verb: &'static str,
    on_entity: &'static [&'static str],
    on_anything: &'static [&'static str],
    ws: &'static [&'static str],
}

impl InputDriver {
    fn outcome(&self) -> Outcome {
        Outcome::new(&Actor::Me, "input")
    }

    /// The world and whether the bridge answers; with `named`, the player's name too (actions
    /// that aim get it from the aim instead).
    fn scene(&self, game: &mut dyn Game, named: bool) -> Result<Scene> {
        let world = game.world()?;
        let bridge = world.bridge(true)?;
        let name = if bridge && named {
            world.bridge_command("re_mcp:state")?["name"]
                .as_str()
                .map(str::to_owned)
        } else {
            None
        };
        Ok(Scene {
            world,
            bridge,
            name,
        })
    }

    /// Turn the player toward `aim`: one `re_mcp:aim` write with the bridge, else /tp from
    /// the querytarget eye. `None`, with the reason in the notes, when it cannot be turned: no
    /// bridge and no cheats.
    fn turn(&self, scene: &mut Scene, aim: &Aim, outcome: &mut Outcome) -> Result<Option<Turned>> {
        if scene.bridge {
            return Self::aim_write(scene, &aim_spec(aim)).map(Some);
        }
        let turned = (|| -> Result<(f64, f64)> {
            let point = match aim {
                Aim::Position(point) => *point,
                Aim::Block(at, face) => face_point(*at, *face),
                Aim::Entity(id) => entity_point(&scene.world, false, id)?,
            };
            let eye = scene.world.actor_target(&Actor::Me)?.position;
            let (yaw, pitch) = facing(eye, point);
            let pitch = pitch.clamp(-90.0, 90.0);
            scene
                .world
                .look_as(&Actor::Me, yaw, pitch, false, 0, &mut || Ok(()))?;
            Ok((yaw, pitch))
        })();
        match turned {
            Ok((yaw, pitch)) => Ok(Some(Turned {
                yaw,
                pitch,
                reply: None,
            })),
            Err(error) => {
                outcome.notes.push(cannot_aim(&error));
                Ok(None)
            }
        }
    }

    /// Where `aim` lies from the player's eyes and what its sight toward it shows, without
    /// turning; needs the bridge.
    fn measure(&self, scene: &mut Scene, aim: &Aim) -> Result<Turned> {
        Self::aim_write(scene, &format!("{};turn=0", aim_spec(aim)))
    }

    /// One `re_mcp:aim` write with options `spec`.
    fn aim_write(scene: &mut Scene, spec: &str) -> Result<Turned> {
        let reply = scene
            .world
            .bridge_write(&format!("re_mcp:aim {}", quoted(spec)?))?;
        if scene.name.is_none() {
            scene.name = reply["name"].as_str().map(str::to_owned);
        }
        let angle = |key: &str| reply["requested"][key].as_f64().unwrap_or(f64::NAN);
        Ok(Turned {
            yaw: angle("yaw"),
            pitch: angle("pitch"),
            reply: Some(reply),
        })
    }

    /// The crosshair read with the bridge.
    fn crosshair(&self, scene: &Scene) -> Result<Option<Value>> {
        if scene.bridge {
            Ok(Some(scene.world.bridge_command("re_mcp:target")?))
        } else {
            Ok(None)
        }
    }

    /// Where the events that confirm the input start: the sequence's recent cursor with a
    /// due tick, else the end of the stream now.
    fn cursor(&self, game: &mut dyn Game) -> Result<EventCursor> {
        match (&self.cursor, self.tick) {
            (Some(cursor), Some(_)) => Ok(cursor.clone()),
            _ => game.cursor(),
        }
    }

    /// The game tick the input goes out on. After an aim that ran on tick `aimed`, wait for
    /// the due tick when the aim finished early, so the click lands on its tick.
    fn send_tick(&self, scene: &Scene, aimed: Option<u64>) -> Result<Option<u64>> {
        let Some(aimed) = aimed else {
            return Ok(self.tick);
        };
        let Some(due) = self.tick else {
            return Ok(Some(aimed));
        };
        let mut now = aimed;
        let until = Instant::now() + TICK * (AIM_LEAD as u32 + 1);
        while now < due && Instant::now() < until {
            thread::sleep(TICK_POLL);
            now = current_tick(&scene.world)?;
        }
        Ok(Some(now))
    }

    /// Send `request`, an input of `control` that goes out on game tick `sent`, and watch
    /// for the events after `cursor` that confirm it.
    #[allow(clippy::too_many_arguments)]
    fn fire(
        &self,
        game: &mut dyn Game,
        scene: &Scene,
        cursor: EventCursor,
        request: InputRequest,
        control: &'static str,
        sent: Option<u64>,
        confirm: Confirm,
        outcome: &mut Outcome,
    ) -> Result<Watch> {
        listen(&scene.world, scene.bridge, confirm.ws);
        game.input(request)?;
        if let Some(tick) = sent {
            self.presses.record(control, tick);
            outcome.evidence["acted_tick"] = tick.into();
        }
        Ok(Watch {
            bridge: scene.bridge,
            player: scene.name.clone(),
            confirm,
            cursor,
            press: sent.map(|tick| (control, tick)),
            presses: self.presses.clone(),
            started: Instant::now(),
        })
    }

    /// What confirms an input: one event of `names` (or `ws`) that `matches` accepts.
    fn confirming(
        &self,
        names: &'static [&'static str],
        ws: &'static [&'static str],
        matches: impl Fn(&HubEvent) -> bool + 'static,
    ) -> Confirm {
        Confirm {
            names,
            ws,
            matches: Box::new(matches),
            enough: Box::new(first),
            timeout: self.confirm,
        }
    }

    /// Finish a started action: wait for it, or with `defer` hand it back to be polled.
    fn settle(&self, game: &mut dyn Game, holds: Controls, step: Step) -> Result<Outcome> {
        let mut pending = InputPending { holds, step };
        if self.defer {
            let mut outcome = self.outcome();
            outcome.pending = Some(Box::new(pending));
            return Ok(outcome);
        }
        loop {
            if let Some(outcome) = pending.poll(game)? {
                return Ok(outcome);
            }
            thread::sleep(POLL);
        }
    }

    /// Attack or interact: aim at the entity, check the crosshair and the reach, walk closer
    /// with `approach`, click.
    fn entity_click(
        &self,
        game: &mut dyn Game,
        entity: Option<&str>,
        click: Click,
        approach: Option<Duration>,
    ) -> Result<Outcome> {
        let mut outcome = self.outcome();
        let Some(id) = entity else {
            let scene = self.scene(game, true)?;
            if let Some(target) = self.crosshair(&scene)? {
                outcome.evidence["crosshair"] = target;
            }
            let cursor = self.cursor(game)?;
            let confirm = self.confirming(click.on_anything, click.ws, |_| true);
            let request = press(click.control);
            let watch = self.fire(
                game,
                &scene,
                cursor,
                request,
                click.control,
                self.tick,
                confirm,
                &mut outcome,
            )?;
            return self.settle(game, Controls::NONE, awaiting(outcome, watch, confirmed()));
        };
        let mut scene = self.scene(game, false)?;
        let cursor = self.cursor(game)?;
        let aim = Aim::Entity(id.to_owned());
        let Some(turned) = self.turn(&mut scene, &aim, &mut outcome)? else {
            outcome.reason = Some(("error", "The player could not be aimed.".to_owned()));
            return Ok(outcome);
        };
        outcome.evidence["aim"] = turned.angles();
        if let Some(sight) = turned.sight() {
            outcome.evidence["crosshair"] = sight;
        }
        match reach_of(&turned, id, 0.0) {
            Reach::Within => {
                // Without the Bridge pack no event reports a melee hit; the target's health,
                // read natively before and after the swing, can.
                if !scene.bridge && click.control == ATTACK.control {
                    match Readback::before(game, id, self.confirm, &mut outcome)? {
                        Ok(readback) => {
                            game.input(press(click.control))?;
                            return self.settle(game, Controls::NONE, read_back(outcome, readback));
                        }
                        Err(reason) => outcome.notes.push(format!(
                            "The hit could not be read back natively ({reason}), so nothing \
                             confirms it."
                        )),
                    }
                }
                let sent = self.send_tick(&scene, turned.tick())?;
                let watch =
                    self.entity_fire(game, &scene, cursor, id, click, sent, &mut outcome)?;
                self.settle(game, Controls::NONE, awaiting(outcome, watch, confirmed()))
            }
            Reach::Blocked(note) => {
                outcome.reason = Some(("blocked", note.clone()));
                outcome.notes.push(note);
                Ok(outcome)
            }
            Reach::Far { distance, reach } => match approach {
                None => {
                    outcome.reason = Some((
                        "out_of_reach",
                        format!("{distance:.1} blocks away, reach {reach}"),
                    ));
                    outcome.notes.push(format!(
                        "Entity {id} is {distance:.1} blocks away, beyond the {reach} block \
                         reach, so nothing was pressed. Pass approach: true to walk within reach \
                         first, or movement_set with to: the entity id."
                    ));
                    Ok(outcome)
                }
                Some(budget) => {
                    let step = self.approach(scene, id.to_owned(), click, budget, outcome);
                    let holds = Controls::VIEW | Controls::MOVE | controls_of(click);
                    self.settle(game, holds, step)
                }
            },
        }
    }

    /// Click entity `id`, which the crosshair shows, and watch for the event it causes.
    #[allow(clippy::too_many_arguments)]
    fn entity_fire(
        &self,
        game: &mut dyn Game,
        scene: &Scene,
        cursor: EventCursor,
        id: &str,
        click: Click,
        sent: Option<u64>,
        outcome: &mut Outcome,
    ) -> Result<Watch> {
        let id = id.to_owned();
        let matches = move |event: &HubEvent| {
            event.source == Source::Ws || event.body["target"]["id"].as_str() == Some(id.as_str())
        };
        let confirm = self.confirming(click.on_entity, click.ws, matches);
        let request = press(click.control);
        self.fire(
            game,
            scene,
            cursor,
            request,
            click.control,
            sent,
            confirm,
            outcome,
        )
    }

    /// Walk at entity `id` until it is within reach, then click it; give up after `budget`.
    /// Each turn aims at the entity, so key.forward walks toward it.
    fn approach(
        &self,
        mut scene: Scene,
        id: String,
        click: Click,
        budget: Duration,
        outcome: Outcome,
    ) -> Step {
        let driver = self.clone();
        let started = Instant::now();
        let mut walking = false;
        let mut next = started;
        let mut outcome = Some(outcome);
        let mut watch: Option<Watch> = None;
        // When the gap to the entity last shrank by STALL_PROGRESS, and that gap.
        let mut progress: Option<(Instant, f64)> = None;
        let mut stalled: Option<f64> = None;
        Box::new(move |game, holds| {
            if let Some(watch) = &watch {
                let Some(found) = watch.check(game)? else {
                    return Ok(None);
                };
                let mut outcome = outcome.take().expect("a finished action is not polled");
                outcome.confirm_with(found);
                if !outcome.confirmed {
                    outcome.reason = Some((
                        "no_event",
                        format!("No {} event arrived.", click.on_entity.join(" or ")),
                    ));
                }
                return Ok(Some(outcome));
            }
            if Instant::now() < next {
                return Ok(None);
            }
            next = Instant::now() + STEER_INTERVAL;
            let current = outcome.as_mut().expect("a finished action is not polled");
            let aim = Aim::Entity(id.clone());
            let mut step = || -> Result<Option<Turned>> {
                // Measure without turning: a turn is a teleport, which also stops the walk.
                let measured = driver.measure(&mut scene, &aim)?;
                let reach = reach_of(&measured, &id, APPROACH_MARGIN);
                if !matches!(reach, Reach::Far { .. }) || started.elapsed() >= budget {
                    return Ok(Some(measured));
                }
                if walking {
                    // Give up early when walking stopped closing the gap.
                    let gap = gap_of(&measured);
                    match progress {
                        Some((_, best)) if best - gap >= STALL_PROGRESS => {
                            progress = Some((Instant::now(), gap));
                        }
                        Some((since, _)) if since.elapsed() >= STALL_WINDOW => {
                            stalled = Some(gap);
                            return Ok(Some(measured));
                        }
                        Some(_) => {}
                        None => progress = Some((Instant::now(), gap)),
                    }
                }
                let facing = measured
                    .reply
                    .as_ref()
                    .map_or(f64::NAN, |reply| reply["yaw"].as_f64().unwrap_or(f64::NAN));
                let off = shortest_turn(facing, measured.yaw).abs();
                if !walking || off.is_nan() || off > STEER_TOLERANCE {
                    driver.turn(&mut scene, &aim, current)?;
                }
                if !walking {
                    game.input(forward(true))?;
                    walking = true;
                }
                Ok(None)
            };
            let stopped = match step() {
                Ok(None) => return Ok(None),
                Ok(Some(measured)) => Ok(measured),
                Err(error) => Err(error),
            };
            // Done walking, or failed (the entity may have died or gone): let go first.
            if walking {
                game.input(forward(false))?;
            }
            *holds = Controls::NONE;
            let measured = stopped?;
            let close = !matches!(reach_of(&measured, &id, APPROACH_MARGIN), Reach::Far { .. });
            current.evidence["approach"] = json!({
                "walked_ms": started.elapsed().as_millis() as u64,
                "reached": close,
                "distance": measured.reply.as_ref().map(|reply| reply["entity"]["distance"].clone()),
            });
            if let Some(gap) = stalled {
                let what = obstacle(&scene, &measured, gap);
                current.evidence["approach"]["blocked"] = true.into();
                current.notes.push(format!(
                    "Walking toward entity {id} stopped closing the gap ({gap:.1} blocks left, \
                     less than {STALL_PROGRESS} blocks closer in {} ms), so it was not {}: {what}. \
                     approach walks straight and does not find paths; walk around first with \
                     movement_set, which does, or use fight.",
                    STALL_WINDOW.as_millis(),
                    click.verb
                ));
                current.reason = Some(("blocked", what));
                return Ok(outcome.take());
            }
            let (cursor, turned) = if close {
                let cursor = game.cursor()?;
                let turned = driver
                    .turn(&mut scene, &aim, current)?
                    .ok_or_else(|| Error::msg("approach needs the Bridge pack to aim"))?;
                (Some(cursor), turned)
            } else {
                (None, measured)
            };
            current.evidence["aim"] = turned.angles();
            if let Some(sight) = turned.sight() {
                current.evidence["crosshair"] = sight;
            }
            match (reach_of(&turned, &id, 0.0), cursor) {
                (Reach::Within, Some(cursor)) => {
                    let sent = driver.send_tick(&scene, turned.tick())?;
                    watch =
                        Some(driver.entity_fire(game, &scene, cursor, &id, click, sent, current)?);
                    Ok(None)
                }
                (Reach::Blocked(note), _) => {
                    current.reason = Some(("blocked", note.clone()));
                    current.notes.push(note);
                    Ok(outcome.take())
                }
                (_, _) => {
                    let distance = turned
                        .reply
                        .as_ref()
                        .and_then(|reply| reply["entity"]["distance"].as_f64())
                        .unwrap_or(f64::NAN);
                    current.reason = Some((
                        "out_of_reach",
                        format!("{distance:.1} blocks away after approach_ms"),
                    ));
                    current.notes.push(format!(
                        "Walked toward entity {id} for {} ms, but it was still {distance:.1} \
                         blocks away, so it was not {}. approach walks straight at it and does \
                         not find paths; give more approach_ms, or walk there with movement_set.",
                        started.elapsed().as_millis(),
                        click.verb
                    ));
                    Ok(outcome.take())
                }
            }
        })
    }

    /// The player's feet: the bridge location, or the querytarget eye position lowered.
    fn position(scene: &Scene) -> Result<[f64; 3]> {
        if scene.bridge {
            return point_of(&scene.world.bridge_command("re_mcp:state")?["location"])
                .ok_or_else(|| Error::msg("re_mcp:state has no location."));
        }
        let [x, y, z] = scene.world.actor_target(&Actor::Me)?.position;
        Ok([x, y - EYE_HEIGHT, z])
    }

    fn goal(scene: &Scene, to: &super::Destination) -> Result<[f64; 3]> {
        match to {
            super::Destination::Position(at) => Ok([at.x, at.y, at.z]),
            super::Destination::Entity(id) => {
                if scene.bridge {
                    let entity = scene
                        .world
                        .bridge_command(&format!("re_mcp:entity {}", quoted(id)?))?;
                    return point_of(&entity["location"])
                        .ok_or_else(|| Error::msg(format!("Entity {id} has no location.")));
                }
                entity_point(&scene.world, false, id)
            }
        }
    }

    /// Walk to `to` along a path, one [`Walker`] step each tick, until arrived, blocked or
    /// out of time.
    fn travel(
        &self,
        game: &mut dyn Game,
        scene: Scene,
        movement: &Movement,
        to: &super::Destination,
    ) -> Result<Outcome> {
        let mut outcome = self.outcome();
        let gait = Gait {
            radius: movement.arrival_radius.unwrap_or(DEFAULT_ARRIVAL),
            water: !movement.avoid_water.unwrap_or(true),
            sprint: movement.sprint == Some(true),
            jump_held: movement.jump == Some(true),
        };
        let mut walker = Walker::new(scene.world.clone(), scene.bridge, gait);
        let from = match walker.reader.read(game) {
            Ok(pose) => pose.feet,
            Err(error) if !scene.bridge => {
                outcome.notes.push(cannot_aim(&error));
                return Ok(outcome);
            }
            Err(error) => return Err(error),
        };
        if let Some(tick) = self.tick {
            outcome.evidence["acted_tick"] = tick.into();
        }
        if gait.jump_held {
            game.input(InputRequest {
                jump: Some(true),
                ..InputRequest::default()
            })?;
        }
        let to = to.clone();
        let started = Instant::now();
        let deadline =
            started + Duration::from_millis(movement.timeout_ms.unwrap_or(DEFAULT_TRAVEL_MS));
        let mut next = started;
        let mut outcome = Some(outcome);
        let step: Step = Box::new(move |game, holds| {
            if Instant::now() < next {
                return Ok(None);
            }
            next = Instant::now() + TICK;
            let mut walked = || -> Result<Option<Travelled>> {
                let goal = Self::goal(&scene, &to)?;
                let pose = walker.reader.read(game)?;
                if Instant::now() >= deadline && horizontal(pose.feet, goal) > gait.radius {
                    return Ok(Some((None, pose.feet, goal)));
                }
                let end = walker.step(game, pose, goal)?;
                Ok(end.map(|end| (Some(end), pose.feet, goal)))
            };
            let walked = walked();
            if matches!(walked, Ok(None)) {
                return Ok(None);
            }
            walker.release(game)?;
            if gait.jump_held {
                game.input(InputRequest {
                    jump: Some(false),
                    ..InputRequest::default()
                })?;
            }
            *holds = Controls::NONE;
            let mut outcome = outcome.take().expect("a finished action is not polled");
            outcome.notes.extend(walker.take_notes());
            outcome.evidence["path"] = walker.evidence();
            let (end, at, goal) = match walked {
                Ok(Some(walked)) => walked,
                Ok(None) => unreachable!("handled above"),
                Err(error) if !scene.bridge => {
                    outcome.notes.push(cannot_aim(&error));
                    return Ok(Some(outcome));
                }
                Err(error) => return Err(error),
            };
            let gap = horizontal(at, goal);
            outcome.confirmed = matches!(end, Some(Walked::Arrived));
            outcome.source = walker.reader.source();
            outcome.evidence["from"] = json!(from);
            outcome.evidence["at"] = json!(at);
            outcome.evidence["target"] = json!(goal);
            outcome.evidence["distance"] = rounded(gap).into();
            outcome.evidence["elapsed_ms"] = (started.elapsed().as_millis() as u64).into();
            match end {
                Some(Walked::Arrived) => {}
                None => outcome.notes.push(format!(
                    "Stopped {} blocks from the target when the time ran out.",
                    rounded(gap)
                )),
                Some(Walked::Blocked(why)) => {
                    outcome.reason = Some(("blocked", why.clone()));
                    outcome.notes.push(format!(
                        "Stopped {} blocks from the target: {why}.",
                        rounded(gap)
                    ));
                }
                Some(Walked::Liquid(id, block)) => {
                    outcome.evidence["liquid"] = json!({ "id": id, "block": block });
                    outcome.notes.push(format!(
                        "Stopped {} blocks from the target at the edge of {id} at {block:?}. Walk \
                         around it, or pass avoid_water: false to walk in.",
                        rounded(gap)
                    ));
                }
            }
            Ok(Some(outcome))
        });
        self.settle(game, Controls::VIEW | Controls::MOVE, step)
    }

    /// Fight the nearest matching mobs, one [`Fighter`] tick at a time.
    fn fight_with(&self, game: &mut dyn Game, args: &Fight) -> Result<Outcome> {
        let mut fighter = Fighter::start(game, args)?;
        let step: Step = Box::new(move |game, holds| {
            let done = fighter.poll(game)?;
            if done.is_some() {
                *holds = Controls::NONE;
            }
            Ok(done)
        });
        self.settle(
            game,
            Controls::VIEW | Controls::MOVE | Controls::ATTACK,
            step,
        )
    }

    /// Press the manual movement keys, for `duration_ms` or until released later.
    fn walk(&self, game: &mut dyn Game, scene: Scene, movement: &Movement) -> Result<Outcome> {
        let mut outcome = self.outcome();
        let from = Self::position(&scene);
        let axes = (movement.forward.is_some() || movement.strafe.is_some()).then_some(Axes {
            forward: movement.forward,
            strafe: movement.strafe,
        });
        game.input(InputRequest {
            r#move: axes,
            jump: movement.jump,
            sneak: movement.sneak,
            sprint: movement.sprint,
            ..InputRequest::default()
        })?;
        if let Some(tick) = self.tick {
            outcome.evidence["acted_tick"] = tick.into();
        }
        let zero = |value: Option<f64>| value.map(|_| 0.0);
        let up = |flag: Option<bool>| flag.filter(|down| *down).map(|_| false);
        // Released when a duration ends; the keys stay held without one.
        let letting_go = movement.duration_ms.map(|ms| {
            let request = InputRequest {
                r#move: axes.map(|axes| Axes {
                    forward: zero(axes.forward),
                    strafe: zero(axes.strafe),
                }),
                jump: up(movement.jump),
                sneak: up(movement.sneak),
                sprint: up(movement.sprint),
                ..InputRequest::default()
            };
            (Duration::from_millis(ms), request)
        });
        if letting_go.is_none() {
            outcome.notes.push(
                "The keys stay held; release them with movement_set (zeros or false) or the stop \
                 action."
                    .to_owned(),
            );
        }
        let holds = if letting_go.is_some() {
            Controls::MOVE
        } else {
            Controls::NONE
        };
        let moves = movement.moves();
        let started = Instant::now();
        let mut outcome = Some(outcome);
        let step: Step = Box::new(move |game, holds| {
            let wait = letting_go.as_ref().map_or(PROBE, |(duration, _)| *duration);
            if started.elapsed() < wait {
                return Ok(None);
            }
            if let Some((_, request)) = &letting_go {
                game.input(request.clone())?;
            }
            *holds = Controls::NONE;
            let mut outcome = outcome.take().expect("a finished action is not polled");
            match (&from, Self::position(&scene)) {
                (Ok(from), Ok(to)) => {
                    let moved = distance(*from, to);
                    outcome.evidence["from"] = json!(from);
                    outcome.evidence["to"] = json!(to);
                    outcome.evidence["moved"] = rounded(moved).into();
                    outcome.source = if scene.bridge { "bridge" } else { "command" };
                    outcome.confirmed = !moves || moved > MOVED;
                    if !outcome.confirmed {
                        outcome.notes.push(
                            "The player did not move; a screen may be open, or something blocks \
                             the way."
                                .to_owned(),
                        );
                    }
                }
                (Err(error), _) => outcome.notes.push(format!(
                    "The position could not be read, so the movement is unconfirmed: {error}"
                )),
                (_, Err(error)) => outcome.notes.push(format!(
                    "The position could not be read, so the movement is unconfirmed: {error}"
                )),
            }
            Ok(Some(outcome))
        });
        self.settle(game, holds, step)
    }

    /// Aim at the block `at` (on `face`) and check the crosshair shows it; the face the
    /// crosshair shows, or `None` (with the note) when something else is there. Without the
    /// bridge nothing reads the crosshair, so `face` comes back as given.
    fn aim_block(
        &self,
        scene: &mut Scene,
        at: [i64; 3],
        face: Option<Face>,
        what: &str,
        outcome: &mut Outcome,
    ) -> Result<Option<(Option<Face>, Option<u64>)>> {
        let Some(turned) = self.turn(scene, &Aim::Block(at, face), outcome)? else {
            return Ok(None);
        };
        outcome.evidence["aim"] = turned.angles();
        let Some(sight) = turned.sight() else {
            return Ok(Some((face, None)));
        };
        let on = on_block(&sight, at, face);
        if !on {
            outcome.notes.push(format!(
                "The crosshair is not on {what}, so nothing was pressed: {}.",
                under_crosshair(&sight)
            ));
        }
        let shown = face.or_else(|| sight["block"]["face"].as_str().and_then(face_named));
        outcome.evidence["crosshair"] = sight;
        Ok(on.then_some((shown, turned.tick())))
    }
}

/// The item in the connected player's hand, as `re_mcp:item` reports it; `None` when empty.
fn held_item(world: &World, hand: Hand) -> Result<Option<Value>> {
    let spec = match hand {
        Hand::Offhand => "equipment=Offhand".to_owned(),
        Hand::Mainhand => {
            let slot = world.bridge_command("re_mcp:state")?["selectedSlot"]
                .as_u64()
                .unwrap_or(0);
            format!("slot={slot}")
        }
    };
    let item = world.bridge_command(&format!("re_mcp:item {}", quoted(&spec)?))?["item"].take();
    Ok((!item.is_null()).then_some(item))
}

/// Whether an entity of type `entity` is a projectile the item `item` throws or shoots.
fn projectile_of(item: &str, entity: &str) -> bool {
    let name = |id: &str| id.rsplit(':').next().unwrap_or(id).to_owned();
    let (item, entity) = (name(item), name(entity));
    if matches!(entity.as_str(), "item" | "player" | "xp_orb") {
        return false;
    }
    let vanilla = match item.as_str() {
        "bow" | "crossbow" => Some("arrow"),
        "trident" => Some("thrown_trident"),
        "experience_bottle" => Some("xp_bottle"),
        "fire_charge" => Some("small_fireball"),
        "wind_charge" => Some("wind_charge_projectile"),
        _ => None,
    };
    vanilla.is_some_and(|kind| entity == kind) || entity.contains(&item) || item.contains(&entity)
}

/// What an item use needs to tell why the game showed no use: the hand, the item held before
/// the click (read outside sequences) and the sequence's last used item.
struct Usage {
    world: World,
    bridge: bool,
    hand: Hand,
    before: Option<Value>,
    presses: Presses,
}

impl Usage {
    /// Remember a confirmed use's item; give an unconfirmed one its reason.
    fn settle(&self, outcome: &mut Outcome) {
        if outcome.confirmed {
            if let Some(id) = outcome.evidence["event"]["body"]["item"]["id"].as_str() {
                self.presses.used(id);
            }
            return;
        }
        if !self.bridge {
            outcome.reason = Some(("no_event", "No event confirmed the use.".to_owned()));
            return;
        }
        let (class, detail) = self
            .explain()
            .unwrap_or_else(|error| ("no_event", format!("The hand could not be read: {error}")));
        outcome
            .notes
            .push(format!("Why no use showed ({class}): {detail}"));
        outcome.reason = Some((class, detail));
    }

    fn explain(&self) -> Result<(&'static str, String)> {
        let held = held_item(&self.world, self.hand)?;
        let known = held
            .as_ref()
            .or(self.before.as_ref())
            .and_then(|item| item["id"].as_str().map(str::to_owned))
            .or_else(|| self.presses.last_item());
        if let Some(item) = &known {
            let around = self.world.bridge_command("re_mcp:entities 48 * 64")?;
            let flying = around["entities"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|entity| {
                    entity["type"]
                        .as_str()
                        .is_some_and(|kind| projectile_of(item, kind))
                });
            if let Some(entity) = flying {
                return Ok((
                    "projectile_in_flight",
                    format!(
                        "{} {} from {item} is still out, {} blocks away; the item may wait for \
                         it to land or return",
                        entity["type"].as_str().unwrap_or("an entity"),
                        entity["id"].as_str().unwrap_or("?"),
                        entity["distance"]
                    ),
                ));
            }
        }
        let Some(item) = held else {
            // The hand emptied after a use of an item that is now nowhere in the inventory: it
            // was thrown and has not come back yet, whatever its projectile entity is called.
            if let Some(item) = &known {
                let inventory = self.world.bridge_command("re_mcp:inventory")?;
                let carried = inventory["slots"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .chain(
                        inventory["equipment"]
                            .as_object()
                            .into_iter()
                            .flat_map(|slots| slots.values()),
                    )
                    .any(|slot| slot["id"].as_str() == Some(item.as_str()));
                if !carried {
                    return Ok((
                        "projectile_in_flight",
                        format!(
                            "The hand is empty and {item}, used just before, is nowhere in the \
                             inventory: it is still thrown or on its way back"
                        ),
                    ));
                }
            }
            return Ok((
                "no_item",
                match &known {
                    Some(item) => format!("The hand is empty now (it held {item})."),
                    None => "The hand is empty.".to_owned(),
                },
            ));
        };
        let id = item["id"].as_str().unwrap_or("the item");
        if let Some(left) = item["cooldown"]["remaining"]
            .as_u64()
            .filter(|left| *left > 0)
        {
            return Ok((
                "cooldown",
                format!(
                    "{id} is on cooldown: {left} of {} ticks left (category {})",
                    item["cooldown"]["ticks"], item["cooldown"]["category"]
                ),
            ));
        }
        Ok((
            "no_event",
            format!(
                "The hand holds {id} with no cooldown and none of its projectiles out, but no \
                 use event came: the item may have no use here, or its add-on script refused it"
            ),
        ))
    }
}

/// The key an entity click presses, as a control it needs.
fn controls_of(click: Click) -> Controls {
    if click.control == "key.attack" {
        Controls::ATTACK
    } else {
        Controls::USE
    }
}

const ATTACK: Click = Click {
    control: "key.attack",
    verb: "attacked",
    on_entity: &["entity_hit_entity"],
    on_anything: &["entity_hit_entity", "entity_hit_block"],
    ws: &[],
};

const INTERACT: Click = Click {
    control: "key.use",
    verb: "interacted with",
    on_entity: &["player_interact_entity"],
    on_anything: &["player_interact_entity", "player_interact_block"],
    ws: &["MobInteracted"],
};

impl super::Driver for InputDriver {
    fn look(&self, game: &mut dyn Game, aim: &Aim) -> Result<Outcome> {
        let mut scene = self.scene(game, false)?;
        let mut outcome = self.outcome();
        let Some(turned) = self.turn(&mut scene, aim, &mut outcome)? else {
            return Ok(outcome);
        };
        outcome.evidence["requested"] = turned.angles();
        match &turned.reply {
            Some(reply) => {
                let (now_yaw, now_pitch) = (
                    reply["yaw"].as_f64().unwrap_or(f64::NAN),
                    reply["pitch"].as_f64().unwrap_or(f64::NAN),
                );
                outcome.evidence["player"] =
                    json!({ "yaw": reply["yaw"], "pitch": reply["pitch"] });
                if let Some(tick) = turned.tick() {
                    outcome.evidence["acted_tick"] = tick.into();
                }
                outcome.source = "bridge";
                outcome.confirmed = shortest_turn(now_yaw, turned.yaw).abs() <= LOOK_TOLERANCE
                    && (now_pitch - turned.pitch).abs() <= LOOK_TOLERANCE;
            }
            None => {
                outcome.source = "command";
                outcome.confirmed = true;
                outcome.notes.push("Turned with /tp.".to_owned());
            }
        }
        Ok(outcome)
    }

    fn move_to(&self, game: &mut dyn Game, movement: &Movement) -> Result<Outcome> {
        let scene = self.scene(game, false)?;
        match &movement.to {
            Some(to) => self.travel(game, scene, movement, to),
            None => self.walk(game, scene, movement),
        }
    }

    fn select_slot(&self, game: &mut dyn Game, slot: u8) -> Result<Outcome> {
        if slot > 8 {
            bail!("Hotbar slot {slot} is outside 0 to 8.");
        }
        let scene = self.scene(game, true)?;
        let mut outcome = self.outcome();
        if scene.bridge {
            let state = scene.world.bridge_command("re_mcp:state")?;
            if state["selectedSlot"] == slot {
                outcome.confirmed = true;
                outcome.source = "bridge";
                outcome.evidence["selectedSlot"] = slot.into();
                return Ok(outcome);
            }
        }
        let cursor = self.cursor(game)?;
        let confirm = self.confirming(&["hotbar_select"], &[], move |event| {
            event.body["to"] == slot
        });
        let request = InputRequest {
            hotbar: Some(slot + 1),
            ..InputRequest::default()
        };
        let watch = self.fire(
            game,
            &scene,
            cursor,
            request,
            "hotbar",
            self.tick,
            confirm,
            &mut outcome,
        )?;
        self.settle(game, Controls::NONE, awaiting(outcome, watch, confirmed()))
    }

    fn use_item(
        &self,
        game: &mut dyn Game,
        mode: UseMode,
        hold_ms: Option<u64>,
        hand: Hand,
    ) -> Result<Outcome> {
        let scene = self.scene(game, true)?;
        let mut outcome = self.outcome();
        if hand == Hand::Offhand {
            outcome.notes.push(
                "The game picks the hand: key.use uses the offhand item (a shield blocks, for \
                 example) when the main hand item has no use of its own."
                    .to_owned(),
            );
        }
        let (request, names, ws): (
            InputRequest,
            &'static [&'static str],
            &'static [&'static str],
        ) = match mode {
            UseMode::Instant => (
                press("key.use"),
                &[
                    "item_use",
                    "item_start_use",
                    "item_complete_use",
                    "item_release_use",
                ],
                &["ItemUsed"],
            ),
            UseMode::Hold => (
                hold("key.use"),
                &[
                    "item_start_use",
                    "item_use",
                    "item_release_use",
                    "item_complete_use",
                    "item_stop_use",
                ],
                &["ItemUsed"],
            ),
            UseMode::Release => (release("key.use"), &USE_ENDS, &[]),
        };
        let timed = match (mode, hold_ms) {
            (UseMode::Hold, Some(ms)) => Some(Duration::from_millis(ms)),
            (UseMode::Hold, None) => {
                outcome.notes.push(
                    "key.use stays down until item_use with mode release (or the stop action)."
                        .to_owned(),
                );
                None
            }
            _ => None,
        };
        let mut confirm = self.confirming(names, ws, |_| true);
        if let Some(held) = timed {
            // A timed hold releases, so wait for the event that ends the use too.
            confirm.enough = Box::new(|found: &[HubEvent]| {
                found
                    .iter()
                    .any(|event| USE_ENDS.contains(&event.name.as_str()))
            });
            confirm.timeout += held;
        }
        let usage = Usage {
            world: scene.world.clone(),
            bridge: scene.bridge,
            hand,
            // Outside a sequence a read before the click costs no timing.
            before: (!self.defer && scene.bridge)
                .then(|| held_item(&scene.world, hand).ok().flatten())
                .flatten(),
            presses: self.presses.clone(),
        };
        let cursor = self.cursor(game)?;
        let watch = self.fire(
            game,
            &scene,
            cursor,
            request,
            "key.use",
            self.tick,
            confirm,
            &mut outcome,
        )?;
        let Some(held) = timed else {
            let then: Then = Box::new(move |_, outcome, found| {
                outcome.confirm_with(found);
                usage.settle(outcome);
                Ok(())
            });
            return self.settle(game, Controls::NONE, awaiting(outcome, watch, then));
        };
        let then: Then = Box::new(move |_, outcome, found| {
            let ended_by = |name: &str| found.events.iter().any(|event| event.name == name);
            let (completed, released) =
                (ended_by("item_complete_use"), ended_by("item_release_use"));
            outcome.confirm_with(found);
            usage.settle(outcome);
            if outcome.confirmed {
                outcome.evidence["completed"] = completed.into();
                outcome.evidence["released"] = released.into();
                if released && !completed {
                    outcome.notes.push(
                        "The hold ended with a release (item_release_use): bows, crossbows, \
                         tridents and add-on shooters fire on release, so watch events_read for \
                         projectile_hit_entity or projectile_hit_block."
                            .to_owned(),
                    );
                } else if !completed {
                    outcome.notes.push(
                        "The item was used but the use did not complete (no item_complete_use): \
                         food at full hunger and holds shorter than the item's use time end this \
                         way."
                            .to_owned(),
                    );
                }
            }
            Ok(())
        });
        let release_at = Instant::now() + held;
        let mut released = false;
        let mut wait = awaiting(outcome, watch, then);
        let step: Step = Box::new(move |game, holds| {
            if !released {
                if Instant::now() < release_at {
                    return Ok(None);
                }
                game.input(release("key.use"))?;
                released = true;
                *holds = Controls::NONE;
            }
            wait(game, holds)
        });
        self.settle(game, Controls::VIEW | Controls::USE, step)
    }

    fn use_on(&self, game: &mut dyn Game, at: [i64; 3], face: Option<Face>) -> Result<Outcome> {
        let mut scene = self.scene(game, false)?;
        let mut outcome = self.outcome();
        let cursor = self.cursor(game)?;
        let what = format!(
            "that block{}",
            face.map(|face| format!(" face {face:?}"))
                .unwrap_or_default()
        );
        let Some((face, aimed)) = self.aim_block(&mut scene, at, face, &what, &mut outcome)? else {
            return Ok(outcome);
        };
        let placed = face.map(|face| beside(at, face));
        let matches = move |event: &HubEvent| match (event.source, event.name.as_str()) {
            (Source::Ws, _) | (_, "item_use") => true,
            (_, "block_place") => {
                placed.is_some() && block_of(&event.body["block"]["location"]) == placed
            }
            _ => block_of(&event.body["block"]["location"]) == Some(at),
        };
        let confirm = self.confirming(
            &[
                "player_interact_block",
                "item_start_use_on",
                "block_place",
                "item_use",
            ],
            &["ItemInteracted", "ItemUsed", "BlockPlaced"],
            matches,
        );
        let sent = self.send_tick(&scene, aimed)?;
        let watch = self.fire(
            game,
            &scene,
            cursor,
            press("key.use"),
            "key.use",
            sent,
            confirm,
            &mut outcome,
        )?;
        self.settle(game, Controls::NONE, awaiting(outcome, watch, confirmed()))
    }

    fn attack(
        &self,
        game: &mut dyn Game,
        entity: Option<&str>,
        approach: Option<Duration>,
    ) -> Result<Outcome> {
        self.entity_click(game, entity, ATTACK, approach)
    }

    fn interact(
        &self,
        game: &mut dyn Game,
        entity: Option<&str>,
        approach: Option<Duration>,
    ) -> Result<Outcome> {
        self.entity_click(game, entity, INTERACT, approach)
    }

    fn break_block(
        &self,
        game: &mut dyn Game,
        at: Option<[i64; 3]>,
        timeout: Duration,
    ) -> Result<Outcome> {
        let mut scene = self.scene(game, at.is_none())?;
        let mut outcome = self.outcome();
        let cursor = self.cursor(game)?;
        let (at, aimed) = match at {
            Some(at) => {
                let what = "the block to break";
                match self.aim_block(&mut scene, at, None, what, &mut outcome)? {
                    Some((_, aimed)) => (Some(at), aimed),
                    None => return Ok(outcome),
                }
            }
            None => {
                let Some(target) = self.crosshair(&scene)? else {
                    return self.mine(game, scene, cursor, None, self.tick, timeout, outcome);
                };
                let at = block_of(&target["block"]["location"]);
                if at.is_none() {
                    outcome.notes.push(format!(
                        "The crosshair is not on a block, so nothing was pressed: {}.",
                        under_crosshair(&target)
                    ));
                }
                outcome.evidence["crosshair"] = target;
                if at.is_none() {
                    return Ok(outcome);
                }
                (at, None)
            }
        };
        let sent = self.send_tick(&scene, aimed)?;
        self.mine(game, scene, cursor, at, sent, timeout, outcome)
    }

    fn place(
        &self,
        game: &mut dyn Game,
        at: [i64; 3],
        face: Face,
        slot: Option<u8>,
    ) -> Result<Outcome> {
        let mut outcome = self.outcome();
        if let Some(slot) = slot {
            let now = InputDriver {
                defer: false,
                ..self.clone()
            };
            let selected = now.select_slot(game, slot)?;
            outcome.evidence["slot"] = selected.to_json();
            if !selected.confirmed {
                outcome.notes.push(format!(
                    "Selecting hotbar slot {slot} was not confirmed; placing with whatever is held."
                ));
            }
        }
        let mut scene = self.scene(game, false)?;
        let cursor = self.cursor(game)?;
        let what = format!("face {face:?} of that block");
        let Some((_, aimed)) = self.aim_block(&mut scene, at, Some(face), &what, &mut outcome)?
        else {
            return Ok(outcome);
        };
        let placed = beside(at, face);
        outcome.evidence["placed_at"] = json!(placed);
        let matches = move |event: &HubEvent| {
            event.source == Source::Ws || block_of(&event.body["block"]["location"]) == Some(placed)
        };
        let confirm = self.confirming(&["block_place"], &["BlockPlaced"], matches);
        let sent = self.send_tick(&scene, aimed)?;
        let watch = self.fire(
            game,
            &scene,
            cursor,
            press("key.use"),
            "key.use",
            sent,
            confirm,
            &mut outcome,
        )?;
        self.settle(game, Controls::NONE, awaiting(outcome, watch, confirmed()))
    }

    fn drop_item(&self, game: &mut dyn Game, quantity: Quantity) -> Result<Outcome> {
        let scene = self.scene(game, false)?;
        let mut outcome = self.outcome();
        let origin = if scene.bridge {
            point_of(&scene.world.bridge_command("re_mcp:state")?["location"])
        } else {
            None
        };
        let (presses, request, wanted) = match quantity {
            Quantity::Whole(_) => (
                1,
                InputRequest {
                    hold: vec!["ControlLeft".to_owned()],
                    press: vec!["key.drop".to_owned()],
                    hold_ms: Some(50),
                    ..InputRequest::default()
                },
                None,
            ),
            Quantity::Count(count) => (count, press("key.drop"), Some(u64::from(count))),
        };
        let matches = move |event: &HubEvent| {
            event.source == Source::Ws
                || (event.body["entity"]["type"] == "minecraft:item"
                    && !event.body["item"].is_null()
                    && origin.is_none_or(|origin| {
                        point_of(&event.body["entity"]["location"])
                            .is_some_and(|at| distance(origin, at) <= DROP_RADIUS)
                    }))
        };
        let amount = |event: &HubEvent| {
            event.body["item"]["amount"]
                .as_u64()
                .or(event.body["count"].as_u64())
                .unwrap_or(1)
        };
        let mut confirm = self.confirming(&["entity_spawn"], &["ItemDropped"], matches);
        confirm.enough = Box::new(move |found: &[HubEvent]| match wanted {
            None => !found.is_empty(),
            Some(count) => found.iter().map(amount).sum::<u64>() >= count,
        });
        // A drop names no player, so it is matched by where the item appears.
        let scene = Scene {
            name: None,
            ..scene
        };
        let cursor = self.cursor(game)?;
        for _ in 1..presses {
            game.input(request.clone())?;
        }
        let watch = self.fire(
            game,
            &scene,
            cursor,
            request,
            "key.drop",
            self.tick,
            confirm,
            &mut outcome,
        )?;
        let then: Then = Box::new(move |_, outcome, found| {
            let dropped: u64 = found.events.iter().map(amount).sum();
            let short = wanted.is_some_and(|count| dropped < count);
            outcome.confirm_with(found);
            if outcome.confirmed {
                outcome.evidence["dropped"] = dropped.into();
            }
            if outcome.confirmed && short {
                outcome.confirmed = false;
                outcome.notes.push(format!(
                    "Only {dropped} of {} items were seen dropping; the slot may have run out.",
                    wanted.unwrap_or_default()
                ));
            }
            Ok(())
        });
        self.settle(game, Controls::NONE, awaiting(outcome, watch, then))
    }

    fn fight(&self, game: &mut dyn Game, args: &Fight) -> Result<Outcome> {
        self.fight_with(game, args)
    }

    fn stop(&self, game: &mut dyn Game) -> Result<Outcome> {
        game.input(InputRequest {
            release_all: true,
            ..InputRequest::default()
        })?;
        let mut outcome = self.outcome();
        outcome.confirmed = true;
        outcome.evidence["released"] = true.into();
        Ok(outcome)
    }
}

impl InputDriver {
    /// Hold key.attack on the block `at` (or whatever the crosshair shows) until it breaks or
    /// `timeout` passes, then let go; read the block when it did not break.
    #[allow(clippy::too_many_arguments)]
    fn mine(
        &self,
        game: &mut dyn Game,
        scene: Scene,
        cursor: EventCursor,
        at: Option<[i64; 3]>,
        sent: Option<u64>,
        timeout: Duration,
        mut outcome: Outcome,
    ) -> Result<Outcome> {
        let matches = move |event: &HubEvent| {
            event.source == Source::Ws
                || at.is_none_or(|at| block_of(&event.body["location"]) == Some(at))
        };
        let mut confirm = self.confirming(&["block_break"], &["BlockBroken"], matches);
        confirm.timeout = timeout;
        let started = Instant::now();
        let watch = self.fire(
            game,
            &scene,
            cursor,
            hold("key.attack"),
            "key.attack",
            sent,
            confirm,
            &mut outcome,
        )?;
        let world = scene.world.clone();
        let bridge = scene.bridge;
        let then: Then = Box::new(move |game, outcome, found| {
            game.input(release("key.attack"))?;
            outcome.evidence["elapsed_ms"] = (started.elapsed().as_millis() as u64).into();
            if let Some([x, y, z]) = at {
                outcome.evidence["location"] = json!([x, y, z]);
                if found.events.is_empty() && bridge {
                    let now = world.bridge_command(&format!("re_mcp:block {x} {y} {z}"))?;
                    outcome.notes.push(format!(
                        "The block at {x} {y} {z} is still {} after {} ms; it may need a better \
                         tool or more time (timeout_ms).",
                        now["id"],
                        timeout.as_millis()
                    ));
                    outcome.evidence["block"] = now;
                }
            }
            outcome.confirm_with(found);
            Ok(())
        });
        let mut wait = awaiting(outcome, watch, then);
        let step: Step = Box::new(move |game, holds| {
            let done = wait(game, holds)?;
            if done.is_some() {
                *holds = Controls::NONE;
            }
            Ok(done)
        });
        self.settle(game, Controls::VIEW | Controls::ATTACK, step)
    }
}
