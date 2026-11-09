//! The fight skill for the connected player: pick the nearest matching mob, walk to it along
//! a path, then fight as a player would: keep it in reach and out of point-blank range, track
//! where it will be with the eased camera (faster the closer it is), swing only while the
//! crosshair is on its body, strafe side to side in between, and open with a sprint hit for
//! knockback. When the eased view does not catch up, one mouse flick turns it the rest of the
//! way. With `all` it moves on to the next mob until none is left, the time runs out or
//! `max_targets` were fought.
//!
//! Hits and kills come from the Bridge pack's `entity_hurt` and `entity_die` events, else
//! from the unstable layer's native health reads (single player), else only from the mob
//! being gone from querytarget, which cannot tell a kill from a despawn.

use std::{
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    Game, MAX_WAIT_MS, Outcome, TICK,
    native::{NativeRead, Tracker},
    point_of, rounded,
    view::{self, Exact},
    walk::{Gait, Pose, PoseReader, Walked, Walker},
};
use crate::{
    bridge::{Actor, quoted},
    error::{Error, Result, bail},
    events::{EventCursor, Source},
    input::{InputRequest, MouseMotion, Movement as Axes},
    world::{World, facing, number, shortest_turn},
};

const DEFAULT_FIGHT_MS: u64 = 60_000;
/// How far around the player a fight looks for mobs by default, and at most.
const DEFAULT_RADIUS: f64 = 24.0;
const MAX_RADIUS: f64 = 64.0;
/// How many mobs `all` fights by default, and at most.
const DEFAULT_ALL_TARGETS: u32 = 16;
const MAX_TARGETS: u32 = 64;
/// How close, horizontally, walking brings the player before it fights.
const ENGAGE: f64 = 2.6;
/// Beyond this horizontal distance the player walks to the mob again.
const CHASE: f64 = 3.6;
/// The farthest horizontal distance a swing goes out at: the player reaches 3 blocks to the
/// mob's body (https://minecraft.wiki/w/Interaction_range), and the body is about 0.3 blocks
/// from its middle.
const SWING_RANGE: f64 = 3.0;
/// Closer than this the player steps back while it turns, so the mob cannot circle it at
/// point-blank range, where the view cannot keep up.
const KEEP_OFF: f64 = 1.5;
/// Farther than this the player steps in, so the mob stays within reach.
const STEP_IN: f64 = 2.8;
/// Where along a footwork step the ground is checked, in blocks: a step back or aside never
/// goes off a platform's edge, with room left for a mob's knockback.
const STEP_CHECKS: [f64; 2] = [0.8, 2.0];
/// A sprint hit needs a run-up from at least this far.
const RUN_UP_FROM: f64 = 2.2;
/// Time between swings: a mob ignores damage for 10 ticks after it was hurt
/// (https://minecraft.wiki/w/Damage#Immunity), plus a tick of margin.
const SWING_EVERY: Duration = Duration::from_millis(550);
/// Time between sprint hits, and how long the run-up holds sprint and forward before it.
const SPRINT_EVERY: Duration = Duration::from_millis(1500);
const RUN_UP: Duration = Duration::from_millis(150);
/// How long the player strafes one way before it turns to the other.
const STRAFE_FLIP: Duration = Duration::from_millis(600);
/// How long after a swing a native health drop still counts as its hit.
const HIT_WINDOW: Duration = Duration::from_millis(700);
/// How long after a mob vanished the Bridge pack's death event may still arrive.
const DEATH_GRACE: Duration = Duration::from_millis(1000);
/// Where on its body the aim goes, as a share of its height from the feet: the torso.
const AIM_SHARE: f64 = 0.62;

/// How tall a mob of type `kind` stands, from its collision box on the wiki's mob pages; the
/// reads give no box, so other types count as 1.8 tall.
fn body_height(kind: Option<&str>) -> f64 {
    match kind.map(|kind| kind.trim_start_matches("minecraft:")) {
        Some("zombie" | "husk" | "drowned" | "zombie_villager" | "zombie_villager_v2") => 1.95,
        Some("skeleton" | "stray" | "bogged") => 1.99,
        Some("wither_skeleton") => 2.4,
        Some("creeper") => 1.7,
        Some("spider") => 0.9,
        Some("cave_spider") => 0.5,
        Some("enderman") => 2.9,
        Some("witch" | "pillager" | "vindicator" | "evoker" | "piglin" | "zombie_pigman") => 1.95,
        _ => 1.8,
    }
}

/// How far off the read view may be from the aim for a swing: sideways
/// the crosshair must stay on the body, about this many blocks from its middle to each side,
/// and never less than this many degrees; up or down the body is tall, so the pitch has more
/// room. Without the pack the reads lag, so a flick waits until they show it.
const AIM_HALF_WIDTH: f64 = 0.4;
const AIM_YAW: f64 = 5.0;
const AIM_PITCH: f64 = 15.0;
/// Flicks per due swing: the flick and one correction.
const MAX_FLICKS: u8 = 2;
/// The longest a flick waits for the reads to show it.
const MAX_FLICK_WAIT: Duration = Duration::from_millis(400);
/// How long after a /tp aim the click waits: the turn reaches the client from the server, and
/// a click before it would go along the old view.
const TP_SETTLE: Duration = Duration::from_millis(100);
/// How long after an exact aim (/tp or the pack's) the eased steps wait: until the reads show
/// the new view.
const EXACT_QUIET: Duration = Duration::from_millis(400);
/// How long the fight keeps looking when no mob matches, before it ends: a mob summoned
/// right before the fight may not show in the reads yet.
const PICK_GRACE: Duration = Duration::from_millis(1500);
/// The most other mobs whose health is read to count hits that went to them.
const MAX_OTHERS: usize = 8;
/// The most events one poll handles.
const EVENT_BATCH: usize = 64;

/// How far one read-lag probe turns, in mouse counts, how many go out (alternately left and
/// right, so the view ends where it was), and how long each waits for the read to show it.
const LAG_COUNTS: f64 = 40.0;
const LAG_SAMPLES: usize = 4;
const LAG_LIMIT: Duration = Duration::from_millis(500);

/// How long the native view read takes to show a mouse turn: the median of a few small turns
/// timed from the motion to the first read that shows it. `None` when fewer than two showed.
fn measure_lag(game: &mut dyn Game, reader: &mut PoseReader) -> Option<Duration> {
    let mut samples = Vec::new();
    for sample in 0..LAG_SAMPLES {
        let before = reader.read(game).ok()?.yaw;
        let sign = if sample % 2 == 0 { 1.0 } else { -1.0 };
        game.input(InputRequest {
            mouse: Some(MouseMotion {
                x: sign * LAG_COUNTS,
                y: 0.0,
            }),
            ..InputRequest::default()
        })
        .ok()?;
        let sent = Instant::now();
        while sent.elapsed() < LAG_LIMIT {
            let now = reader.read(game).ok()?.yaw;
            if shortest_turn(before, now).abs() >= 0.5 {
                samples.push(sent.elapsed());
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
    median(samples)
}

/// The middle of `samples`, or `None` with fewer than two.
fn median(mut samples: Vec<Duration>) -> Option<Duration> {
    if samples.len() < 2 {
        return None;
    }
    samples.sort();
    Some(samples[samples.len() / 2])
}

/// How long a flick waits for the reads to show the view on the aim: twice the read lag, at
/// most [`MAX_FLICK_WAIT`].
fn flick_wait(read_lag: Duration) -> Duration {
    (read_lag * 2).min(MAX_FLICK_WAIT)
}

/// A mouse flick toward a mob whose result the reads may not show yet.
#[derive(Clone, Copy)]
struct Flick {
    sent: Instant,
    /// Flicks sent for this swing so far.
    count: u8,
}

/// The forward key for a mob `gap` blocks away: back off inside [`KEEP_OFF`], step in beyond
/// [`STEP_IN`], else stand.
fn forward_for(gap: f64) -> f64 {
    if gap < KEEP_OFF {
        -1.0
    } else if gap > STEP_IN {
        1.0
    } else {
        0.0
    }
}

/// Whether a view at `yaw` and `pitch` is close enough to `aim` (yaw, pitch) for a swing at
/// a mob `gap` blocks away: within its body sideways (see [`AIM_HALF_WIDTH`]) and
/// [`AIM_PITCH`] up or down.
fn on_aim(yaw: f64, pitch: f64, aim: (f64, f64), gap: f64) -> bool {
    let room = AIM_HALF_WIDTH.atan2(gap.max(0.1)).to_degrees().max(AIM_YAW);
    let off_yaw = shortest_turn(yaw, aim.0).abs();
    off_yaw <= room && (pitch - aim.1).abs() <= AIM_PITCH
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fight {
    #[serde(default)]
    pub actor: Actor,
    pub target: Option<String>,
    #[serde(default)]
    pub all: bool,
    pub timeout_ms: Option<u64>,
    pub max_targets: Option<u32>,
    pub radius: Option<f64>,
    #[serde(default)]
    pub aim: AimMode,
}

/// How the fight aims without the Bridge pack (with it, the pack's exact aim always does).
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AimMode {
    /// /tp to the torso right before the click: accurate, and each one prints a chat line.
    #[default]
    Tp,
    /// Mouse flicks, waiting for the lagging reads to show them: quiet, and less accurate.
    Mouse,
}

/// Which mobs a fight takes on.
#[derive(Clone, Debug, PartialEq)]
pub enum Wanted {
    Id(String),
    Type(String),
    Family(String),
}

impl Wanted {
    /// An entity id (digits, maybe with a minus), a type id (with a colon) or a family name;
    /// without a target, the monster family.
    pub fn parse(target: Option<&str>) -> Result<Self> {
        let target = target.unwrap_or("monster").trim();
        let allowed = |char: char| char.is_ascii_alphanumeric() || "_:.-".contains(char);
        if target.is_empty() || !target.chars().all(allowed) {
            bail!(
                "target {target:?} must be an entity id, a type id such as minecraft:zombie or a \
                 family such as monster."
            );
        }
        let digits = target.strip_prefix('-').unwrap_or(target);
        Ok(if digits.chars().all(|char| char.is_ascii_digit()) {
            Self::Id(target.to_owned())
        } else if target.contains(':') {
            Self::Type(target.to_owned())
        } else {
            Self::Family(target.to_owned())
        })
    }

    /// The querytarget selector of the matching mobs within `radius` of the player.
    fn selector(&self, radius: f64) -> String {
        let r = number(radius);
        match self {
            Self::Id(_) => format!("@e[r={r},type=!player]"),
            Self::Type(kind) => format!("@e[r={r},type={kind}]"),
            Self::Family(family) => format!("@e[r={r},family={family},type=!player]"),
        }
    }
}

impl Fight {
    fn check(&self) -> Result<(Wanted, Duration, u32, f64)> {
        let wanted = Wanted::parse(self.target.as_deref())?;
        let timeout = self.timeout_ms.unwrap_or(DEFAULT_FIGHT_MS);
        if timeout > MAX_WAIT_MS {
            bail!("timeout_ms is at most {MAX_WAIT_MS}.");
        }
        let most = match (self.max_targets, self.all) {
            (Some(count), _) if !(1..=MAX_TARGETS).contains(&count) => {
                bail!("max_targets must be 1 to {MAX_TARGETS}.")
            }
            (Some(count), _) => count,
            (None, true) => DEFAULT_ALL_TARGETS,
            (None, false) => 1,
        };
        if matches!(wanted, Wanted::Id(_)) && (self.all || most > 1) {
            bail!("A fight against one entity id takes no all or max_targets above 1.");
        }
        let radius = self.radius.unwrap_or(DEFAULT_RADIUS);
        if !(radius > 0.0 && radius <= MAX_RADIUS) {
            bail!("radius must be above 0 and at most {MAX_RADIUS}.");
        }
        Ok((wanted, Duration::from_millis(timeout), most, radius))
    }
}

/// A mob as one read saw it.
#[derive(Clone, Debug)]
struct Foe {
    id: String,
    kind: Option<String>,
    feet: [f64; 3],
    /// Current and maximum health, when the read gives them.
    health: Option<[f64; 2]>,
}

/// How a kill showed.
#[derive(Clone, Debug)]
struct Kill {
    /// bridge (entity_die), native (health 0 or gone after a read) or command (gone from
    /// querytarget: not a confirmation).
    source: &'static str,
    seen: &'static str,
    tick: Option<u64>,
}

impl Kill {
    fn confirmed(&self) -> bool {
        self.source != "command"
    }
}

/// One mob the fight took on.
struct Engagement {
    foe: Foe,
    started: Instant,
    swings: u32,
    sprint_swings: u32,
    hits: u32,
    /// A swing whose native health drop is still awaited: when, and the health before it.
    pending: Option<(Instant, f64)>,
    killed: Option<(Kill, Instant)>,
    /// When the mob was last missing from the reads.
    vanished: Option<Instant>,
    /// Why the fight left it alive.
    left: Option<(&'static str, String)>,
    walking: bool,
    /// Swings whose native read saw no health drop: a hit on armor, during the mob's hurt
    /// immunity, or a miss.
    no_damage: u32,
    /// Swings that went out with the view still off the aim after a flick and its
    /// correction.
    off_aim: u32,
    /// Time spent walking to the mob, with a swing due but waiting for its aim, and waiting
    /// out the swing cadence; and the fight's ticks with the mob.
    phases: [Duration; 3],
    ticks: u32,
    last_tick: Option<Instant>,
}

impl Engagement {
    fn new(foe: Foe) -> Self {
        Self {
            foe,
            started: Instant::now(),
            swings: 0,
            sprint_swings: 0,
            hits: 0,
            pending: None,
            killed: None,
            vanished: None,
            left: None,
            walking: false,
            no_damage: 0,
            off_aim: 0,
            phases: [Duration::ZERO; 3],
            ticks: 0,
            last_tick: None,
        }
    }

    fn to_json(&self, counts_hits: bool) -> Value {
        let (hits, misses) = if counts_hits {
            (
                json!(self.hits),
                json!(self.swings.saturating_sub(self.hits)),
            )
        } else {
            (Value::Null, Value::Null)
        };
        let mut value = json!({
            "id": self.foe.id,
            "type": self.foe.kind,
            "swings": self.swings,
            "sprint_swings": self.sprint_swings,
            "hits": hits,
            "misses": misses,
            "killed": self.killed.is_some(),
            "kill": self.killed.as_ref().map(|(kill, _)| json!({
                "confirmed": kill.confirmed(),
                "source": kill.source,
                "seen": kill.seen,
                "tick": kill.tick,
            })),
            "time_to_kill_ms": self.killed.as_ref().map(|(_, at)| {
                at.duration_since(self.started).as_millis() as u64
            }),
        });
        if counts_hits {
            value["no_damage"] = self.no_damage.into();
        }
        value["off_aim"] = self.off_aim.into();
        let ms = |phase: Duration| phase.as_millis() as u64;
        value["phases_ms"] = json!({
            "walking": ms(self.phases[0]),
            "aiming": ms(self.phases[1]),
            "cadence": ms(self.phases[2]),
            "ticks": self.ticks,
        });
        if let Some((class, detail)) = &self.left {
            value["left"] = json!({ "class": class, "detail": detail });
        }
        value
    }
}

/// Where hits and deaths come from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reads {
    Bridge,
    Native,
    Command,
}

impl Reads {
    fn name(self) -> &'static str {
        match self {
            Self::Bridge => "bridge",
            Self::Native => "native",
            Self::Command => "command",
        }
    }
}

/// The movement keys a fight holds.
#[derive(Clone, Copy, Default, PartialEq)]
struct Keys {
    forward: f64,
    strafe: f64,
    sprint: bool,
    /// Held near an edge: a sneaking player does not walk or get pushed off a block's edge.
    sneak: bool,
}

/// A fight while it runs; see the module docs.
pub struct Fighter {
    world: World,
    reads: Reads,
    /// The player's name, which the Bridge pack's events name as the attacker.
    name: Option<String>,
    wanted: Wanted,
    radius: f64,
    most: u32,
    started: Instant,
    deadline: Instant,
    walker: Walker,
    cursor: Option<EventCursor>,
    /// With native reads: follows each mob fought and adds its hurts and death to the event
    /// stream as `native` events, as a sequence does for the mobs its steps target.
    tracker: Tracker,
    done: Vec<Engagement>,
    current: Option<Engagement>,
    keys: Keys,
    last_swing: Option<Instant>,
    last_sprint: Option<Instant>,
    run_up: Option<Instant>,
    strafe: f64,
    strafed: Instant,
    next: Instant,
    /// Mouse flicks that turned the view the rest of the way onto a mob.
    flicks: u32,
    /// Swings aimed with a look command because mouse motion does not turn the view.
    look_aims: u32,
    aim_mode: AimMode,
    /// A flick the reads may not show yet, while a due swing waits for the view.
    flick: Option<Flick>,
    /// When the last /tp aim went out, while its click waits for [`TP_SETTLE`].
    teleported: Option<Instant>,
    /// When the last exact aim (/tp or the pack's) went out.
    exact_at: Option<Instant>,
    /// Since when no mob matched.
    empty_since: Option<Instant>,
    /// Hits on other mobs than the one fought: a crowd in the way of the swing.
    stray_hits: u32,
    /// The other mobs the fight picked from, with their last read health, for `stray_hits`.
    others: Vec<(String, f64)>,
    ended: Option<&'static str>,
    notes: Vec<String>,
}

impl Fighter {
    /// Check the arguments and read what the fight needs before its first step.
    pub fn start(game: &mut dyn Game, args: &Fight) -> Result<Self> {
        let (wanted, timeout, most, radius) = args.check()?;
        let world = game.world()?;
        let bridge = world.bridge(true)?;
        let mut notes = Vec::new();
        let reads = if bridge {
            Reads::Bridge
        } else {
            match game.native_list(1.0, None) {
                Ok(_) => Reads::Native,
                Err(error) => {
                    notes.push(format!(
                        "Neither the Bridge pack nor the native read ({error}) answers, so hits \
                         are not counted and a mob gone from querytarget counts as killed \
                         without confirmation (a despawn reads the same)."
                    ));
                    Reads::Command
                }
            }
        };
        let name = if bridge {
            world.bridge_command("re_mcp:state")?["name"]
                .as_str()
                .map(str::to_owned)
        } else {
            None
        };
        let cursor = if bridge { Some(game.cursor()?) } else { None };
        let gait = Gait {
            radius: ENGAGE,
            water: false,
            sprint: false,
            jump_held: false,
        };
        let mut walker = Walker::new(world.clone(), bridge, gait);
        if reads == Reads::Native && args.aim == AimMode::Mouse && !view::Camera::lag_measured() {
            match measure_lag(game, &mut walker.reader) {
                Some(lag) => walker.camera.set_read_lag(lag),
                None => notes.push(format!(
                    "The native view read did not show a small mouse turn within {} ms, so \
                     the aim assumes reads lag {} ms.",
                    LAG_LIMIT.as_millis(),
                    walker.camera.read_lag().as_millis()
                )),
            }
        }
        let now = Instant::now();
        Ok(Self {
            walker,
            world,
            reads,
            name,
            wanted,
            radius,
            most,
            started: now,
            deadline: now + timeout,
            cursor,
            tracker: Tracker::default(),
            done: Vec::new(),
            current: None,
            keys: Keys::default(),
            last_swing: None,
            last_sprint: None,
            run_up: None,
            strafe: 1.0,
            strafed: now,
            next: now,
            flicks: 0,
            look_aims: 0,
            aim_mode: args.aim,
            flick: None,
            teleported: None,
            exact_at: None,
            empty_since: None,
            stray_hits: 0,
            others: Vec::new(),
            ended: None,
            notes,
        })
    }

    /// One tick of the fight; the outcome once it ended. On an error the keys are let go
    /// before it is returned.
    pub fn poll(&mut self, game: &mut dyn Game) -> Result<Option<Outcome>> {
        if Instant::now() < self.next {
            return Ok(None);
        }
        self.next = Instant::now() + TICK;
        match self.tick(game) {
            Ok(false) => Ok(None),
            Ok(true) => {
                self.let_go(game)?;
                Ok(Some(self.outcome()))
            }
            Err(error) => {
                let _ = self.let_go(game);
                Err(error)
            }
        }
    }

    /// One tick; true once the fight is over.
    fn tick(&mut self, game: &mut dyn Game) -> Result<bool> {
        self.drain_events(game)?;
        if self.reads == Reads::Native {
            self.tracker.poll(game, false);
            self.watch_others(game);
        }
        if self
            .current
            .as_ref()
            .is_some_and(|engaged| engaged.killed.is_some())
        {
            self.finish_current(game)?;
        }
        if Instant::now() >= self.deadline {
            self.ended = Some("timeout");
            if let Some(mut engaged) = self.current.take() {
                engaged.left = Some(("timeout", "the fight's timeout_ms ran out".to_owned()));
                self.done.push(engaged);
            }
            return Ok(true);
        }
        let pose = self.walker.reader.read(game)?;
        let Some(engaged) = &self.current else {
            if self.done.len() as u32 >= self.most {
                self.ended = Some("max_targets");
                return Ok(true);
            }
            return match self.pick(game, pose)? {
                Some(foe) => {
                    self.empty_since = None;
                    self.current = Some(Engagement::new(foe));
                    self.walker.retarget();
                    if self.reads == Reads::Native {
                        let id = self.current.as_ref().expect("just set").foe.id.clone();
                        self.tracker.track(game, &id);
                    }
                    Ok(false)
                }
                None if self.empty_since.get_or_insert_with(Instant::now).elapsed()
                    < PICK_GRACE =>
                {
                    Ok(false)
                }
                None => {
                    self.ended = Some("no_targets_left");
                    Ok(true)
                }
            };
        };
        let id = engaged.foe.id.clone();
        match self.locate(game, &id)? {
            Some(foe) => self.engage(game, pose, foe)?,
            None => self.vanished(),
        }
        Ok(false)
    }

    /// The nearest matching mob not fought yet, or `None`.
    fn pick(&mut self, game: &mut dyn Game, pose: Pose) -> Result<Option<Foe>> {
        let mut foes = match self.wanted.clone() {
            Wanted::Id(id) => self.locate(game, &id)?.into_iter().collect(),
            Wanted::Type(kind) if self.reads == Reads::Native => {
                // The native list can leave out mobs querytarget finds (seen with freshly
                // summoned ones), so both count.
                let mut foes = self.native_foes(game, Some(&kind))?;
                for foe in self.command_foes().unwrap_or_default() {
                    if !foes.iter().any(|known| known.id == foe.id) {
                        foes.push(foe);
                    }
                }
                foes
            }
            _ => self.command_foes()?,
        };
        foes.retain(|foe| !self.done.iter().any(|done| done.foe.id == foe.id));
        foes.sort_by(|a, b| flat(a.feet, pose.feet).total_cmp(&flat(b.feet, pose.feet)));
        if self.reads == Reads::Native {
            // The others' health, to count swings that hit them instead.
            self.others = foes
                .iter()
                .skip(1)
                .take(MAX_OTHERS)
                .filter_map(|foe| match game.native_entity(&foe.id) {
                    NativeRead::Read {
                        entity: Some(vitals),
                        ..
                    } => vitals.health.map(|[current, _]| (foe.id.clone(), current)),
                    _ => None,
                })
                .collect();
        }
        Ok(foes.into_iter().next())
    }

    fn native_foes(&mut self, game: &mut dyn Game, kind: Option<&str>) -> Result<Vec<Foe>> {
        Ok(game
            .native_list(self.radius, kind)?
            .iter()
            .filter_map(foe_of)
            .collect())
    }

    /// Mobs from querytarget; without the Bridge pack's or a native read's type, a family's
    /// mobs have none.
    fn command_foes(&self) -> Result<Vec<Foe>> {
        let kind = match &self.wanted {
            Wanted::Type(kind) => Some(kind.clone()),
            _ => None,
        };
        Ok(self
            .world
            .targets(&self.wanted.selector(self.radius))?
            .into_iter()
            .map(|target| Foe {
                id: target.unique_id,
                kind: kind.clone(),
                feet: target.position,
                health: None,
            })
            .collect())
    }

    /// Mob `id` now, or `None` when it is gone.
    fn locate(&mut self, game: &mut dyn Game, id: &str) -> Result<Option<Foe>> {
        match self.reads {
            Reads::Bridge => {
                match self
                    .world
                    .bridge_command(&format!("re_mcp:entity {}", quoted(id)?))
                {
                    Ok(entity) => Ok(Some(Foe {
                        id: id.to_owned(),
                        kind: entity["type"].as_str().map(str::to_owned),
                        feet: point_of(&entity["location"])
                            .ok_or_else(|| Error::msg(format!("Entity {id} has no location.")))?,
                        health: health_of(&entity["health"]),
                    })),
                    Err(error) if error.to_string().contains("No loaded entity") => Ok(None),
                    Err(error) => Err(error),
                }
            }
            Reads::Native => {
                let kind = self
                    .current
                    .as_ref()
                    .and_then(|engaged| engaged.foe.kind.clone());
                let reach = self.radius + CHASE;
                let listed = game
                    .native_list(reach, kind.as_deref())?
                    .iter()
                    .filter_map(foe_of)
                    .find(|foe| foe.id == id);
                if listed.is_some() {
                    return Ok(listed);
                }
                // Missing from the list is not gone: the read by id decides, and querytarget
                // gives the place.
                let vitals = match game.native_entity(id) {
                    NativeRead::Read { entity: None, .. } => return Ok(None),
                    NativeRead::Read {
                        entity: Some(vitals),
                        ..
                    } => Some(vitals),
                    NativeRead::Unavailable(_) => None,
                };
                let place = self
                    .world
                    .targets(&self.wanted.selector(reach))?
                    .into_iter()
                    .find(|target| target.unique_id == id);
                Ok(place.map(|target| Foe {
                    id: id.to_owned(),
                    kind: vitals
                        .as_ref()
                        .and_then(|vitals| vitals.kind.clone())
                        .or(kind),
                    feet: target.position,
                    health: vitals.and_then(|vitals| vitals.health),
                }))
            }
            Reads::Command => Ok(self
                .world
                .targets(&self.wanted.selector(self.radius + CHASE))?
                .into_iter()
                .find(|target| target.unique_id == id)
                .map(|target| Foe {
                    id: target.unique_id,
                    kind: None,
                    feet: target.position,
                    health: None,
                })),
        }
    }

    /// The Bridge pack's hurt and death events since the last poll, for the current mob.
    fn drain_events(&mut self, game: &mut dyn Game) -> Result<()> {
        let Some(cursor) = &mut self.cursor else {
            return Ok(());
        };
        let wanted = |event: &crate::events::HubEvent| {
            event.source == Source::Bridge
                && matches!(event.name.as_str(), "entity_hurt" | "entity_die")
        };
        for _ in 0..EVENT_BATCH {
            let (event, _) = game.wait_event(cursor, Duration::ZERO, &wanted)?;
            let Some(event) = event else { break };
            *cursor = event.cursor.clone();
            let Some(engaged) = &mut self.current else {
                continue;
            };
            let by_me = self.name.is_some()
                && event.body["attacker"]["name"].as_str() == self.name.as_deref();
            if event.body["entity"]["id"].as_str() != Some(engaged.foe.id.as_str()) {
                if by_me && event.name == "entity_hurt" {
                    self.stray_hits += 1;
                }
                continue;
            }
            if event.name == "entity_die" {
                let kill = Kill {
                    source: "bridge",
                    seen: "entity_die",
                    tick: event.tick,
                };
                engaged.killed.get_or_insert((kill, Instant::now()));
            } else if by_me {
                engaged.hits += 1;
            }
        }
        Ok(())
    }

    /// Read the other mobs' health while a swing may still land, and count each drop as a
    /// hit that went to them instead of the mob fought.
    fn watch_others(&mut self, game: &mut dyn Game) {
        if self.last_swing.is_none_or(|at| at.elapsed() > HIT_WINDOW) {
            return;
        }
        let mut index = 0;
        while index < self.others.len() {
            let (id, before) = self.others[index].clone();
            match game.native_entity(&id) {
                NativeRead::Read {
                    entity: Some(vitals),
                    ..
                } => {
                    if let Some([current, _]) = vitals.health {
                        if current < before {
                            self.stray_hits += 1;
                        }
                        self.others[index].1 = current;
                    }
                    index += 1;
                }
                NativeRead::Read { entity: None, .. } => {
                    self.others.remove(index);
                }
                NativeRead::Unavailable(_) => return,
            }
        }
    }

    /// The current mob was missing from a read: with native reads after a read showed it, a
    /// kill; with the Bridge pack a kill once its death event came, else after a grace a
    /// mob that went away; with commands only, gone without confirmation.
    fn vanished(&mut self) {
        let Some(engaged) = &mut self.current else {
            return;
        };
        let since = *engaged.vanished.get_or_insert_with(Instant::now);
        match self.reads {
            Reads::Native => {
                if engaged.pending.take().is_some() {
                    engaged.hits += 1;
                }
                let kill = Kill {
                    source: "native",
                    seen: "gone",
                    tick: None,
                };
                engaged.killed = Some((kill, since));
            }
            Reads::Bridge if since.elapsed() >= DEATH_GRACE => {
                engaged.left = Some((
                    "gone",
                    "the mob went away without an entity_die event: a despawn or teleport"
                        .to_owned(),
                ));
            }
            Reads::Bridge => {}
            Reads::Command => {
                let kill = Kill {
                    source: "command",
                    seen: "gone",
                    tick: None,
                };
                engaged.killed = Some((kill, since));
            }
        }
        if engaged.left.is_some() {
            let engaged = self.current.take().expect("checked above");
            self.done.push(engaged);
        }
    }

    /// Walk to the mob, or fight it when it is close: face it, keep the distance, strafe
    /// between swings, swing when its immunity wore off.
    fn engage(&mut self, game: &mut dyn Game, pose: Pose, foe: Foe) -> Result<()> {
        let engaged = self.current.as_mut().expect("engage needs a mob");
        engaged.vanished = None;
        if let Some([current, _]) = foe.health {
            // Each drop since the last read is a hit (the reads come about once a tick, and
            // the mob's hurt immunity keeps two hits at least 10 ticks apart).
            let dropped = engaged
                .foe
                .health
                .is_some_and(|[before, _]| current < before);
            if self.reads == Reads::Native && dropped && engaged.hits < engaged.swings {
                engaged.hits += 1;
                engaged.pending = None;
            } else if engaged
                .pending
                .is_some_and(|(at, _)| at.elapsed() >= HIT_WINDOW)
            {
                engaged.pending = None;
                engaged.no_damage += 1;
            }
            if current <= 0.0 && self.reads == Reads::Native {
                let kill = Kill {
                    source: "native",
                    seen: "health_zero",
                    tick: None,
                };
                engaged.killed.get_or_insert((kill, Instant::now()));
            }
        }
        if foe.kind.is_some() {
            engaged.foe.kind.clone_from(&foe.kind);
        }
        let height = body_height(engaged.foe.kind.as_deref());
        engaged.foe.feet = foe.feet;
        // A read without health (a native read that failed this tick) keeps the last known
        // one, so the next drop is still measured against it.
        if foe.health.is_some() {
            engaged.foe.health = foe.health;
        }
        let gap = flat(pose.feet, foe.feet);
        let walking = engaged.walking;
        let ready = self.last_swing.is_none_or(|at| at.elapsed() >= SWING_EVERY);
        let chasing = gap > CHASE || (walking && gap > ENGAGE);
        // Where this tick's time goes: walking to the mob, a due swing waiting for its aim,
        // or the swing cadence.
        let now = Instant::now();
        let spent = engaged
            .last_tick
            .replace(now)
            .map_or(Duration::ZERO, |at| now.duration_since(at));
        let phase = if chasing {
            0
        } else if ready {
            1
        } else {
            2
        };
        engaged.phases[phase] += spent;
        engaged.ticks += 1;
        if chasing {
            return self.chase(game, pose, foe);
        }
        if walking {
            self.walker.release(game)?;
            self.current.as_mut().expect("engaged").walking = false;
        }
        // Aim at the torso where the latest read shows the mob.
        let aim = [foe.feet[0], foe.feet[1] + height * AIM_SHARE, foe.feet[2]];
        let (yaw, pitch) = facing(pose.eye(), aim);
        // Between swings the view follows the mob in eased steps. While a flick waits for the
        // reads to show it, or right after an exact aim the reads do not show yet, steps from
        // those reads would turn the view away again.
        let exact_recent = self.exact_at.is_some_and(|at| at.elapsed() < EXACT_QUIET);
        if self.flick.is_none() && !exact_recent {
            self.walker
                .camera
                .step(game, &self.world, pose.yaw, pose.pitch, (yaw, pitch))?;
        }
        if let Some(since) = self.run_up {
            if gap < KEEP_OFF {
                // Too close for a run-up: back off and swing as usual.
                self.run_up = None;
            } else {
                if (since.elapsed() >= RUN_UP || gap <= RUN_UP_FROM)
                    && self.swing(game, pose, &foe, (yaw, pitch), true)?
                {
                    self.run_up = None;
                }
                return Ok(());
            }
        }
        let sprint_ready = self
            .last_sprint
            .is_none_or(|at| at.elapsed() >= SPRINT_EVERY);
        if ready && sprint_ready && (RUN_UP_FROM..=SWING_RANGE).contains(&gap) {
            self.run_up = Some(Instant::now());
            return self.set_keys(
                game,
                Keys {
                    forward: 1.0,
                    strafe: 0.0,
                    sprint: true,
                    sneak: false,
                },
            );
        }
        if ready && gap <= SWING_RANGE && self.swing(game, pose, &foe, (yaw, pitch), false)? {
            return Ok(());
        }
        if self.strafed.elapsed() >= STRAFE_FLIP {
            self.strafe = -self.strafe;
            self.strafed = Instant::now();
        }
        let keys = self.on_ground(pose, forward_for(gap));
        self.set_keys(game, keys)
    }

    /// Walk toward the mob along a path.
    fn chase(&mut self, game: &mut dyn Game, pose: Pose, foe: Foe) -> Result<()> {
        let engaged = self.current.as_mut().expect("chase needs a mob");
        if !engaged.walking {
            engaged.walking = true;
            self.run_up = None;
            self.set_keys(game, Keys::default())?;
        }
        self.walker.set_radius(ENGAGE);
        let why = match self.walker.step(game, pose, foe.feet)? {
            None => return Ok(()),
            Some(Walked::Arrived) => {
                self.walker.release(game)?;
                self.current.as_mut().expect("engaged").walking = false;
                return Ok(());
            }
            Some(Walked::Blocked(why)) => why,
            Some(Walked::Liquid(id, [x, y, z])) => format!("{id} at {x} {y} {z} is in the way"),
        };
        self.walker.release(game)?;
        let mut engaged = self.current.take().expect("engaged");
        engaged.walking = false;
        engaged.left = Some(("blocked", format!("the mob could not be reached: {why}")));
        self.done.push(engaged);
        Ok(())
    }

    /// Click key.attack at the mob once the view is on it: the read view within the body's
    /// angle of the aim (see [`on_aim`]). When it is off, the Bridge pack's exact aim turns
    /// it and the click follows if its new sight shows the mob; without the pack a mouse
    /// flick turns it (see [`Fighter::mouse_aim`]). False when no click went out.
    fn swing(
        &mut self,
        game: &mut dyn Game,
        pose: Pose,
        foe: &Foe,
        aim: (f64, f64),
        sprint: bool,
    ) -> Result<bool> {
        if pose.yaw.is_nan() {
            return Ok(false);
        }
        let pitch_read = pose.pitch.unwrap_or(aim.1);
        let aimed = match (self.reads, self.aim_mode) {
            (Reads::Bridge, _) => self.pack_aim(pose, pitch_read, foe, aim)?,
            (_, AimMode::Mouse) => self.mouse_aim(game, pose, pitch_read, foe, aim)?,
            (_, AimMode::Tp) => self.tp_aim(pose, pitch_read, foe, aim)?,
        };
        if !aimed {
            return Ok(false);
        }
        game.input(InputRequest {
            press: vec!["key.attack".to_owned()],
            ..InputRequest::default()
        })?;
        let now = Instant::now();
        self.last_swing = Some(now);
        let engaged = self.current.as_mut().expect("swing needs a mob");
        engaged.swings += 1;
        if sprint {
            engaged.sprint_swings += 1;
            self.last_sprint = Some(now);
        }
        if self.reads == Reads::Native
            && let Some([current, _]) = foe.health
        {
            // The swing before saw no drop by now: it did no damage.
            if engaged.pending.is_some() {
                engaged.no_damage += 1;
            }
            engaged.pending = Some((now, current));
        }
        self.strafe = -self.strafe;
        self.strafed = now;
        let keys = self.on_ground(pose, forward_for(flat(pose.feet, foe.feet)));
        self.set_keys(game, keys)?;
        Ok(true)
    }

    /// With the Bridge pack: the read view is current, so an angle check against the aim
    /// decides. When it is off, the pack's exact aim turns the view on the server (no chat),
    /// and the click follows once the client took the turn, as with /tp.
    fn pack_aim(&mut self, pose: Pose, pitch: f64, foe: &Foe, aim: (f64, f64)) -> Result<bool> {
        if let Some(settled) = self.settled() {
            return Ok(settled);
        }
        if on_aim(pose.yaw, pitch, aim, flat(pose.feet, foe.feet)) {
            return Ok(true);
        }
        let spec = format!("entity={}", foe.id);
        self.world
            .bridge_write(&format!("re_mcp:aim {}", quoted(&spec)?))?;
        self.aimed_exactly(aim.1);
        Ok(false)
    }

    /// After an exact aim: `Some(true)` once the client had [`TP_SETTLE`] to take the turn,
    /// `Some(false)` before, `None` without one in flight.
    fn settled(&mut self) -> Option<bool> {
        let at = self.teleported?;
        if at.elapsed() < TP_SETTLE {
            return Some(false);
        }
        self.teleported = None;
        Some(true)
    }

    /// An exact aim (/tp or the pack's) to `pitch` went out: the click waits for it, and the
    /// eased steps leave the view alone until the reads show it.
    fn aimed_exactly(&mut self, pitch: f64) {
        self.walker.camera.snapped(pitch);
        self.look_aims += 1;
        self.teleported = Some(Instant::now());
        self.exact_at = self.teleported;
    }

    /// Without the pack, with `aim: "tp"`: when the read view is off the aim, /tp turns it to
    /// the torso on the server, and the click follows once the client took the turn.
    fn tp_aim(&mut self, pose: Pose, pitch: f64, foe: &Foe, aim: (f64, f64)) -> Result<bool> {
        if let Some(settled) = self.settled() {
            return Ok(settled);
        }
        if on_aim(pose.yaw, pitch, aim, flat(pose.feet, foe.feet)) {
            return Ok(true);
        }
        view::exact(&self.world, false, aim.0, aim.1)?;
        self.aimed_exactly(aim.1);
        Ok(false)
    }

    /// Without the pack: whether the click may go out. When the read view is off the aim, one
    /// mouse flick at the measured sensitivity turns the rest of the way; the reads are then
    /// watched until they show the view on the aim, at most [`flick_wait`] after the flick.
    /// Still off, one correction flick goes out and is watched the same way, and then the
    /// click goes out regardless, counted as `off_aim`. Only when mouse motion does not turn
    /// the view does /tp aim (it shows in chat).
    fn mouse_aim(
        &mut self,
        game: &mut dyn Game,
        pose: Pose,
        pitch: f64,
        foe: &Foe,
        aim: (f64, f64),
    ) -> Result<bool> {
        let on = on_aim(pose.yaw, pitch, aim, flat(pose.feet, foe.feet));
        let wait = flick_wait(self.walker.camera.read_lag());
        let flicked = match self.flick {
            _ if on => {
                self.flick = None;
                return Ok(true);
            }
            Some(flick) if flick.sent.elapsed() < wait => return Ok(false),
            Some(flick) if flick.count >= MAX_FLICKS => {
                self.flick = None;
                self.current.as_mut().expect("aiming at a mob").off_aim += 1;
                return Ok(true);
            }
            Some(flick) => flick.count,
            None => 0,
        };
        match self
            .walker
            .camera
            .exact_turn(game, (pose.yaw, pitch), aim)?
        {
            Exact::Sent => {
                self.flicks += 1;
                self.flick = Some(Flick {
                    sent: Instant::now(),
                    count: flicked + 1,
                });
                Ok(false)
            }
            Exact::NotYet => Ok(false),
            Exact::NoMouse => {
                view::exact(&self.world, false, aim.0, aim.1)?;
                self.walker.camera.snapped(aim.1);
                self.look_aims += 1;
                Ok(true)
            }
        }
    }

    /// The footwork keys for `forward` and the current strafe, kept on solid ground: a step
    /// that would leave the ground (off a platform's edge) strafes the other way, or stands.
    fn on_ground(&mut self, pose: Pose, forward: f64) -> Keys {
        let yaw = pose.yaw.to_radians();
        // Bedrock yaw 0 faces +z; the player's right is then -x.
        let (ahead, right) = ([-yaw.sin(), yaw.cos()], [-yaw.cos(), -yaw.sin()]);
        let lands = |walker: &mut Walker, forward: f64, strafe: f64| {
            let (x, z) = (
                ahead[0] * forward + right[0] * strafe,
                ahead[1] * forward + right[1] * strafe,
            );
            let length = x.hypot(z);
            if length == 0.0 || yaw.is_nan() {
                return true;
            }
            STEP_CHECKS.iter().all(|step| {
                let reach = step / length;
                walker.ground_at([
                    pose.feet[0] + x * reach,
                    pose.feet[1],
                    pose.feet[2] + z * reach,
                ])
            })
        };
        // With an edge behind, mobs crowding in push the player over it: lean into them, and
        // sneak while any edge is near, which keeps the player on the block.
        let back = lands(&mut self.walker, -1.0, 0.0);
        let sneak =
            !back || !lands(&mut self.walker, 0.0, 1.0) || !lands(&mut self.walker, 0.0, -1.0);
        let forward = if back { forward } else { 1.0 };
        for (forward, strafe) in [
            (forward, self.strafe),
            (forward, -self.strafe),
            (0.0, self.strafe),
            (0.0, -self.strafe),
        ] {
            if lands(&mut self.walker, forward, strafe) {
                self.strafe = if strafe == 0.0 { self.strafe } else { strafe };
                return Keys {
                    forward,
                    strafe,
                    sprint: false,
                    sneak,
                };
            }
        }
        Keys {
            sneak,
            ..Keys::default()
        }
    }

    /// Hold the movement keys `keys` asks for, sending only a change.
    fn set_keys(&mut self, game: &mut dyn Game, keys: Keys) -> Result<()> {
        if keys == self.keys {
            return Ok(());
        }
        let sprint = (keys.sprint != self.keys.sprint).then_some(keys.sprint);
        let sneak = (keys.sneak != self.keys.sneak).then_some(keys.sneak);
        self.keys = keys;
        game.input(InputRequest {
            r#move: Some(Axes {
                forward: Some(keys.forward),
                strafe: Some(keys.strafe),
            }),
            sprint,
            sneak,
            ..InputRequest::default()
        })
    }

    /// The current mob is done: killed, or left.
    fn finish_current(&mut self, game: &mut dyn Game) -> Result<()> {
        if let Some(engaged) = self.current.take() {
            if engaged.walking {
                self.walker.release(game)?;
            }
            self.done.push(engaged);
        }
        self.run_up = None;
        self.set_keys(game, Keys::default())
    }

    /// Let go of every key the fight holds.
    fn let_go(&mut self, game: &mut dyn Game) -> Result<()> {
        self.walker.release(game)?;
        self.set_keys(game, Keys::default())
    }

    fn outcome(&mut self) -> Outcome {
        let mut outcome = Outcome::new(&Actor::Me, "input");
        let counts_hits = self.reads != Reads::Command;
        let targets: Vec<Value> = self
            .done
            .iter()
            .map(|engaged| engaged.to_json(counts_hits))
            .collect();
        let kills = self
            .done
            .iter()
            .filter(|engaged| {
                engaged
                    .killed
                    .as_ref()
                    .is_some_and(|(kill, _)| kill.confirmed())
            })
            .count();
        let ended = self.ended.unwrap_or("timeout");
        outcome.source = self.reads.name();
        outcome.confirmed = !self.done.is_empty() && kills == self.done.len();
        outcome.evidence = json!({
            "targets": targets,
            "kills": kills,
            "ended": ended,
            "elapsed_ms": self.started.elapsed().as_millis() as u64,
            "flicks": self.flicks,
            "look_aims": self.look_aims,
            "stray_hits": self.stray_hits,
            "read_lag_ms": self.walker.camera.read_lag().as_millis() as u64,
            "path": self.walker.evidence(),
        });
        outcome.notes = std::mem::take(&mut self.notes);
        outcome.notes.extend(self.walker.take_notes());
        if self.look_aims > 0 {
            outcome.notes.push(match (self.reads, self.aim_mode) {
                (Reads::Bridge, _) => format!(
                    "{} swings aimed with the Bridge pack's exact aim first, which shows \
                     nothing in chat.",
                    self.look_aims
                ),
                (_, AimMode::Tp) => format!(
                    "{} swings aimed with /tp first, and each /tp printed a line in chat; \
                     aim: \"mouse\" aims quietly, though less accurately.",
                    self.look_aims
                ),
                (_, AimMode::Mouse) => format!(
                    "Mouse motion does not turn the view, so {} swings aimed with /tp first, \
                     which shows in chat.",
                    self.look_aims
                ),
            });
        }
        if !outcome.confirmed {
            outcome.reason = Some(if self.done.is_empty() {
                (
                    "no_target",
                    format!(
                        "No mob matching {} was within {} blocks.",
                        self.describe_wanted(),
                        rounded(self.radius)
                    ),
                )
            } else if let Some(left) = self.done.iter().find_map(|done| done.left.clone()) {
                left
            } else if self.reads == Reads::Command {
                (
                    "no_event",
                    "Nothing confirms the kills without the Bridge pack or the native read."
                        .to_owned(),
                )
            } else {
                (
                    "timeout",
                    "The time ran out before every mob died.".to_owned(),
                )
            });
        }
        outcome
    }

    fn describe_wanted(&self) -> String {
        match &self.wanted {
            Wanted::Id(id) => format!("id {id}"),
            Wanted::Type(kind) => format!("type {kind}"),
            Wanted::Family(family) => format!("family {family}"),
        }
    }
}

fn flat(a: [f64; 3], b: [f64; 3]) -> f64 {
    (a[0] - b[0]).hypot(a[2] - b[2])
}

fn health_of(value: &Value) -> Option<[f64; 2]> {
    Some([value["current"].as_f64()?, value["max"].as_f64()?])
}

/// A mob from a native list entry.
fn foe_of(entry: &Value) -> Option<Foe> {
    Some(Foe {
        id: entry["id"].as_str()?.to_owned(),
        kind: entry["type"].as_str().map(str::to_owned),
        feet: point_of(&entry["location"])?,
        health: health_of(&entry["health"]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flick_waits_twice_the_read_lag_at_most_400_ms() {
        assert_eq!(
            flick_wait(Duration::from_millis(140)),
            Duration::from_millis(280)
        );
        assert_eq!(flick_wait(Duration::from_millis(300)), MAX_FLICK_WAIT);
    }

    #[test]
    fn the_player_backs_off_close_mobs_and_steps_in_to_far_ones() {
        assert_eq!(forward_for(1.0), -1.0);
        assert_eq!(forward_for(2.2), 0.0);
        assert_eq!(forward_for(3.1), 1.0);
    }

    #[test]
    fn with_the_pack_a_swing_needs_the_read_view_within_the_body_angle() {
        // At 2 blocks the body spans about 11 degrees each side; never less than 5.
        assert!(on_aim(10.0, 20.0, (0.0, 20.0), 2.0));
        assert!(!on_aim(13.0, 20.0, (0.0, 20.0), 2.0));
        assert!(!on_aim(6.0, 20.0, (0.0, 20.0), 10.0));
        assert!(on_aim(4.0, 20.0, (0.0, 20.0), 10.0));
        // The pitch has 15 degrees of room, across the 180 seam too.
        assert!(!on_aim(0.0, 40.0, (0.0, 20.0), 2.0));
        assert!(on_aim(179.0, 20.0, (-179.0, 20.0), 2.0));
    }

    #[test]
    fn the_read_lag_is_the_median_of_at_least_two_samples() {
        let ms = Duration::from_millis;
        assert_eq!(median(vec![ms(90), ms(300), ms(60), ms(80)]), Some(ms(90)));
        assert_eq!(median(vec![ms(90)]), None);
    }

    #[test]
    fn the_aim_is_the_torso_of_the_mob() {
        let eye = [0.0, 1.62, 0.0];
        let height = body_height(Some("minecraft:zombie"));
        // About 1.2 of the zombie's 1.95 blocks, a little below the player's eyes: the view
        // looks slightly down.
        let torso = [0.0, height * AIM_SHARE, 2.0];
        assert!((torso[1] - 1.21).abs() < 0.01);
        let (_, pitch) = facing(eye, torso);
        assert!(pitch > 0.0 && pitch < 15.0, "{pitch}");
        assert_eq!(body_height(Some("minecraft:spider")), 0.9);
        assert_eq!(body_height(None), 1.8);
    }
}
