//! Gameplay actions through interchangeable drivers. The input driver plays the player who
//! ran /connect with synthetic keyboard and mouse input and checks the result with the events
//! the action causes; the GameTest driver calls `SimulatedPlayer` methods on a simulated
//! player. Every action returns an [`Outcome`] that says whether the game confirmed it and
//! what showed it.

mod background;
mod condition;
mod fight;
mod gametest;
mod input;
mod native;
mod path;
mod refs;
mod spawn;
mod summary;
mod terrain;
#[cfg(test)]
mod tests;
mod view;
mod walk;
mod warm;

use std::{
    cell::RefCell,
    collections::HashMap,
    fmt,
    rc::Rc,
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::{
    bridge::{Actor, INSTALL_HINT, quoted},
    error::{Error, Result, bail},
    events::{EventCursor, HubEvent, Source},
    gametest::{Face, Vec3},
    input::InputRequest,
    session::Session,
    world::{World, distance},
};

pub use background::{Background, RunLog, StatusArgs, StopArgs, run_queued};
use condition::{Check, Probe, Watch};
use gametest::GameTestDriver;
use input::InputDriver;
pub use native::{NativeRead, Vitals};
pub use warm::{warm_summary, warm_up};

/// How long an action's confirming event gets to arrive.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(2);
/// One game tick.
const TICK: Duration = Duration::from_millis(50);
const DEFAULT_BREAK_MS: u64 = 10_000;
const MAX_WAIT_MS: u64 = 300_000;
const DEFAULT_SEQUENCE_MS: u64 = 60_000;
const MAX_STEPS: usize = 64;
/// The most runs a repeated step makes.
const MAX_RUNS: u32 = 1000;
const MAX_DROP: u32 = 64;
/// How often a sequence reads the tick while it waits for a step.
const TICK_POLL: Duration = Duration::from_millis(10);
/// How many ticks ahead a sequence sends a simulated player's step, so command round trips
/// do not delay it; the GameTest pack runs it on its tick.
const SEND_AHEAD: u64 = 20;
/// How many ticks ahead a sequence starts an actor self step that aims first (an attack or
/// interaction with an entity, a use on or placement against a block, breaking a block), so
/// the click goes out on its tick: aiming takes a `re_mcp:aim` round trip and its result.
const AIM_LEAD: u64 = 8;
/// How many ticks ahead a sequence starts an actor self look: its `re_mcp:aim` write runs on
/// the game tick after the command arrives, so the turn lands about on its tick.
const TURN_LEAD: u64 = 3;
/// How many ticks of slack a sequence needs before the next send to check running steps.
const SERVICE_SLACK: u64 = 4;
/// How long `approach` walks toward the entity by default.
const DEFAULT_APPROACH_MS: u64 = 5_000;

/// Every action a sequence step can name.
pub const ACTIONS: [&str; 13] = [
    "item_use",
    "item_use_on",
    "attack",
    "entity_interact",
    "block_break",
    "block_place",
    "item_drop",
    "movement_set",
    "fight",
    "look",
    "hotbar_select",
    "spawn",
    "stop",
];

/// What the drivers need from the session. Tests supply a fake game.
pub trait Game {
    /// The connected WebSocket world.
    fn world(&mut self) -> Result<World>;
    /// The end of the merged event stream; take it before acting.
    fn cursor(&mut self) -> Result<EventCursor>;
    /// The first event after `after` that `matches`, waiting up to `timeout`, and notes on
    /// sources that were skipped.
    fn wait_event(
        &mut self,
        after: &EventCursor,
        timeout: Duration,
        matches: &dyn Fn(&HubEvent) -> bool,
    ) -> Result<(Option<HubEvent>, Vec<String>)>;
    /// Synthetic input through the injected agent.
    fn input(&mut self, request: InputRequest) -> Result<()>;
    /// Let `duration` pass at a sequence's wait point. While a background sequence runs, the
    /// session answers other tool calls here.
    fn pause(&mut self, duration: Duration) {
        thread::sleep(duration);
    }
    /// Whether the MCP client went away, so running sequences should stop.
    fn abandoned(&mut self) -> bool {
        false
    }
    /// Start a screen recording with `video_start`'s arguments; its report.
    fn record_start(&mut self, _args: Value) -> Result<Value> {
        bail!("This game cannot record.")
    }
    /// Stop the recording; video_stop's report.
    fn record_stop(&mut self) -> Result<Value> {
        bail!("This game cannot record.")
    }
    /// Mark a chapter in the running recording; nothing without one.
    fn mark(&mut self, _title: &str) {}
    /// What names the loaded world: it changes with each world load. `None` when unknown.
    fn world_key(&mut self) -> Option<String> {
        None
    }
    /// The world key of the connected player's last warm-up.
    fn warm_key(&mut self) -> Option<String> {
        None
    }
    fn set_warm_key(&mut self, _key: Option<String>) {}
    /// Entity `id` as the unstable layer's native read sees it on the game's local server.
    fn native_entity(&mut self, _id: &str) -> NativeRead {
        NativeRead::Unavailable("this game has no unstable layer".to_owned())
    }
    /// Add an event derived from native reads to the merged event stream.
    fn push_native(&mut self, _name: &str, _tick: u64, _body: Value) {}
    /// The held item's attachables as the unstable layer's player_render reads them:
    /// `{animating, players, attachables}`, or `None` when the capability is not active.
    fn attachable(&mut self) -> Result<Option<Value>> {
        Ok(None)
    }
    /// The connected player as the unstable layer's native read sees it on the game's local
    /// server; fails with why when it cannot answer.
    fn native_player(&mut self) -> Result<protocol::unstable::NativePlayer> {
        bail!("this game has no unstable layer")
    }
    /// Entities around the connected player as the unstable layer's native read sees them,
    /// nearest first, as entities_list entries (id, type, location, distance, health), of
    /// type `entity_type` when given; fails with why when it cannot answer. Every game
    /// implements it, so a wrapper never hides the native read from a fight.
    fn native_list(&mut self, radius: f64, entity_type: Option<&str>) -> Result<Vec<Value>>;
}

impl Game for Session {
    fn world(&mut self) -> Result<World> {
        self.ensure_world()
    }

    fn cursor(&mut self) -> Result<EventCursor> {
        self.event_cursor()
    }

    fn wait_event(
        &mut self,
        after: &EventCursor,
        timeout: Duration,
        matches: &dyn Fn(&HubEvent) -> bool,
    ) -> Result<(Option<HubEvent>, Vec<String>)> {
        let waited = Session::wait_event(self, after, timeout, matches)?;
        Ok((waited.event, waited.notes))
    }

    fn input(&mut self, request: InputRequest) -> Result<()> {
        Session::input(self, request).map(drop)
    }

    fn abandoned(&mut self) -> bool {
        self.client_gone()
    }

    fn record_start(&mut self, args: Value) -> Result<Value> {
        let args = serde_json::from_value(args)
            .map_err(|error| Error::msg(format!("Invalid record: {error}")))?;
        Session::record_start(self, args)
    }

    fn record_stop(&mut self) -> Result<Value> {
        Session::record_stop(self)
    }

    fn mark(&mut self, title: &str) {
        Session::mark(self, title);
    }

    fn world_key(&mut self) -> Option<String> {
        self.world_load_key()
    }

    fn warm_key(&mut self) -> Option<String> {
        self.last_warm_key()
    }

    fn set_warm_key(&mut self, key: Option<String>) {
        self.remember_warm_key(key);
    }

    fn native_entity(&mut self, id: &str) -> NativeRead {
        self.native_vitals(id)
    }

    fn push_native(&mut self, name: &str, tick: u64, body: Value) {
        self.push_native_event(name, tick, body);
    }

    fn attachable(&mut self) -> Result<Option<Value>> {
        self.held_attachable()
    }

    fn native_player(&mut self) -> Result<protocol::unstable::NativePlayer> {
        Session::native_player(self, &crate::bridge::Actor::Me)
    }

    fn native_list(&mut self, radius: f64, entity_type: Option<&str>) -> Result<Vec<Value>> {
        let mut notes = Vec::new();
        if !self.native_world(&mut notes) {
            bail!("{}", notes.join("; "));
        }
        let filter = crate::session::world_state::EntityFilter {
            player: None,
            radius,
            entity_type,
        };
        Ok(self.native_entities(&filter)?.0)
    }
}

/// An action that started and has not finished: its confirming event has not come yet, or it
/// runs for a while (walking, mining, holding use). A sequence starts its next steps while
/// earlier ones finish.
pub trait Pending {
    /// Check on the action without blocking for long; its outcome once it finished.
    fn poll(&mut self, game: &mut dyn Game) -> Result<Option<Outcome>>;
    /// The connected player's controls the action still keeps for itself; see [`Controls`].
    fn holds(&self) -> Controls {
        Controls::NONE
    }
}

impl<F> Pending for F
where
    F: FnMut(&mut dyn Game) -> Result<Option<Outcome>>,
{
    fn poll(&mut self, game: &mut dyn Game) -> Result<Option<Outcome>> {
        self(game)
    }
}

impl fmt::Debug for dyn Pending {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("Pending")
    }
}

/// The parts of the connected player's input an action uses. A sequence starts an actor self
/// step while earlier ones still wait for their confirming event, unless an earlier action
/// still holds a control the step needs; then the step waits for it.
///
/// - `VIEW`: where the player looks. Every action that turns or clicks needs it. Walking to a
///   destination keeps it (it steers), and so do breaking a block and a timed hold of
///   key.use: turning would move the mining crosshair or change where a release throws.
/// - `USE`, `ATTACK`: key.use and key.attack. A timed hold keeps key.use and breaking keeps
///   key.attack until they release it, so a second hold or a click of the same key waits.
/// - `MOVE`: the movement keys, kept by walking (to a destination, for duration_ms, or while
///   `approach` closes in on an entity).
///
/// So a look then an attack, or attacks every few ticks, overlap: each click goes out on its
/// tick while the ones before still wait for their events. Two holds of key.use, or an attack
/// during a walk to a destination, do not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Controls(u8);

impl Controls {
    pub const NONE: Self = Self(0);
    pub const VIEW: Self = Self(1);
    pub const USE: Self = Self(2);
    pub const ATTACK: Self = Self(4);
    pub const MOVE: Self = Self(8);

    pub fn overlaps(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }
}

impl std::ops::BitOr for Controls {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// The game ticks the connected player's clicks and holds went out on, by control, shared by
/// the actions of one sequence. A running action counts only the events from its own input:
/// those from its tick until the next input of the same control. It also keeps the item the
/// player last used, so a later use that fails can look for that item's projectile.
#[derive(Clone, Default)]
pub struct Presses {
    inputs: Rc<RefCell<Vec<(&'static str, u64)>>>,
    item: Rc<RefCell<Option<String>>>,
}

impl Presses {
    fn record(&self, control: &'static str, tick: u64) {
        self.inputs.borrow_mut().push((control, tick));
    }

    /// The first tick after `tick` that `control` went out again on.
    fn next_after(&self, control: &str, tick: u64) -> Option<u64> {
        self.inputs
            .borrow()
            .iter()
            .filter(|(pressed, at)| *pressed == control && *at > tick)
            .map(|(_, at)| *at)
            .min()
    }

    fn used(&self, item: &str) {
        *self.item.borrow_mut() = Some(item.to_owned());
    }

    /// The item the player last used in this sequence.
    fn last_item(&self) -> Option<String> {
        self.item.borrow().clone()
    }
}

/// What one action did.
#[derive(Debug)]
pub struct Outcome {
    /// "self" or the simulated player's name.
    pub actor: String,
    /// "input", "gametest", or "command" for a summon.
    pub driver: &'static str,
    /// Whether an event or a readback showed the result.
    pub confirmed: bool,
    /// What `evidence` comes from: "event", "bridge", "command", "gametest" or "input".
    pub source: &'static str,
    pub evidence: Value,
    /// The game tick of the confirming bridge event.
    pub tick: Option<u64>,
    pub notes: Vec<String>,
    /// Why an unconfirmed action did not happen: a class (out_of_reach, blocked, cooldown,
    /// no_item, projectile_in_flight, no_event, error) and what showed it.
    pub reason: Option<(&'static str, String)>,
    /// Set instead of the fields above when a deferring driver started the action and it
    /// still runs; poll it for the real outcome.
    pub pending: Option<Box<dyn Pending>>,
}

/// "self" or the simulated player's name.
fn actor_name(actor: &Actor) -> String {
    match actor {
        Actor::Me => "self".to_owned(),
        Actor::Named(name) => name.clone(),
    }
}

impl Outcome {
    fn new(actor: &Actor, driver: &'static str) -> Self {
        Self {
            actor: actor_name(actor),
            driver,
            confirmed: false,
            source: driver,
            evidence: json!({}),
            tick: None,
            notes: Vec::new(),
            reason: None,
            pending: None,
        }
    }

    /// Take the events that confirm the action, or the notes on why none came.
    fn confirm_with(&mut self, found: Confirmation) {
        self.notes.extend(found.notes);
        let Some(first) = found.events.first() else {
            self.confirmed = false;
            return;
        };
        self.confirmed = true;
        self.source = "event";
        self.tick = first.tick;
        self.evidence["event"] = first.to_json();
        if found.events.len() > 1 {
            self.evidence["events"] = found.events.iter().map(HubEvent::to_json).collect();
        }
    }

    pub fn to_json(&self) -> Value {
        let mut value = json!({
            "actor": self.actor,
            "driver": self.driver,
            "confirmed": self.confirmed,
            "source": self.source,
            "evidence": self.evidence,
            "notes": self.notes,
        });
        if let Some(tick) = self.tick {
            value["tick"] = tick.into();
        }
        if let Some((class, detail)) = self.reason.as_ref().filter(|_| !self.confirmed) {
            value["reason"] = json!({ "class": class, "detail": detail });
        }
        value
    }
}

/// What confirms an action.
pub struct Expect<'a> {
    /// Bridge event types.
    pub bridge: &'a [&'a str],
    /// Legacy WebSocket events, used when the bridge is absent.
    pub ws: &'a [&'a str],
    /// The player a bridge event must name, for event types that name one.
    pub player: Option<&'a str>,
    /// Further checks on an event of the right type.
    pub matches: &'a dyn Fn(&HubEvent) -> bool,
}

/// The events that confirmed an action, and notes on why none did.
pub struct Confirmation {
    pub events: Vec<HubEvent>,
    pub notes: Vec<String>,
}

/// Whether one event is enough.
fn first(found: &[HubEvent]) -> bool {
    !found.is_empty()
}

/// The field of a bridge event that names the acting entity.
fn actor_field(event: &str) -> Option<&'static str> {
    match event {
        "entity_hit_entity" | "entity_hit_block" => Some("attacker"),
        "entity_spawn" | "entity_remove" => None,
        _ => Some("player"),
    }
}

/// Subscribe to the legacy events `ws` that confirm an action when the bridge is absent.
fn listen(world: &World, bridge: bool, ws: &[&str]) {
    if !bridge && !ws.is_empty() {
        let names: Vec<String> = ws.iter().map(|name| (*name).to_owned()).collect();
        world.socket().subscribe(&names);
    }
}

/// Collect the events after `after` that `expect` accepts, until `enough` holds for them or
/// `timeout` passes; the note on a missing event names `waited`, the whole wait, rather than
/// this call's `timeout`.
fn confirm_after(
    game: &mut dyn Game,
    bridge: bool,
    after: &EventCursor,
    timeout: Duration,
    waited: Duration,
    expect: &Expect,
    enough: &dyn Fn(&[HubEvent]) -> bool,
) -> Result<Confirmation> {
    let (source, names) = if bridge {
        (Source::Bridge, expect.bridge)
    } else {
        (Source::Ws, expect.ws)
    };
    if names.is_empty() {
        let note = if bridge {
            "No Bridge event reports this action, so it is unconfirmed."
        } else {
            "No WebSocket event reports this action, so it is unconfirmed. Add the Bridge pack to \
             confirm it with script events."
        };
        return Ok(Confirmation {
            events: Vec::new(),
            notes: vec![note.to_owned()],
        });
    }
    let others = RefCell::new(Vec::new());
    let test = |event: &HubEvent| {
        if event.source != source
            || !names.contains(&event.name.as_str())
            || !(expect.matches)(event)
        {
            return false;
        }
        if let (Source::Bridge, Some(player), Some(field)) =
            (source, expect.player, actor_field(&event.name))
        {
            let by = event.body[field]["name"]
                .as_str()
                .unwrap_or("an unnamed entity");
            if by != player {
                others.borrow_mut().push(format!("{} by {by}", event.name));
                return false;
            }
        }
        true
    };
    let deadline = Instant::now() + timeout;
    let mut cursor = after.clone();
    let mut events = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let (event, skipped) = game.wait_event(&cursor, remaining, &test)?;
        for note in skipped {
            if !notes.contains(&note) {
                notes.push(note);
            }
        }
        let Some(event) = event else { break };
        cursor = event.cursor.clone();
        events.push(event);
        if enough(&events) {
            break;
        }
    }
    if events.is_empty() {
        let by = expect
            .player
            .map(|player| format!(" from {player}"))
            .unwrap_or_default();
        notes.push(format!(
            "No {} event{by} arrived within {} ms.",
            names.join(" or "),
            waited.as_millis()
        ));
        let others = others.into_inner();
        if !others.is_empty() {
            notes.push(format!(
                "Events from others did not count: {}.",
                others.join(", ")
            ));
        }
    }
    Ok(Confirmation { events, notes })
}

/// A point as `[x, y, z]` from `{x, y, z}`.
fn point_of(value: &Value) -> Option<[f64; 3]> {
    Some([
        value["x"].as_f64()?,
        value["y"].as_f64()?,
        value["z"].as_f64()?,
    ])
}

/// The block that holds a point.
fn block_of(value: &Value) -> Option<[i64; 3]> {
    point_of(value).map(|point| point.map(|axis| axis.floor() as i64))
}

fn block(at: Vec3) -> [i64; 3] {
    [at.x, at.y, at.z].map(|axis| axis.floor() as i64)
}

fn vec3([x, y, z]: [f64; 3]) -> Vec3 {
    Vec3 { x, y, z }
}

fn block_vec3(at: [i64; 3]) -> Vec3 {
    vec3(at.map(|axis| axis as f64))
}

fn normal(face: Face) -> [i64; 3] {
    match face {
        Face::Down => [0, -1, 0],
        Face::Up => [0, 1, 0],
        Face::North => [0, 0, -1],
        Face::South => [0, 0, 1],
        Face::East => [1, 0, 0],
        Face::West => [-1, 0, 0],
    }
}

/// The block next to `at` across `face`: where a block placed on that face goes.
fn beside(at: [i64; 3], face: Face) -> [i64; 3] {
    let step = normal(face);
    [at[0] + step[0], at[1] + step[1], at[2] + step[2]]
}

/// The centre of a block's face, or of the block without a face.
fn face_point(at: [i64; 3], face: Option<Face>) -> [f64; 3] {
    let step = face.map_or([0, 0, 0], normal);
    [0, 1, 2].map(|axis| at[axis] as f64 + 0.5 + 0.5 * step[axis] as f64)
}

/// A Script API face name (`North`) as a [`Face`].
fn face_named(name: &str) -> Option<Face> {
    serde_json::from_value(name.to_ascii_lowercase().into()).ok()
}

fn horizontal(a: [f64; 3], b: [f64; 3]) -> f64 {
    distance([a[0], 0.0, a[2]], [b[0], 0.0, b[2]])
}

fn rounded(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// Where to aim at or walk to an entity: halfway between its feet and its eyes with the
/// bridge, else the querytarget position.
fn entity_point(world: &World, bridge: bool, id: &str) -> Result<[f64; 3]> {
    if bridge {
        let entity = world.bridge_command(&format!("re_mcp:entity {}", quoted(id)?))?;
        let feet = point_of(&entity["location"])
            .ok_or_else(|| Error::msg(format!("Entity {id} has no location.")))?;
        let eye = point_of(&entity["eye"]).unwrap_or(feet);
        return Ok([0, 1, 2].map(|axis| (feet[axis] + eye[axis]) / 2.0));
    }
    world
        .targets("@e")?
        .into_iter()
        .find(|target| target.unique_id == id)
        .map(|target| target.position)
        .ok_or_else(|| Error::msg(format!("No entity with id {id} matched querytarget @e.")))
}

/// What to look at.
#[derive(Clone, Debug, PartialEq)]
pub enum Aim {
    Position([f64; 3]),
    Entity(String),
    Block([i64; 3], Option<Face>),
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UseMode {
    #[default]
    Instant,
    Hold,
    Release,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Hand {
    #[default]
    Mainhand,
    Offhand,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Whole {
    Stack,
}

/// How many items to drop: a count, or the whole selected stack.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum Quantity {
    Count(u32),
    Whole(Whole),
}

impl Default for Quantity {
    fn default() -> Self {
        Self::Count(1)
    }
}

/// Where to walk.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Destination {
    Position(Vec3),
    Entity(String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemUse {
    #[serde(default)]
    actor: Actor,
    #[serde(default)]
    mode: UseMode,
    ms: Option<u64>,
    ticks: Option<u64>,
    #[serde(default)]
    hand: Hand,
}

impl ItemUse {
    /// How long a hold lasts, in milliseconds; `None` keeps the item in use.
    fn hold_ms(&self) -> Result<Option<u64>> {
        let held = match (self.ms, self.ticks) {
            (Some(_), Some(_)) => bail!("Give ms or ticks, not both."),
            (Some(ms), None) => Some(ms),
            (None, Some(ticks)) => Some(ticks.saturating_mul(TICK.as_millis() as u64)),
            (None, None) => None,
        };
        if held.is_some() && self.mode != UseMode::Hold {
            bail!("ms and ticks only apply to mode hold.");
        }
        if held.is_some_and(|ms| ms > MAX_WAIT_MS) {
            bail!("A hold lasts at most {MAX_WAIT_MS} ms.");
        }
        Ok(held)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemUseOn {
    #[serde(default)]
    actor: Actor,
    location: Vec3,
    face: Option<Face>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityTarget {
    #[serde(default)]
    actor: Actor,
    entity_id: Option<String>,
    /// Walk toward the entity first when it is beyond survival reach.
    #[serde(default)]
    approach: bool,
    approach_ms: Option<u64>,
}

impl EntityTarget {
    /// How long to walk toward the entity when it is beyond reach; `None` without approach.
    fn approach(&self) -> Result<Option<Duration>> {
        if !self.approach {
            if self.approach_ms.is_some() {
                bail!("approach_ms only applies with approach: true.");
            }
            return Ok(None);
        }
        if self.entity_id.is_none() {
            bail!("approach needs entity_id: the entity to walk toward.");
        }
        let ms = self.approach_ms.unwrap_or(DEFAULT_APPROACH_MS);
        if ms > MAX_WAIT_MS {
            bail!("approach_ms is at most {MAX_WAIT_MS}.");
        }
        Ok(Some(Duration::from_millis(ms)))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockBreak {
    #[serde(default)]
    actor: Actor,
    location: Option<Vec3>,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockPlace {
    #[serde(default)]
    actor: Actor,
    location: Vec3,
    face: Face,
    slot: Option<u8>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemDrop {
    #[serde(default)]
    actor: Actor,
    #[serde(default)]
    quantity: Quantity,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Movement {
    #[serde(default)]
    actor: Actor,
    to: Option<Destination>,
    sprint: Option<bool>,
    jump: Option<bool>,
    arrival_radius: Option<f64>,
    timeout_ms: Option<u64>,
    forward: Option<f64>,
    strafe: Option<f64>,
    sneak: Option<bool>,
    duration_ms: Option<u64>,
    avoid_water: Option<bool>,
}

impl Movement {
    fn check(&self) -> Result<()> {
        let manual = self.forward.is_some()
            || self.strafe.is_some()
            || self.sneak.is_some()
            || self.duration_ms.is_some();
        if self.to.is_some() && manual {
            bail!("Give to, or the manual fields (forward, strafe, sneak, duration_ms), not both.");
        }
        if self.to.is_none()
            && (self.arrival_radius.is_some()
                || self.timeout_ms.is_some()
                || self.avoid_water.is_some())
        {
            bail!("arrival_radius, timeout_ms and avoid_water only apply with to.");
        }
        if self.to.is_none() && !manual && self.sprint.is_none() && self.jump.is_none() {
            bail!(
                "movement_set needs to, or at least one of forward, strafe, jump, sneak and sprint."
            );
        }
        if self.arrival_radius.is_some_and(|radius| radius <= 0.0) {
            bail!("arrival_radius must be above 0.");
        }
        if self.timeout_ms.unwrap_or(0) > MAX_WAIT_MS || self.duration_ms.unwrap_or(0) > MAX_WAIT_MS
        {
            bail!("timeout_ms and duration_ms are at most {MAX_WAIT_MS}.");
        }
        Ok(())
    }

    /// Whether the manual fields ask the player to change position.
    fn moves(&self) -> bool {
        self.forward.is_some_and(|value| value != 0.0)
            || self.strafe.is_some_and(|value| value != 0.0)
            || self.jump == Some(true)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Look {
    #[serde(default)]
    actor: Actor,
    position: Option<Vec3>,
    entity_id: Option<String>,
    block: Option<Vec3>,
    face: Option<Face>,
}

impl Look {
    fn aim(&self) -> Result<Aim> {
        match (&self.position, &self.entity_id, &self.block) {
            (Some(position), None, None) => Ok(Aim::Position([position.x, position.y, position.z])),
            (None, Some(id), None) => Ok(Aim::Entity(id.clone())),
            (None, None, Some(at)) => Ok(Aim::Block(block(*at), self.face)),
            _ => bail!("look needs exactly one of position, entity_id and block."),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActorOnly {
    #[serde(default)]
    actor: Actor,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HotbarSelect {
    #[serde(default)]
    actor: Actor,
    slot: u8,
}

/// One action with its arguments.
#[derive(Debug)]
pub enum Request {
    ItemUse(ItemUse),
    ItemUseOn(ItemUseOn),
    Attack(EntityTarget),
    Interact(EntityTarget),
    BlockBreak(BlockBreak),
    BlockPlace(BlockPlace),
    ItemDrop(ItemDrop),
    Movement(Movement),
    Fight(fight::Fight),
    Look(Look),
    Hotbar(HotbarSelect),
    Spawn(spawn::Spawn),
    Stop(ActorOnly),
}

fn parsed<T: DeserializeOwned>(action: &str, args: Value) -> Result<T> {
    serde_json::from_value(args)
        .map_err(|error| Error::msg(format!("Invalid arguments for {action}: {error}")))
}

impl Request {
    pub fn parse(action: &str, args: Value) -> Result<Self> {
        let args = if args.is_null() { json!({}) } else { args };
        Ok(match action {
            "item_use" => Self::ItemUse(parsed(action, args)?),
            "item_use_on" => Self::ItemUseOn(parsed(action, args)?),
            "attack" => Self::Attack(parsed(action, args)?),
            "entity_interact" => Self::Interact(parsed(action, args)?),
            "block_break" => Self::BlockBreak(parsed(action, args)?),
            "block_place" => Self::BlockPlace(parsed(action, args)?),
            "item_drop" => Self::ItemDrop(parsed(action, args)?),
            "movement_set" => Self::Movement(parsed(action, args)?),
            "fight" => Self::Fight(parsed(action, args)?),
            "look" => Self::Look(parsed(action, args)?),
            "hotbar_select" => Self::Hotbar(parsed(action, args)?),
            "spawn" => Self::Spawn(parsed(action, args)?),
            "stop" => Self::Stop(parsed(action, args)?),
            other => bail!(
                "Unknown action {other:?}; use one of {}.",
                ACTIONS.join(", ")
            ),
        })
    }

    fn actor(&self) -> &Actor {
        match self {
            Self::ItemUse(args) => &args.actor,
            Self::ItemUseOn(args) => &args.actor,
            Self::Attack(args) | Self::Interact(args) => &args.actor,
            Self::BlockBreak(args) => &args.actor,
            Self::BlockPlace(args) => &args.actor,
            Self::ItemDrop(args) => &args.actor,
            Self::Movement(args) => &args.actor,
            Self::Fight(args) => &args.actor,
            Self::Look(args) => &args.actor,
            Self::Hotbar(args) => &args.actor,
            Self::Spawn(args) => &args.actor,
            Self::Stop(args) => &args.actor,
        }
    }

    /// The entity the action aims at, walks to or summons, by id.
    fn target(&self) -> Option<&str> {
        match self {
            Self::Attack(args) | Self::Interact(args) => args.entity_id.as_deref(),
            Self::Look(args) => args.entity_id.as_deref(),
            Self::Movement(Movement {
                to: Some(Destination::Entity(id)),
                ..
            }) => Some(id),
            Self::Fight(args) => args.target.as_deref().filter(|target| {
                matches!(fight::Wanted::parse(Some(target)), Ok(fight::Wanted::Id(_)))
            }),
            _ => None,
        }
    }

    /// The connected player's controls this action needs free; see [`Controls`].
    fn controls(&self) -> Controls {
        let walks = |args: &EntityTarget| {
            if args.approach {
                Controls::MOVE
            } else {
                Controls::NONE
            }
        };
        match self {
            Self::Look(_) => Controls::VIEW,
            Self::Attack(args) => Controls::VIEW | Controls::ATTACK | walks(args),
            Self::Interact(args) => Controls::VIEW | Controls::USE | walks(args),
            Self::ItemUse(args) if args.mode == UseMode::Release => Controls::USE,
            Self::ItemUse(_) | Self::ItemUseOn(_) | Self::BlockPlace(_) => {
                Controls::VIEW | Controls::USE
            }
            Self::BlockBreak(_) => Controls::VIEW | Controls::ATTACK,
            Self::Movement(args) if args.to.is_some() => Controls::VIEW | Controls::MOVE,
            Self::Movement(_) => Controls::MOVE,
            Self::Fight(_) => Controls::VIEW | Controls::MOVE | Controls::ATTACK,
            // A slot change ends a held use, so it waits for one.
            Self::Hotbar(_) => Controls::USE,
            Self::ItemDrop(_) | Self::Spawn(_) | Self::Stop(_) => Controls::NONE,
        }
    }

    /// How many ticks early a sequence starts this actor self step so it lands on its tick:
    /// [`AIM_LEAD`] when it aims before it clicks, [`TURN_LEAD`] for a look (one write).
    fn lead(&self) -> u64 {
        if self.aims() {
            AIM_LEAD
        } else if matches!(self, Self::Look(_)) {
            TURN_LEAD
        } else {
            0
        }
    }

    /// Whether the input driver turns the player toward a target before it clicks; the
    /// click then waits for its due tick after aiming.
    fn aims(&self) -> bool {
        match self {
            Self::Attack(args) | Self::Interact(args) => args.entity_id.is_some(),
            Self::ItemUseOn(_) | Self::BlockPlace(_) => true,
            Self::BlockBreak(args) => args.location.is_some(),
            _ => false,
        }
    }
}

/// Gameplay actions, one method each. Each returns what the game showed of the result.
pub trait Driver {
    fn look(&self, game: &mut dyn Game, aim: &Aim) -> Result<Outcome>;
    /// Walk to a destination, or press movement keys (`movement.to` unset).
    fn move_to(&self, game: &mut dyn Game, movement: &Movement) -> Result<Outcome>;
    fn select_slot(&self, game: &mut dyn Game, slot: u8) -> Result<Outcome>;
    /// Use the held item; `hold_ms` bounds a hold, `None` keeps it in use until a release.
    fn use_item(
        &self,
        game: &mut dyn Game,
        mode: UseMode,
        hold_ms: Option<u64>,
        hand: Hand,
    ) -> Result<Outcome>;
    fn use_on(&self, game: &mut dyn Game, at: [i64; 3], face: Option<Face>) -> Result<Outcome>;
    /// Attack an entity by id, or whatever is under the crosshair. `approach` walks toward
    /// the entity for up to that long when it is beyond reach.
    fn attack(
        &self,
        game: &mut dyn Game,
        entity: Option<&str>,
        approach: Option<Duration>,
    ) -> Result<Outcome>;
    /// Interact with an entity by id, or whatever is under the crosshair; `approach` as for
    /// [`Driver::attack`].
    fn interact(
        &self,
        game: &mut dyn Game,
        entity: Option<&str>,
        approach: Option<Duration>,
    ) -> Result<Outcome>;
    /// Break a block, or the one under the crosshair, giving up after `timeout`.
    fn break_block(
        &self,
        game: &mut dyn Game,
        at: Option<[i64; 3]>,
        timeout: Duration,
    ) -> Result<Outcome>;
    /// Place the selected (or `slot`) item against `face` of the block at `at`.
    fn place(
        &self,
        game: &mut dyn Game,
        at: [i64; 3],
        face: Face,
        slot: Option<u8>,
    ) -> Result<Outcome>;
    fn drop_item(&self, game: &mut dyn Game, quantity: Quantity) -> Result<Outcome>;
    /// Release held keys and stop moving, using and breaking.
    fn stop(&self, game: &mut dyn Game) -> Result<Outcome>;
    /// Fight the nearest matching mobs; see [`fight`].
    fn fight(&self, game: &mut dyn Game, args: &fight::Fight) -> Result<Outcome>;
}

/// When and how a step's action starts: `defer` hands back actions that still run as
/// [`Outcome::pending`]; `at` is the game tick (with about how long until it) a GameTest
/// action is sent ahead for; `tick` is the game tick an input action's click is due on (read
/// just before it starts, or later when it aims first and waits for it); `presses` are the
/// sequence's inputs so far; `cursor` a recent end of the event stream.
#[derive(Clone, Default)]
struct Start {
    defer: bool,
    at: Option<(u64, Duration)>,
    tick: Option<u64>,
    presses: Presses,
    cursor: Option<EventCursor>,
}

/// The driver for an actor: input for the connected player, GameTest for a named one.
fn driver(actor: &Actor, confirm: Duration, start: Start) -> Box<dyn Driver> {
    match actor {
        Actor::Me => Box::new(InputDriver {
            confirm,
            defer: start.defer,
            tick: start.tick,
            presses: start.presses,
            cursor: start.cursor,
        }),
        Actor::Named(name) => Box::new(GameTestDriver {
            name: name.clone(),
            confirm,
            defer: start.defer,
            at: start.at,
        }),
    }
}

/// Run one action; `confirm` bounds the wait for a confirming event.
pub fn perform(game: &mut dyn Game, request: &Request, confirm: Duration) -> Result<Outcome> {
    perform_with(game, request, confirm, Start::default())
}

/// [`perform`], started as `start` says.
fn perform_with(
    game: &mut dyn Game,
    request: &Request,
    confirm: Duration,
    start: Start,
) -> Result<Outcome> {
    let driver = driver(request.actor(), confirm, start);
    match request {
        Request::ItemUse(args) => driver.use_item(game, args.mode, args.hold_ms()?, args.hand),
        Request::ItemUseOn(args) => driver.use_on(game, block(args.location), args.face),
        Request::Attack(args) => driver.attack(game, args.entity_id.as_deref(), args.approach()?),
        Request::Interact(args) => {
            driver.interact(game, args.entity_id.as_deref(), args.approach()?)
        }
        Request::BlockBreak(args) => {
            let timeout = args.timeout_ms.unwrap_or(DEFAULT_BREAK_MS);
            if timeout > MAX_WAIT_MS {
                bail!("timeout_ms is at most {MAX_WAIT_MS}.");
            }
            driver.break_block(
                game,
                args.location.map(block),
                Duration::from_millis(timeout),
            )
        }
        Request::BlockPlace(args) => {
            if args.slot.is_some_and(|slot| slot > 8) {
                bail!("slot must be 0 to 8.");
            }
            driver.place(game, block(args.location), args.face, args.slot)
        }
        Request::ItemDrop(args) => {
            if let Quantity::Count(count) = args.quantity
                && !(1..=MAX_DROP).contains(&count)
            {
                bail!("quantity must be 1 to {MAX_DROP}, or \"stack\".");
            }
            driver.drop_item(game, args.quantity)
        }
        Request::Movement(args) => {
            args.check()?;
            driver.move_to(game, args)
        }
        Request::Fight(args) => driver.fight(game, args),
        Request::Look(args) => driver.look(game, &args.aim()?),
        Request::Hotbar(args) => driver.select_slot(game, args.slot),
        Request::Spawn(args) => spawn::spawn(game, args, confirm),
        Request::Stop(_) => driver.stop(game),
    }
}

/// Run the action a tool names with the tool's arguments; the outcome as JSON.
pub fn run(game: &mut dyn Game, action: &str, args: Value) -> Result<Value> {
    Ok(perform(game, &Request::parse(action, args)?, CONFIRM_TIMEOUT)?.to_json())
}

/// Turn simulated player `name` to `yaw` and `pitch` and confirm it by readback; `None` when
/// `name` is not a simulated player. A teleport's pitch does not last on a simulated player,
/// so this uses a continuous GameTest look, which the game keeps until the next look.
pub fn look_simulated(
    game: &mut dyn Game,
    name: &str,
    yaw: f64,
    pitch: f64,
) -> Result<Option<Outcome>> {
    gametest::look_toward(game, name, CONFIRM_TIMEOUT, yaw, pitch)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    action: String,
    #[serde(default)]
    args: Value,
    at_tick: Option<u64>,
    after_ms: Option<u64>,
    wait_for: Option<Check>,
    wait_ms: Option<u64>,
    #[serde(rename = "if")]
    only_if: Option<Check>,
    repeat: Option<Repeat>,
    expect: Option<StepExpect>,
    /// A name later steps use as `$name` for the entity a spawn step summons.
    #[serde(rename = "as")]
    label: Option<String>,
}

/// What a step's outcome must show once its confirmation settles; a step that does not
/// show it counts as a failure.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepExpect {
    /// The game confirmed the action.
    confirmed: Option<bool>,
    /// A condition that holds then; its events count from when the action started.
    holds: Option<Check>,
}

impl StepExpect {
    fn validate(&self) -> Result<()> {
        match (self.confirmed, &self.holds) {
            (None, None) => bail!("expect needs confirmed, holds or both."),
            (Some(false), _) => bail!("expect confirmed can only be true."),
            (_, Some(check)) => check.validate(),
            _ => Ok(()),
        }
    }

    /// Whether `outcome` shows what was expected, with what showed it.
    fn check(
        &self,
        outcome: &Outcome,
        since: &EventCursor,
        game: &mut dyn Game,
        probe: &Probe,
    ) -> Value {
        let mut met = true;
        let mut report = json!({});
        if self.confirmed.is_some() {
            met &= outcome.confirmed;
            report["confirmed"] = outcome.confirmed.into();
        }
        if let Some(check) = &self.holds {
            match Watch::new(check.clone(), since.clone()).holds(game, probe) {
                Ok(detail) => {
                    met &= detail.is_some();
                    report["holds"] = detail.unwrap_or(Value::Null);
                }
                Err(error) => {
                    met = false;
                    report["error"] = format!("expect holds could not be checked: {error}").into();
                }
            }
        }
        report["met"] = met.into();
        report
    }
}

/// Run a step's action again and again.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Repeat {
    times: Option<u32>,
    until: Option<Check>,
    every_ticks: Option<u64>,
    every_ms: Option<u64>,
}

impl Repeat {
    /// The runs, `every` in the sequence's clock units: ticks with `by_tick`, else ms.
    fn plan(self, by_tick: bool) -> Result<Runs> {
        if self.times.is_none() && self.until.is_none() {
            bail!("repeat needs times, until or both.");
        }
        if self
            .times
            .is_some_and(|times| !(1..=MAX_RUNS).contains(&times))
        {
            bail!("repeat times must be 1 to {MAX_RUNS}.");
        }
        let every = match (self.every_ticks, self.every_ms) {
            (Some(_), Some(_)) => bail!("Give repeat every_ticks or every_ms, not both."),
            (Some(every), None) | (None, Some(every)) => every,
            (None, None) if by_tick => 1,
            (None, None) => TICK.as_millis() as u64,
        };
        if every == 0 {
            bail!("repeat every_ticks and every_ms must be at least 1.");
        }
        if let Some(until) = &self.until {
            until.validate()?;
        }
        Ok(Runs {
            times: self.times.unwrap_or(MAX_RUNS),
            every,
            until: self.until,
        })
    }
}

/// How a repeated step runs: at most `times` runs, `every` clock units apart, until `until`
/// holds.
struct Runs {
    times: u32,
    every: u64,
    until: Option<Check>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SequenceArgs {
    actor: Option<String>,
    steps: Vec<Step>,
    timeout_ms: Option<u64>,
    stop_when: Option<Check>,
    /// Return at once and run the steps while other tool calls are served.
    #[serde(default)]
    pub background: bool,
    #[serde(default)]
    fail_fast: bool,
    max_failures: Option<u32>,
    /// Record the screen from the first step to the last: true, or video_start's arguments.
    record: Option<Value>,
    /// How long a blocking call waits before it answers with the run still running.
    blocking_limit_ms: Option<u64>,
    /// Warm up before the steps: true, false or "auto" (default).
    warm_up: Option<Value>,
    /// Reply with every step's whole entry, not only the steps that went wrong.
    #[serde(default)]
    pub verbose: bool,
}

/// How long a blocking action_sequence waits for its run by default.
const DEFAULT_BLOCKING_LIMIT_MS: u64 = 25_000;

impl SequenceArgs {
    /// How long a blocking call waits for its run; `None` waits to the end.
    pub fn blocking_limit(&self) -> Option<Duration> {
        match self.blocking_limit_ms.unwrap_or(DEFAULT_BLOCKING_LIMIT_MS) {
            0 => None,
            ms => Some(Duration::from_millis(ms)),
        }
    }

    /// video_start's arguments when the run records.
    fn recording(&self) -> Result<Option<Value>> {
        Ok(match &self.record {
            None | Some(Value::Bool(false)) => None,
            Some(Value::Bool(true)) => Some(json!({})),
            Some(fields @ Value::Object(_)) => Some(fields.clone()),
            Some(other) => {
                bail!("record must be true, false or video_start's arguments, not {other}.")
            }
        })
    }
}

/// A parsed step and when it runs: ticks or milliseconds after the sequence starts.
struct Planned {
    index: usize,
    action: String,
    at: u64,
    request: Request,
    /// The arguments as given, to parse again once the entities they name exist.
    args: Value,
    /// Whether the step names entities that spawn steps summon (`$name`).
    refs: bool,
    /// The step's `as` name.
    label: Option<String>,
    /// The condition to wait for, and how long.
    wait_for: Option<(Check, Duration)>,
    only_if: Option<Check>,
    repeat: Option<Runs>,
    expect: Option<StepExpect>,
    /// How many ticks before `at` the step starts, with the tick clock.
    lead: u64,
}

impl Planned {
    fn checks(&self) -> impl Iterator<Item = &Check> {
        let wait_for = self.wait_for.as_ref().map(|(check, _)| check);
        let until = self.repeat.as_ref().and_then(|runs| runs.until.as_ref());
        let expect = self
            .expect
            .as_ref()
            .and_then(|expect| expect.holds.as_ref());
        wait_for
            .into_iter()
            .chain(&self.only_if)
            .chain(until)
            .chain(expect)
    }

    /// Whether the step waits on the game, so it cannot be sent ahead of its tick.
    fn conditional(&self) -> bool {
        self.wait_for.is_some() || self.only_if.is_some() || self.repeat.is_some()
    }

    /// Every `$name` reference in the step's arguments and conditions.
    fn references(&self) -> Vec<String> {
        let mut found = Vec::new();
        refs::in_value(&self.args, &mut found);
        for check in self.checks() {
            refs::in_check(check, &mut found);
        }
        found
    }

    /// Put the ids of the spawned entities in place of the step's references.
    fn resolve(&mut self, ids: &HashMap<String, String>) -> Result<()> {
        if !self.refs {
            return Ok(());
        }
        let mut args = self.args.clone();
        refs::substitute(&mut args, ids)?;
        self.request = Request::parse(&self.action, args)?;
        let until = self.repeat.as_mut().and_then(|runs| runs.until.as_mut());
        let holds = self
            .expect
            .as_mut()
            .and_then(|expect| expect.holds.as_mut());
        let wait_for = self.wait_for.as_mut().map(|(check, _)| check);
        for check in wait_for
            .into_iter()
            .chain(self.only_if.as_mut())
            .chain(until)
            .chain(holds)
        {
            refs::substitute_check(check, ids)?;
        }
        Ok(())
    }
}

/// Check and plan a sequence to run in the background; see [`Background::queue`].
pub fn prepare_sequence(game: &mut dyn Game, args: SequenceArgs) -> Result<Prepared> {
    prepare(game, args, CONFIRM_TIMEOUT)
}

fn current_tick(world: &World) -> Result<u64> {
    world.bridge_command("re_mcp:ping")?["tick"]
        .as_u64()
        .ok_or_else(|| Error::msg("re_mcp:ping answered without a tick."))
}

/// Where an action's outcome goes: its step's entry, or one run of a repeated step.
#[derive(Clone, Copy)]
struct Slot {
    entry: usize,
    run: Option<usize>,
}

/// An action that still runs: where its outcome goes, and the end of the event stream when
/// it started, which its step's expect counts events from.
struct Started {
    slot: Slot,
    pending: Box<dyn Pending>,
    since: EventCursor,
}

/// Sequence steps that started and still run, every step's entry, and the failures so far.
struct Running {
    steps: Vec<Started>,
    /// The running step to check next.
    turn: usize,
    entries: Vec<Value>,
    /// Each entry's expect.
    expects: Vec<Option<StepExpect>>,
    /// The game tick of the sequence's tick 0, with the tick clock.
    start: Option<u64>,
    /// Actions and steps that failed: an error, an unconfirmed outcome or an unmet expect.
    failures: u32,
    /// The index of the last step that failed.
    failed_step: Value,
    /// Where a background run publishes settled results.
    log: Option<Rc<RefCell<RunLog>>>,
    /// Which entries the log has in full.
    published: Vec<bool>,
}

impl Running {
    fn new(start: Option<u64>, log: Option<Rc<RefCell<RunLog>>>) -> Self {
        Self {
            steps: Vec::new(),
            turn: 0,
            entries: Vec::new(),
            expects: Vec::new(),
            start,
            failures: 0,
            failed_step: Value::Null,
            log,
            published: Vec::new(),
        }
    }

    /// Add a step's entry; its index.
    fn push(&mut self, entry: Value, expect: Option<StepExpect>) -> usize {
        self.entries.push(entry);
        self.expects.push(expect);
        self.published.push(false);
        self.entries.len() - 1
    }

    /// Add the entry of a step that will not act: skipped, or failed before acting.
    fn settle(&mut self, entry: Value) {
        let failed = entry.get("error").is_some();
        let index = self.push(entry, None);
        if failed {
            self.fail(index);
        }
        self.publish(index);
    }

    fn slot(&mut self, slot: Slot) -> &mut Value {
        let entry = &mut self.entries[slot.entry];
        match slot.run {
            Some(run) => &mut entry["runs"][run],
            None => entry,
        }
    }

    /// Count a failure of entry `entry`.
    fn fail(&mut self, entry: usize) {
        self.failures += 1;
        self.failed_step = self.entries[entry]["index"].clone();
        if let Some(log) = &self.log {
            log.borrow_mut().failures = self.failures;
        }
    }

    /// Hand entry `entry` to the log once, when nothing more will change in it.
    fn publish(&mut self, entry: usize) {
        if std::mem::replace(&mut self.published[entry], true) {
            return;
        }
        if let Some(log) = &self.log {
            log.borrow_mut().results.push(self.entries[entry].clone());
        }
        self.sync();
    }

    /// Hand a settled action to the log: its step's entry, or for a run of a repeated step
    /// the entry with only that run, then the whole entry once the repeat ended and its last
    /// run settled.
    fn publish_slot(&mut self, slot: Slot) {
        let Some(run) = slot.run else {
            return self.publish(slot.entry);
        };
        if let Some(log) = &self.log {
            let entry = &self.entries[slot.entry];
            let mut partial = json!({ "index": entry["index"], "action": entry["action"] });
            for clock in ["at_tick", "after_ms"] {
                if let Some(at) = entry.get(clock) {
                    partial[clock] = at.clone();
                }
            }
            partial["runs"] = json!([entry["runs"][run]]);
            log.borrow_mut().results.push(partial);
        }
        self.settle_repeat(slot.entry);
    }

    /// Publish a repeated step's entry once its repeat ended and no run of it still runs.
    fn settle_repeat(&mut self, entry: usize) {
        if self.entries[entry].get("ended").is_some()
            && !self.steps.iter().any(|started| started.slot.entry == entry)
        {
            self.publish(entry);
        } else {
            self.sync();
        }
    }

    /// Tell the log which action runs now: the one that started last.
    fn sync(&self) {
        let Some(log) = &self.log else {
            return;
        };
        log.borrow_mut().current = self.steps.last().map_or(Value::Null, |started| {
            let entry = &self.entries[started.slot.entry];
            let action = match started.slot.run {
                Some(run) => &entry["runs"][run],
                None => entry,
            };
            let mut current = json!({
                "index": entry["index"],
                "started_tick": action.get("started_tick").or(entry.get("started_tick")),
                "started_ms": action.get("started_ms").or(entry.get("started_ms")),
            });
            if let Some(run) = started.slot.run {
                current["run"] = run.into();
            }
            current
        });
    }

    /// The connected player's controls that running actions still hold.
    fn holding(&self) -> Controls {
        self.steps.iter().fold(Controls::NONE, |held, started| {
            held | started.pending.holds()
        })
    }

    /// Record a finished action, check its step's expect and count a failure; with the tick
    /// clock, its started_tick is the tick its input went out on or its first GameTest
    /// method call ran on.
    fn record(
        &mut self,
        slot: Slot,
        result: Result<Outcome>,
        since: &EventCursor,
        game: &mut dyn Game,
        probe: &Probe,
    ) {
        let expected = match (&result, &self.expects[slot.entry]) {
            (Ok(outcome), Some(expect)) => Some(expect.check(outcome, since, game, probe)),
            _ => None,
        };
        let start = self.start;
        let entry = self.slot(slot);
        let failed = match result {
            Ok(outcome) => {
                if let (Some(start), Some(acted)) = (start, outcome.evidence["acted_tick"].as_u64())
                {
                    entry["started_tick"] = acted.saturating_sub(start).into();
                }
                entry["outcome"] = outcome.to_json();
                let mut failed = !outcome.confirmed;
                if let Some(expected) = expected {
                    failed |= expected["met"] == false;
                    entry["expect"] = expected;
                }
                failed
            }
            Err(error) if gone(&error) => {
                entry["skipped"] =
                    "Its target died or went away before it acted, so it did not run.".into();
                entry["reason"] = json!({ "class": TARGET_DEAD, "detail": error.to_string() });
                false
            }
            Err(error) => {
                entry["error"] = error.to_string().into();
                true
            }
        };
        if failed {
            self.fail(slot.entry);
        }
        self.publish_slot(slot);
    }

    /// Check one running action, taking turns, and record it when it finished.
    fn service(&mut self, game: &mut dyn Game, probe: &Probe) {
        if self.steps.is_empty() {
            return;
        }
        let index = self.turn % self.steps.len();
        let finished = match self.steps[index].pending.poll(game) {
            Ok(None) => None,
            Ok(Some(outcome)) => Some(Ok(outcome)),
            Err(error) => Some(Err(error)),
        };
        match finished {
            None => self.turn = index + 1,
            Some(result) => {
                let started = self.steps.remove(index);
                self.turn = index;
                self.record(started.slot, result, &started.since, game, probe);
            }
        }
    }
}

/// When a sequence's steps are due.
#[derive(Clone, Copy)]
enum Clock {
    /// Game ticks after `start`.
    Tick { start: u64 },
    /// Wall time after `start`, `unit` per step time unit.
    Wall { start: Instant, unit: Duration },
}

/// How a wait for a step's time ended.
enum Due {
    /// The time came; the game tick it came on, with the tick clock.
    Now(Option<u64>),
    /// The sequence's timeout passed first.
    Expired,
    /// The sequence stopped first: stop_when, sequence_stop, failures or a disconnect.
    Stopped,
}

/// How a wait for a condition ended.
enum Held {
    /// It held; what showed it.
    Yes(Value),
    /// Its wait ran out first.
    Expired,
    /// The sequence stopped first.
    Stopped,
}

const STOPPED_SKIP: &str = "stop_when held before this step started.";

/// The reason class of an action whose target died or went away before it acted.
const TARGET_DEAD: &str = "target_dead";

/// Whether an action failed because the entity it names is no longer loaded.
fn gone(error: &Error) -> bool {
    let text = error.to_string();
    text.contains("No loaded entity with id") || text.contains("No entity with id")
}

/// A sequence while it runs: its clock, the actions that still run, and stop_when.
struct Run {
    world: World,
    bridge: bool,
    /// The actor for conditions that do not name one.
    actor: Actor,
    clock: Clock,
    started: Instant,
    deadline: Instant,
    running: Running,
    stop_when: Option<Watch>,
    /// What showed stop_when holding, or why it could not be checked, once either happened.
    stopped: Option<Value>,
    /// When stop_when was last checked.
    checked: Option<Instant>,
    /// The connected player's inputs so far.
    presses: Presses,
    /// With the tick clock, an end of the event stream read while the sequence had time, so
    /// an input action need not read one before it acts.
    cursor: Option<EventCursor>,
    /// How long checking running actions and stop_when took lately.
    idle_cost: Duration,
    /// What stopped the sequence: stop_when, sequence_stop, disconnect, fail_fast or
    /// max_failures.
    cause: &'static str,
    /// How many failures stop the sequence, and the argument that set it.
    limit: Option<(u32, &'static str)>,
    /// Without the Bridge pack: reads the targets' health natively and reports hurts and
    /// deaths as `native` events.
    native: Option<native::Tracker>,
}

impl Run {
    fn probe(&self) -> Probe<'_> {
        Probe {
            world: &self.world,
            bridge: self.bridge,
            actor: &self.actor,
        }
    }

    /// Without the Bridge pack, read the tracked targets' health so hurts and deaths reach
    /// the event stream before conditions read it (at most once a tick).
    fn observe(&mut self, game: &mut dyn Game) {
        if let Some(tracker) = self.native.as_mut() {
            tracker.poll(game, false);
        }
    }

    /// Without the Bridge pack, start reading entity `id`'s health.
    fn track(&mut self, game: &mut dyn Game, id: &str) {
        if let Some(tracker) = self.native.as_mut() {
            tracker.track(game, id);
        }
    }

    fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Start the clock and the timeout again from now, with tick 0 `ahead` ticks away.
    fn restart(&mut self, ahead: u64, timeout: Duration) -> Result<()> {
        self.started = Instant::now();
        self.deadline = self.started + timeout;
        self.clock = match self.clock {
            Clock::Tick { .. } => {
                let start = current_tick(&self.world)? + ahead;
                self.running.start = Some(start);
                Clock::Tick { start }
            }
            Clock::Wall { unit, .. } => Clock::Wall {
                start: self.started,
                unit,
            },
        };
        Ok(())
    }

    /// Check one running action.
    fn service(&mut self, game: &mut dyn Game) {
        let probe = Probe {
            world: &self.world,
            bridge: self.bridge,
            actor: &self.actor,
        };
        self.running.service(game, &probe);
    }

    /// Why a step did not start once the sequence stopped.
    fn skip_note(&self) -> String {
        match self.cause {
            "stop_when" => STOPPED_SKIP.to_owned(),
            "disconnect" => "The MCP client disconnected before this step started.".to_owned(),
            "take_over" => "The player took over with F9 before this step started.".to_owned(),
            cause => format!("{cause} ended the sequence before this step started."),
        }
    }

    /// What happened to an action that still ran when the sequence stopped.
    fn interrupt_note(&self) -> String {
        match self.cause {
            "disconnect" => {
                "The MCP client disconnected while this ran; its actor got the stop action."
                    .to_owned()
            }
            "take_over" => {
                "The player took over with F9 while this ran; its actor got the stop action."
                    .to_owned()
            }
            cause => {
                format!("{cause} ended the sequence while this ran; its actor got the stop action.")
            }
        }
    }

    /// Whether fail_fast or max_failures ends the sequence now; records why.
    fn over_limit(&mut self) -> bool {
        let Some((limit, by)) = self.limit else {
            return false;
        };
        if self.running.failures < limit {
            return false;
        }
        self.cause = by;
        self.stopped = Some(json!({
            "at_ms": self.elapsed_ms(),
            "by": by,
            "failures": self.running.failures,
            "step": self.running.failed_step,
        }));
        true
    }

    /// Whether the sequence must stop: sequence_stop asked for it, the player took over, the
    /// client went away, enough steps failed, or stop_when holds (checked at most once a tick
    /// unless `now`). A condition that cannot be checked stops the sequence too.
    fn stopping(&mut self, game: &mut dyn Game, now: bool) -> bool {
        if self.stopped.is_some() {
            return true;
        }
        self.observe(game);
        let at_ms = self.elapsed_ms();
        let requested = match &self.running.log {
            _ if game.abandoned() => Some("disconnect"),
            Some(log) if log.borrow().taken_over => Some("take_over"),
            Some(log) if log.borrow().stop_requested => Some("sequence_stop"),
            _ => None,
        };
        if let Some(by) = requested {
            self.cause = by;
            self.stopped = Some(json!({ "at_ms": at_ms, "by": by }));
            return true;
        }
        if self.over_limit() {
            return true;
        }
        let Some(watch) = self.stop_when.as_mut() else {
            return false;
        };
        if !now && self.checked.is_some_and(|at| at.elapsed() < TICK) {
            return false;
        }
        self.checked = Some(Instant::now());
        let probe = Probe {
            world: &self.world,
            bridge: self.bridge,
            actor: &self.actor,
        };
        self.stopped = match watch.holds(game, &probe) {
            Ok(None) => None,
            Ok(Some(detail)) => Some(json!({ "at_ms": at_ms, "detail": detail })),
            Err(error) => Some(json!({
                "at_ms": at_ms,
                "error": format!("stop_when could not be checked: {error}"),
            })),
        };
        self.stopped.is_some()
    }

    /// The work done while waiting: check one running action, read a fresh event cursor and
    /// check stop_when. Whether the sequence must stop. Remembers how long it took, so waits
    /// only do it while there is that much slack.
    fn idle(&mut self, game: &mut dyn Game) -> bool {
        let began = Instant::now();
        self.service(game);
        if self.cursor.is_some()
            && let Ok(cursor) = game.cursor()
        {
            self.cursor = Some(cursor);
        }
        let stop = self.stopping(game, false);
        self.idle_cost = began.elapsed().max(self.idle_cost * 3 / 4);
        stop
    }

    /// Whether `left` leaves room for [`Run::idle`] before a step is due.
    fn has_slack(&self, left: Duration) -> bool {
        left >= (TICK * SERVICE_SLACK as u32).max(self.idle_cost + TICK * 2)
    }

    /// Wait until `lead` ticks before clock time `at`, while running actions go on and
    /// stop_when is watched. Running actions and stop_when are only checked while there is
    /// slack, so checking them does not delay the step.
    fn wait_due(&mut self, game: &mut dyn Game, at: u64, lead: u64) -> Result<Due> {
        loop {
            match self.clock {
                Clock::Tick { start } => {
                    let now = current_tick(&self.world)?;
                    let target = (start + at).saturating_sub(lead);
                    if now >= target {
                        return Ok(Due::Now(Some(now)));
                    }
                    if Instant::now() >= self.deadline {
                        return Ok(Due::Expired);
                    }
                    let left = TICK * u32::try_from(target - now).unwrap_or(u32::MAX);
                    if self.has_slack(left) && self.idle(game) {
                        return Ok(Due::Stopped);
                    }
                }
                Clock::Wall { start, unit } => {
                    let due = start + unit.saturating_mul(u32::try_from(at).unwrap_or(u32::MAX));
                    if due > self.deadline {
                        return Ok(Due::Expired);
                    }
                    let left = due.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Ok(Due::Now(None));
                    }
                    if !self.has_slack(left) {
                        game.pause(left);
                        continue;
                    }
                    if self.idle(game) {
                        return Ok(Due::Stopped);
                    }
                }
            }
            game.pause(TICK_POLL);
        }
    }

    /// Check `watch` once a tick until it holds, `until` passes, or stop_when holds.
    fn wait_check(
        &mut self,
        game: &mut dyn Game,
        watch: &mut Watch,
        until: Instant,
    ) -> Result<Held> {
        loop {
            self.observe(game);
            let polled = Instant::now();
            if let Some(detail) = watch.holds(game, &self.probe())? {
                return Ok(Held::Yes(detail));
            }
            if self.stopping(game, false) {
                return Ok(Held::Stopped);
            }
            if Instant::now() >= until {
                return Ok(Held::Expired);
            }
            self.service(game);
            game.pause(TICK.saturating_sub(polled.elapsed()));
        }
    }

    /// Clock units since the start: ticks, or wall time in the clock's unit.
    fn position(&self) -> Result<u64> {
        Ok(match self.clock {
            Clock::Tick { start } => current_tick(&self.world)?.saturating_sub(start),
            Clock::Wall { start, unit } => {
                (start.elapsed().as_nanos() / unit.as_nanos().max(1)) as u64
            }
        })
    }

    /// Start an action and record its outcome, or keep it running.
    fn launch(
        &mut self,
        game: &mut dyn Game,
        request: &Request,
        confirm: Duration,
        start: Start,
        slot: Slot,
    ) {
        let counts_events = self.running.expects[slot.entry]
            .as_ref()
            .and_then(|expect| expect.holds.as_ref())
            .is_some_and(Check::reads_events);
        let since = match counts_events {
            true => game.cursor().unwrap_or_default(),
            false => EventCursor::default(),
        };
        match perform_with(game, request, confirm, start) {
            Ok(Outcome {
                pending: Some(pending),
                ..
            }) => {
                self.running.steps.push(Started {
                    slot,
                    pending,
                    since,
                });
                self.running.sync();
            }
            result => {
                let probe = Probe {
                    world: &self.world,
                    bridge: self.bridge,
                    actor: &self.actor,
                };
                self.running.record(slot, result, &since, game, &probe);
            }
        }
    }

    /// Start `request` as `slot`, due at clock time `at`; `now` is the game tick its wait
    /// ended on, with the tick clock. An actor self action first waits until no running
    /// action holds a control it needs (see [`Controls`]), then gets the tick its input is
    /// due on: the step's tick when it aims first (it waits for that tick after aiming),
    /// else the tick it starts on. Returns whether the sequence stopped while it waited.
    #[allow(clippy::too_many_arguments)]
    fn start_action(
        &mut self,
        game: &mut dyn Game,
        request: &Request,
        confirm: Duration,
        mut start: Start,
        slot: Slot,
        at: u64,
        mut now: Option<u64>,
    ) -> Result<bool> {
        start.defer = true;
        start.presses = self.presses.clone();
        start.cursor = self.cursor.clone();
        if *request.actor() == Actor::Me {
            let needs = request.controls();
            let mut waited = false;
            while self.running.holding().overlaps(needs) {
                if self.idle(game) {
                    return Ok(true);
                }
                game.pause(TICK_POLL);
                waited = true;
            }
            if let Clock::Tick { start: zero } = self.clock {
                if waited {
                    now = Some(current_tick(&self.world)?);
                }
                start.tick = Some(match now {
                    Some(now) if !request.aims() => now,
                    _ => zero + at,
                });
            }
        }
        self.launch(game, request, confirm, start, slot);
        Ok(false)
    }

    /// Run a repeated step's action `runs.every` clock units apart from clock time `first`
    /// (which the first run is due on and has waited for), until `runs.until` holds (checked
    /// before each run, with events since `since`), `runs.times` runs started, the timeout
    /// passed or stop_when held. Later runs start `lead` ticks early, as the step did. Each
    /// run goes in the entry's runs with the tick it was planned for, and `ended` says what
    /// ended them.
    #[allow(clippy::too_many_arguments)]
    fn repeat(
        &mut self,
        game: &mut dyn Game,
        request: &Request,
        runs: &Runs,
        entry: usize,
        since: EventCursor,
        confirm: Duration,
        first: u64,
        lead: u64,
        mut now: Option<u64>,
    ) -> Result<()> {
        let mut until = runs.until.clone().map(|check| Watch::new(check, since));
        let ticks = matches!(self.clock, Clock::Tick { .. });
        let mut count: u32 = 0;
        let ended = loop {
            if count >= runs.times {
                break "times";
            }
            let at = first + u64::from(count) * runs.every;
            // Wait for the run's time first, so until and stop_when are read right before it
            // starts, with the events that came during the wait.
            if count > 0 {
                match self.wait_due(game, at, lead)? {
                    Due::Now(tick) => now = tick,
                    Due::Expired => break "timeout",
                    Due::Stopped => break self.cause,
                }
                if Instant::now() > self.deadline {
                    break "timeout";
                }
            }
            self.observe(game);
            if let Some(watch) = until.as_mut() {
                match watch.holds(game, &self.probe()) {
                    Ok(Some(detail)) => {
                        self.running.entries[entry]["until"] = detail;
                        break "until";
                    }
                    Ok(None) => {}
                    Err(error) => {
                        self.running.entries[entry]["error"] =
                            format!("repeat until could not be checked: {error}").into();
                        self.running.fail(entry);
                        break "error";
                    }
                }
            }
            if self.stopping(game, true) {
                break self.cause;
            }
            let mut run = json!({ "run": count });
            if ticks {
                run["at_tick"] = at.into();
            }
            if let (Some(now), Clock::Tick { start }, 0) = (now, self.clock, lead) {
                run["started_tick"] = now.saturating_sub(start).into();
            }
            run["started_ms"] = self.elapsed_ms().into();
            if let Some(list) = self.running.entries[entry]["runs"].as_array_mut() {
                list.push(run);
            }
            let slot = Slot {
                entry,
                run: Some(count as usize),
            };
            if self.start_action(game, request, confirm, Start::default(), slot, at, now)? {
                break self.cause;
            }
            count += 1;
            // The target died or went away: later runs would find nothing to act on.
            if self.running.entries[entry]["runs"][count as usize - 1]["reason"]["class"]
                == TARGET_DEAD
            {
                break TARGET_DEAD;
            }
        };
        self.running.entries[entry]["ended"] = ended.into();
        self.running.settle_repeat(entry);
        Ok(())
    }
}

/// Run every step, ordered by its time: `at_tick` counts game ticks from the start (the
/// Bridge pack's `re_mcp:ping` tick; 50 ms each without it), `after_ms` milliseconds. A step
/// without a time runs right after the one before it. Simulated player steps start at their
/// time and finish (confirm, walk, mine, hold) while later steps start; with the tick clock
/// they are sent [`SEND_AHEAD`] ticks early and the GameTest pack runs them on their tick, and
/// tick 0 is that far after the call. Steps of actor self finish before the next one starts.
///
/// At its time a step first waits for `wait_for`, then runs only if `if` holds, then runs its
/// action once or as `repeat` says; these conditions count events from when the step before
/// it started. `stop_when` is checked once a tick throughout: once it holds, the steps not
/// started are skipped, running actions are left, and every actor that acted gets the stop
/// action. A failing step is reported and the rest still run, unless `fail_fast` or
/// `max_failures` says how many failures stop the sequence as stop_when does; an error, an
/// unconfirmed outcome and an unmet `expect` each count. Steps that would start after
/// `timeout_ms` are skipped.
#[cfg(test)]
fn sequence_with(game: &mut dyn Game, args: SequenceArgs, confirm: Duration) -> Result<Value> {
    let prepared = prepare(game, args, confirm)?;
    execute(game, prepared)
}

/// A sequence checked and planned, ready to run.
pub struct Prepared {
    run: Run,
    planned: Vec<Planned>,
    by_tick: bool,
    by_ms: bool,
    ticks: bool,
    reads_events: bool,
    /// Where the first step's conditions count events from.
    previous: EventCursor,
    notes: Vec<String>,
    confirm: Duration,
    /// video_start's arguments when the run records.
    record: Option<Value>,
    /// How many ticks after the call tick 0 is, with the tick clock.
    ahead: u64,
    timeout: Duration,
    /// Where the summary reads deaths and hits from: the Bridge pack's or native events.
    summary_from: EventCursor,
    /// Whether the connected player warms up before the steps; see [`warm`].
    warm: bool,
}

impl Prepared {
    /// Every actor the steps name.
    pub fn actors(&self) -> Vec<Actor> {
        let mut actors: Vec<Actor> = Vec::new();
        for step in &self.planned {
            if !actors.contains(step.request.actor()) {
                actors.push(step.request.actor().clone());
            }
        }
        actors
    }

    /// The clock the steps run by: "tick" or "wall".
    pub fn clock(&self) -> &'static str {
        if self.ticks { "tick" } else { "wall" }
    }

    /// Each step with when it is due, in step order.
    pub fn plan(&self) -> Vec<Value> {
        let mut plan: Vec<(usize, Value)> = self
            .planned
            .iter()
            .map(|step| {
                let mut entry = json!({ "index": step.index, "action": step.action });
                if self.by_tick {
                    entry["at_tick"] = step.at.into();
                } else if self.by_ms {
                    entry["after_ms"] = step.at.into();
                }
                (step.index, entry)
            })
            .collect();
        plan.sort_by_key(|(index, _)| *index);
        plan.into_iter().map(|(_, entry)| entry).collect()
    }

    /// Publish settled results to `log` while the sequence runs.
    pub fn attach(&mut self, log: Rc<RefCell<RunLog>>) {
        self.run.running.log = Some(log);
    }
}

/// Check and plan a sequence: everything [`sequence_with`] does before the first step.
fn prepare(game: &mut dyn Game, args: SequenceArgs, confirm: Duration) -> Result<Prepared> {
    if args.steps.is_empty() {
        bail!("action_sequence needs at least one step.");
    }
    if args.steps.len() > MAX_STEPS {
        bail!("action_sequence takes at most {MAX_STEPS} steps.");
    }
    let limit = match (args.fail_fast, args.max_failures) {
        (true, Some(_)) => bail!("Give fail_fast or max_failures, not both."),
        (_, Some(0)) => bail!("max_failures must be at least 1."),
        (_, Some(limit)) => Some((limit, "max_failures")),
        (true, None) => Some((1, "fail_fast")),
        (false, None) => None,
    };
    let repeats_every = |step: &Step, ticks: bool| {
        step.repeat.as_ref().is_some_and(|repeat| {
            if ticks {
                repeat.every_ticks.is_some()
            } else {
                repeat.every_ms.is_some()
            }
        })
    };
    let by_tick = args
        .steps
        .iter()
        .any(|step| step.at_tick.is_some() || repeats_every(step, true));
    let by_ms = args
        .steps
        .iter()
        .any(|step| step.after_ms.is_some() || repeats_every(step, false));
    if by_tick && by_ms {
        bail!(
            "Schedule every step with at_tick (and repeat every_ticks) or every step with after_ms \
             (and repeat every_ms), not both."
        );
    }
    let timeout_ms = args.timeout_ms.unwrap_or(DEFAULT_SEQUENCE_MS);
    if timeout_ms > MAX_WAIT_MS {
        bail!("timeout_ms is at most {MAX_WAIT_MS}.");
    }
    let record = args.recording()?;
    let warm_mode = warm::WarmUp::parse(args.warm_up.as_ref())?;
    let actor = match &args.actor {
        Some(name) => Actor::try_from(name.clone()).map_err(Error::msg)?,
        None => Actor::Me,
    };
    if let Some(check) = &args.stop_when {
        check
            .validate()
            .map_err(|error| Error::msg(format!("stop_when: {error}")))?;
    }
    let mut planned = Vec::with_capacity(args.steps.len());
    let mut at = 0;
    for (index, step) in args.steps.into_iter().enumerate() {
        let failed = |error: Error| Error::msg(format!("Step {index}: {error}"));
        if step.action == "action_sequence" {
            bail!("Step {index}: a sequence cannot contain another sequence.");
        }
        if step.wait_ms.is_some() && step.wait_for.is_none() {
            bail!("Step {index}: wait_ms only applies with wait_for.");
        }
        if step.wait_ms.is_some_and(|ms| ms > MAX_WAIT_MS) {
            bail!("Step {index}: wait_ms is at most {MAX_WAIT_MS}.");
        }
        for check in step.wait_for.iter().chain(&step.only_if) {
            check.validate().map_err(failed)?;
        }
        if let Some(expect) = &step.expect {
            expect.validate().map_err(failed)?;
        }
        let repeat = step
            .repeat
            .map(|repeat| repeat.plan(by_tick))
            .transpose()
            .map_err(failed)?;
        let mut step_args = if step.args.is_null() {
            json!({})
        } else {
            step.args
        };
        if let (Some(actor), Some(fields)) = (&args.actor, step_args.as_object_mut()) {
            fields
                .entry("actor")
                .or_insert_with(|| actor.as_str().into());
        }
        if let Some(label) = &step.label {
            if step.action != "spawn" {
                bail!(
                    "Step {index}: as names a spawn step's entity; this step is {}.",
                    step.action
                );
            }
            if label.is_empty() || !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                bail!("Step {index}: as must be letters, digits and underscores, such as A.");
            }
            if planned
                .iter()
                .any(|earlier: &Planned| earlier.label.as_ref() == Some(label))
            {
                bail!("Step {index}: another step already has as: {label}.");
            }
        }
        let request = Request::parse(&step.action, step_args.clone()).map_err(failed)?;
        if let Request::Spawn(spawn) = &request {
            spawn.check().map_err(failed)?;
        }
        at = step.at_tick.or(step.after_ms).unwrap_or(at);
        let wait = Duration::from_millis(step.wait_ms.unwrap_or(timeout_ms));
        let mut step = Planned {
            index,
            action: step.action,
            at,
            request,
            args: step_args,
            refs: false,
            label: step.label,
            wait_for: step.wait_for.map(|check| (check, wait)),
            only_if: step.only_if,
            repeat,
            expect: step.expect,
            lead: 0,
        };
        let references = step.references();
        for name in &references {
            let spawned = planned.iter().any(|earlier| {
                earlier.action == "spawn"
                    && earlier.at <= step.at
                    && (earlier.label.as_deref() == Some(name.as_str())
                        || *name == format!("spawn:{}", earlier.index))
            });
            if !spawned {
                bail!(
                    "Step {index}: ${name} names no earlier spawn step; give a spawn step as: \
                     {name}, or use $spawn:<its index>."
                );
            }
        }
        step.refs = !references.is_empty();
        planned.push(step);
    }
    if let Some(check) = &args.stop_when {
        let mut references = Vec::new();
        refs::in_check(check, &mut references);
        for name in references {
            let spawned = planned.iter().any(|step| {
                step.action == "spawn"
                    && (step.label.as_deref() == Some(name.as_str())
                        || name == format!("spawn:{}", step.index))
            });
            if !spawned {
                bail!("stop_when: ${name} names no spawn step.");
            }
        }
    }
    let checks: Vec<&Check> = planned
        .iter()
        .flat_map(Planned::checks)
        .chain(&args.stop_when)
        .collect();
    let reads_events = checks.iter().any(|check| check.reads_events());
    let reads_state = checks.iter().any(|check| check.reads_state());
    let world = game.world()?;
    let bridge = (by_tick || !checks.is_empty()) && world.bridge(true)?;
    // Whether the pack answers at all, which decides where kills and hurts come from.
    let world_bridge = bridge || world.bridge(false)?;
    if reads_state && !bridge {
        bail!("state conditions read the Bridge pack's player state. {INSTALL_HINT}");
    }
    let ticks = by_tick && bridge;
    // With the tick clock a simulated player's step is sent ahead of its tick, unless it
    // waits on the game. An actor self step that aims starts early enough for its click to go
    // out on its tick, but not before the self step ahead of it, so the player's steps keep
    // their order.
    planned.sort_by_key(|step| step.at);
    let mut last_self: Option<u64> = None;
    let mut last_spawn: Option<u64> = None;
    for step in &mut planned {
        let waits = step.wait_for.is_some() || step.only_if.is_some();
        step.lead = match step.request.actor() {
            _ if !ticks => 0,
            Actor::Named(_) if !step.conditional() => SEND_AHEAD,
            Actor::Named(_) => 0,
            Actor::Me => {
                let room = last_self.map_or(AIM_LEAD, |before| step.at - before);
                last_self = Some(step.at);
                if waits {
                    0
                } else {
                    step.request.lead().min(room)
                }
            }
        };
        // A step that names a spawned entity starts after its spawn step.
        if step.refs
            && let Some(spawned) = last_spawn
        {
            step.lead = step.lead.min(step.at - spawned);
        }
        if step.action == "spawn" {
            last_spawn = Some(step.at);
        }
    }
    planned.sort_by_key(|step| (step.at as i64 - step.lead as i64, step.at));
    let ahead = planned.iter().map(|step| step.lead).max().unwrap_or(0);

    let mut notes = Vec::new();
    if by_tick && !ticks {
        notes.push(
            "at_tick needs the Bridge pack's tick (re_mcp:ping); without it each tick counts as 50 \
             ms of wall clock time."
                .to_owned(),
        );
    }
    // Conditions count events from when the step before started; the first step's from now.
    let previous = if reads_events {
        game.cursor()?
    } else {
        EventCursor::default()
    };
    // The summary reads deaths and hits from here, the pack's or the native tracker's; read
    // before the clock starts.
    let summary_from = if reads_events {
        previous.clone()
    } else {
        game.cursor()?
    };
    let started = Instant::now();
    let clock = if ticks {
        Clock::Tick {
            start: current_tick(&world)? + ahead,
        }
    } else {
        Clock::Wall {
            start: started,
            unit: if by_tick {
                TICK
            } else {
                Duration::from_millis(1)
            },
        }
    };
    let run = Run {
        world,
        bridge,
        actor,
        clock,
        started,
        deadline: started + Duration::from_millis(timeout_ms),
        running: Running::new(
            match clock {
                Clock::Tick { start } => Some(start),
                Clock::Wall { .. } => None,
            },
            None,
        ),
        stop_when: args
            .stop_when
            .map(|check| Watch::new(check, previous.clone())),
        stopped: None,
        checked: None,
        presses: Presses::default(),
        cursor: ticks.then(|| summary_from.clone()),
        idle_cost: Duration::ZERO,
        cause: "stop_when",
        limit,
        native: (!world_bridge).then(native::Tracker::default),
    };
    // Auto warms up the connected player only for runs that play it.
    let plays_self = planned
        .iter()
        .any(|step| *step.request.actor() == Actor::Me && step.action != "spawn");
    let warm =
        (warm_mode != warm::WarmUp::Auto || plays_self) && warm_mode.wanted(game, record.is_some());
    Ok(Prepared {
        run,
        planned,
        by_tick,
        by_ms,
        ticks,
        reads_events,
        previous,
        notes,
        confirm,
        record,
        ahead,
        timeout: Duration::from_millis(timeout_ms),
        summary_from,
        warm,
    })
}

/// Run a prepared sequence to its end; see [`sequence_with`]. A warm-up runs first when the
/// plan asks for one. With `record`, a recording then starts before the first step and stops
/// once the last one settled, also when the run fails. The steps' clock starts after both.
fn execute(game: &mut dyn Game, mut prepared: Prepared) -> Result<Value> {
    let warmed = prepared.warm.then(|| warm::warm_up(game));
    let record = prepared.record.take();
    let started = match record {
        Some(record) => Some(game.record_start(record).map_err(|error| {
            Error::msg(format!("record: the recording did not start: {error}"))
        })?),
        None => None,
    };
    if warmed.is_some() || started.is_some() {
        prepared.run.restart(prepared.ahead, prepared.timeout)?;
    }
    let result = execute_steps(game, prepared);
    let stopped = started.as_ref().map(|_| game.record_stop());
    let mut result = result?;
    if let Some(warmed) = warmed {
        result["warm_up"] = warmed;
    }
    if let (Some(started), Some(stopped)) = (started, stopped) {
        result["recording"] = match stopped {
            Ok(mut report) => {
                if let (Some(report), Some(notes)) =
                    (report.as_object_mut(), started["notes"].as_array())
                    && !notes.is_empty()
                {
                    report.insert("start_notes".to_owned(), notes.clone().into());
                }
                report
            }
            Err(error) => {
                json!({ "error": format!("The recording did not stop cleanly: {error}") })
            }
        };
    }
    Ok(result)
}

/// Run the steps of a prepared sequence; see [`execute`].
fn execute_steps(game: &mut dyn Game, prepared: Prepared) -> Result<Value> {
    let Prepared {
        mut run,
        planned,
        by_tick,
        by_ms,
        ticks,
        reads_events,
        mut previous,
        mut notes,
        confirm,
        summary_from: events_from,
        ..
    } = prepared;
    let mut acted: Vec<Actor> = Vec::new();
    // The entity each actor aimed at last, which its item uses count toward.
    let mut aimed: Vec<(Actor, String)> = Vec::new();
    let mut spawned: Vec<summary::Spawned> = Vec::new();
    let mut ids: HashMap<String, String> = HashMap::new();
    let mut planned = planned.into_iter();
    for mut step in planned.by_ref() {
        let mut entry = json!({ "index": step.index, "action": step.action });
        if by_tick {
            entry["at_tick"] = step.at.into();
        } else if by_ms {
            entry["after_ms"] = step.at.into();
        }
        if run.stopping(game, true) {
            entry["skipped"] = run.skip_note().into();
            run.running.settle(entry);
            break;
        }
        let lead = step.lead;
        let mut start = Start::default();
        let mut now = None;
        let ready = match (run.wait_due(game, step.at, lead)?, run.clock) {
            (Due::Now(Some(tick)), Clock::Tick { start: zero }) => {
                now = Some(tick);
                let act = zero + step.at;
                if lead == 0 {
                    entry["started_tick"] = tick.saturating_sub(zero).into();
                } else if matches!(step.request.actor(), Actor::Named(_)) {
                    start.at = Some((act, TICK * act.saturating_sub(tick) as u32));
                }
                Instant::now() + TICK * act.saturating_sub(tick) as u32 <= run.deadline
            }
            (Due::Now(_), _) => true,
            (Due::Expired, _) => false,
            (Due::Stopped, _) => {
                entry["skipped"] = run.skip_note().into();
                run.running.settle(entry);
                break;
            }
        };
        if !ready || Instant::now() > run.deadline {
            entry["skipped"] = "The sequence timeout passed before this step's time.".into();
            run.running.settle(entry);
            continue;
        }
        if let Err(error) = step.resolve(&ids) {
            entry["error"] = error.to_string().into();
            run.running.settle(entry);
            continue;
        }
        if let Some((check, wait)) = step.wait_for.take() {
            let mut watch = Watch::new(check, previous.clone());
            let waited = Instant::now();
            let until = (waited + wait).min(run.deadline);
            match run.wait_check(game, &mut watch, until) {
                Ok(Held::Yes(detail)) => {
                    entry["wait_for"] = json!({
                        "waited_ms": waited.elapsed().as_millis() as u64,
                        "detail": detail,
                    });
                    if let Clock::Tick { start: zero } = run.clock {
                        let tick = current_tick(&run.world)?;
                        now = Some(tick);
                        entry["started_tick"] = tick.saturating_sub(zero).into();
                    }
                }
                Ok(Held::Expired) => {
                    entry["skipped"] = format!(
                        "wait_for did not hold within {} ms.",
                        waited.elapsed().as_millis()
                    )
                    .into();
                    run.running.settle(entry);
                    continue;
                }
                Ok(Held::Stopped) => {
                    entry["skipped"] = run.skip_note().into();
                    run.running.settle(entry);
                    break;
                }
                Err(error) => {
                    entry["error"] = format!("wait_for could not be checked: {error}").into();
                    run.running.settle(entry);
                    continue;
                }
            }
        }
        run.observe(game);
        if let Some(check) = step.only_if.take() {
            match Watch::new(check, previous.clone()).holds(game, &run.probe()) {
                Ok(Some(detail)) => entry["if"] = detail,
                Ok(None) => {
                    entry["skipped"] = "Its if condition did not hold.".into();
                    run.running.settle(entry);
                    continue;
                }
                Err(error) => {
                    entry["error"] = format!("if could not be checked: {error}").into();
                    run.running.settle(entry);
                    continue;
                }
            }
        }
        let since = previous.clone();
        if reads_events {
            previous = game.cursor()?;
        }
        let actor = step.request.actor();
        if !acted.contains(actor) {
            acted.push(actor.clone());
        }
        let target = match (&step.request, step.request.target()) {
            (Request::Spawn(_), _) => None,
            (_, Some(id)) => {
                aimed.retain(|(by, _)| by != actor);
                aimed.push((actor.clone(), id.to_owned()));
                Some(id.to_owned())
            }
            (Request::ItemUse(_) | Request::ItemUseOn(_), None) => aimed
                .iter()
                .find(|(by, _)| by == actor)
                .map(|(_, id)| id.clone()),
            _ => None,
        };
        if let Some(target) = target {
            run.track(game, &target);
            entry["target"] = target.into();
        }
        entry["started_ms"] = run.elapsed_ms().into();
        game.mark(&format!("step {} {}", step.index, step.action));
        let index = run.running.push(entry, step.expect.take());
        match &step.repeat {
            Some(runs) => {
                run.running.entries[index]["runs"] = json!([]);
                let first = match (now, run.clock) {
                    (Some(tick), Clock::Tick { start: zero }) => {
                        tick.saturating_sub(zero).max(step.at)
                    }
                    _ => run.position()?.max(step.at),
                };
                let request = &step.request;
                run.repeat(game, request, runs, index, since, confirm, first, lead, now)?;
            }
            None => {
                let slot = Slot {
                    entry: index,
                    run: None,
                };
                if run.start_action(game, &step.request, confirm, start, slot, step.at, now)? {
                    run.running.entries[index]["skipped"] = run.skip_note().into();
                    break;
                }
            }
        }
        if step.action == "spawn" {
            // A spawn finishes when it starts; its id stands in for its references from now.
            let entry = &run.running.entries[index];
            let last = match entry["runs"].as_array() {
                Some(runs) => runs.last().unwrap_or(&Value::Null),
                None => entry,
            };
            if let Some(id) = last["outcome"]["evidence"]["id"].as_str() {
                ids.insert(format!("spawn:{}", step.index), id.to_owned());
                if let Some(label) = &step.label {
                    ids.insert(label.clone(), id.to_owned());
                }
                spawned.push(summary::Spawned {
                    id: id.to_owned(),
                    label: step.label.clone(),
                    step: step.index,
                    kind: last["outcome"]["evidence"]["type"].clone(),
                });
                if let Some(watch) = run.stop_when.as_mut() {
                    let mut names = Vec::new();
                    refs::in_check(watch.check_mut(), &mut names);
                    if names.iter().all(|name| ids.contains_key(name)) {
                        refs::substitute_check(watch.check_mut(), &ids)?;
                    }
                }
            }
            if let Some(spawn) = spawned.last().filter(|spawn| spawn.step == step.index) {
                let id = spawn.id.clone();
                run.track(game, &id);
            }
        }
    }
    for step in planned {
        let mut entry =
            json!({ "index": step.index, "action": step.action, "skipped": run.skip_note() });
        if by_tick {
            entry["at_tick"] = step.at.into();
        } else if by_ms {
            entry["after_ms"] = step.at.into();
        }
        run.running.settle(entry);
    }
    while !run.running.steps.is_empty() && !run.stopping(game, false) {
        for _ in 0..run.running.steps.len() {
            run.service(game);
        }
        game.pause(TICK_POLL);
    }
    // A last read, so a death in the final moments still reaches the summary.
    if let Some(tracker) = run.native.as_mut() {
        tracker.poll(game, true);
        if let Some(reason) = tracker.stopped() {
            notes.push(format!(
                "Without the Bridge pack, deaths and hurts come from native health reads, which \
                 stopped: {reason}. killed_by, final and entity_die conditions may miss them."
            ));
        }
    }
    // A failure of the last action still ends the sequence as fail_fast says.
    if run.stopped.is_none() {
        run.over_limit();
    }
    let mut stopped = run.stopped.take();
    if let Some(stopped) = stopped.as_mut() {
        let note = run.interrupt_note();
        for started in std::mem::take(&mut run.running.steps) {
            run.running.slot(started.slot)["interrupted"] = note.as_str().into();
        }
        stopped["actors_stopped"] = acted
            .into_iter()
            .map(|actor| {
                match perform(
                    game,
                    &Request::Stop(ActorOnly {
                        actor: actor.clone(),
                    }),
                    confirm,
                ) {
                    Ok(outcome) => outcome.to_json(),
                    Err(error) => json!({
                        "actor": actor_name(&actor),
                        "error": error.to_string(),
                    }),
                }
            })
            .collect();
    }
    for entry in 0..run.running.entries.len() {
        run.running.publish(entry);
    }
    let failures = run.running.failures;
    let wall_ms = run.elapsed_ms();
    let steps = std::mem::take(&mut run.running.entries);
    let mut late: Vec<(u64, String)> = steps
        .iter()
        .filter(|step| step.get("wait_for").is_none())
        .flat_map(|step| {
            let runs = step["runs"].as_array().map_or_else(Vec::new, |runs| {
                runs.iter()
                    .map(|run| (format!("step {} run {}", step["index"], run["run"]), run))
                    .collect()
            });
            std::iter::once((format!("step {}", step["index"]), step)).chain(runs)
        })
        .filter_map(|(name, action)| {
            let (at, started) = (
                action["at_tick"].as_u64()?,
                action["started_tick"].as_u64()?,
            );
            (started > at + 1).then(|| (started - at, format!("{name} by {} ticks", started - at)))
        })
        .collect();
    if !late.is_empty() {
        late.sort_by_key(|(delay, _)| std::cmp::Reverse(*delay));
        let latest: Vec<&str> = late.iter().take(3).map(|(_, name)| name.as_str()).collect();
        notes.push(format!(
            "{} actions started more than a tick after their at_tick (latest: {}): an earlier \
             action of the same player still held a control they need (a hold, a walk or \
             breaking a block), a wait_for or the step before took that long, or the game \
             answered slowly. Each step's at_tick and started_tick (verbose: true) show them all.",
            late.len(),
            latest.join(", ")
        ));
    }
    let confirmed = steps
        .iter()
        .flat_map(|step| match step["runs"].as_array() {
            Some(runs) => runs.iter().collect(),
            None => vec![step],
        })
        .filter(|action| action["outcome"]["confirmed"] == true)
        .count();
    let mut result = json!({
        "clock": if ticks { "tick" } else { "wall" },
        "confirmed": confirmed,
        "steps": steps,
        "stopped": stopped,
        "notes": notes,
    });
    if run.limit.is_some() {
        result["failures"] = failures.into();
    }
    let ends = match &run.native {
        Some(tracker) => summary::Ends::Native(tracker),
        None => summary::Ends::Bridge,
    };
    let summary = summary::summarize(
        game,
        &run.world,
        &result["steps"],
        &spawned,
        &events_from,
        ends,
    );
    result["timing"] = json!({
        "wall_ms": wall_ms,
        "first_tick": summary["first_tick"],
        "last_tick": summary["last_tick"],
    });
    result["summary"] = summary;
    Ok(result)
}
