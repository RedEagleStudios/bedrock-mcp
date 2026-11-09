//! Walking the connected player to a point along a path. The walker plans with
//! [`path::find`] over cells from the best [`terrain`] source, then follows the path's
//! waypoints: the [`Camera`] turns the view toward the next one in eased steps, key.forward
//! walks while the view roughly faces it, and key.jump climbs a ledge. It plans again when
//! the goal moves, the player leaves the path or gets stuck, up to [`MAX_REPLANS`] times.
//! Without a block source it walks straight at the goal and says so.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{
    Game,
    path::{self, Bound, Goal, Grid, Limits, Plan},
    point_of, rounded,
    terrain::{BridgeBlocks, CommandBlocks},
    view::Camera,
};
use crate::{
    bridge::Actor,
    error::{Error, Result},
    input::{InputRequest, Movement as Axes},
    world::{World, facing},
};

/// Eye height of a standing player.
pub const EYE_HEIGHT: f64 = 1.62;
/// How far around the start a path may go, in blocks.
const PATH_RADIUS: i64 = 32;
/// The most spots one search expands: with the Bridge pack's cheap reads, and with test
/// commands, where every new box costs 64 commands.
const BRIDGE_NODES: usize = 6000;
const COMMAND_NODES: usize = 2500;
/// How long one search may take with each source.
const BRIDGE_PLAN_TIME: Duration = Duration::from_millis(1500);
const COMMAND_PLAN_TIME: Duration = Duration::from_secs(6);
/// The most plans after the first.
pub const MAX_REPLANS: u32 = 6;
/// How close the feet must come to a waypoint, horizontally, to head for the next one.
const WAYPOINT_REACHED: f64 = 0.45;
/// How far off its path the player may get before it plans again.
const OFF_PATH: f64 = 2.0;
/// How far, in degrees, the view may still be off the way for key.forward to stay down;
/// beyond it the player stops and turns first, as a person would.
const WALK_ANGLE: f64 = 60.0;
/// How close to a ledge, horizontally, the player jumps.
const JUMP_DISTANCE: f64 = 1.6;
/// How long key.jump stays down for one jump.
const JUMP_HOLD: Duration = Duration::from_millis(250);
/// Walking that moves less than [`STUCK_MOVE`] blocks in this long is stuck.
const STUCK_WINDOW: Duration = Duration::from_millis(900);
const STUCK_MOVE: f64 = 0.3;
/// The view's pitch while walking stays within this, in degrees.
const WALK_PITCH: f64 = 30.0;
/// How far ahead walking without a block source looks for water or lava, in blocks.
const LIQUID_LOOKAHEAD: f64 = 0.8;

/// Where the player stands and looks.
#[derive(Clone, Copy, Debug)]
pub struct Pose {
    pub feet: [f64; 3],
    pub yaw: f64,
    /// `None` when the read gives no pitch (querytarget).
    pub pitch: Option<f64>,
}

impl Pose {
    pub fn eye(&self) -> [f64; 3] {
        [self.feet[0], self.feet[1] + EYE_HEIGHT, self.feet[2]]
    }
}

/// Reads the connected player's pose: the Bridge pack's state, else the unstable layer's
/// native player read, else querytarget (no pitch).
pub struct PoseReader {
    world: World,
    bridge: bool,
    /// Whether the native read answered; `None` before the first try.
    native: Option<bool>,
}

impl PoseReader {
    pub fn new(world: World, bridge: bool) -> Self {
        Self {
            world,
            bridge,
            native: None,
        }
    }

    /// Where the reads come from: bridge, native or command.
    pub fn source(&self) -> &'static str {
        match (self.bridge, self.native) {
            (true, _) => "bridge",
            (false, Some(true)) => "native",
            (false, _) => "command",
        }
    }

    pub fn read(&mut self, game: &mut dyn Game) -> Result<Pose> {
        if self.bridge {
            let state = self.world.bridge_command("re_mcp:state")?;
            let feet = point_of(&state["location"])
                .ok_or_else(|| Error::msg("re_mcp:state has no location."))?;
            return Ok(Pose {
                feet,
                yaw: state["yaw"].as_f64().unwrap_or(f64::NAN),
                pitch: state["pitch"].as_f64(),
            });
        }
        if self.native != Some(false) {
            match game.native_player() {
                Ok(player) => {
                    self.native = Some(true);
                    let [x, y, z] = player.entity.position.map(f64::from);
                    let [pitch, yaw] = player
                        .entity
                        .rotation
                        .map_or([f64::NAN; 2], |angles| angles.map(f64::from));
                    return Ok(Pose {
                        feet: [x, y, z],
                        yaw,
                        pitch: Some(pitch).filter(|pitch| !pitch.is_nan()),
                    });
                }
                Err(_) => self.native = Some(false),
            }
        }
        let me = self.world.actor_target(&Actor::Me)?;
        let [x, y, z] = me.position;
        Ok(Pose {
            feet: [x, y - EYE_HEIGHT, z],
            yaw: me.yaw,
            pitch: None,
        })
    }
}

/// Where cells come from now.
enum Terrain {
    /// Not chosen yet: the first plan tries the best source.
    Unread,
    Grid(Grid<Box<dyn path::Blocks>>, &'static str),
    /// No source answered: walk straight at the goal.
    Straight,
}

/// Why walking ended before the goal.
pub enum Walked {
    Arrived,
    /// No way on: the note says why.
    Blocked(String),
    /// Water or lava one step ahead while walking straight: its id and block.
    Liquid(String, [i64; 3]),
}

/// What the walker asks of a walk.
#[derive(Clone, Copy, Debug)]
pub struct Gait {
    /// Horizontal distance to the goal that counts as arrived.
    pub radius: f64,
    /// Whether paths may cross water; without a block source, whether to walk into it.
    pub water: bool,
    pub sprint: bool,
    /// key.jump stays held for the whole walk (the caller's jump), so the walker leaves it.
    pub jump_held: bool,
}

/// Follows a path to a goal; see the module docs.
pub struct Walker {
    world: World,
    bridge: bool,
    gait: Gait,
    terrain: Terrain,
    pub reader: PoseReader,
    pub camera: Camera,
    /// Waypoints of the current plan, as points at the middle of their blocks' floors.
    waypoints: Vec<[f64; 3]>,
    /// Where the current plan starts, and its search result.
    plan: Option<([f64; 3], Plan, Vec<[i64; 3]>)>,
    next: usize,
    /// The goal the current plan was made for.
    planned_for: Option<[f64; 3]>,
    /// Plans after the first, for any reason.
    pub replans: u32,
    /// Time spent planning, all plans together.
    planning: Duration,
    /// Plans made because the player got stuck or left its path; [`MAX_REPLANS`] caps them.
    retries: u32,
    /// Whether key.forward is down.
    walking: bool,
    jump_until: Option<Instant>,
    /// When and where the player last moved by [`STUCK_MOVE`].
    progress: Option<(Instant, [f64; 3])>,
    stuck: u32,
    pub notes: Vec<String>,
    pub last: Option<Pose>,
}

/// Horizontal distance.
fn flat(a: [f64; 3], b: [f64; 3]) -> f64 {
    (a[0] - b[0]).hypot(a[2] - b[2])
}

/// How far along the segment from `a` to `b` the point `at` lies, horizontally: 0 at `a`, 1
/// at `b`, more past it.
fn along(at: [f64; 3], a: [f64; 3], b: [f64; 3]) -> f64 {
    let (dx, dz) = (b[0] - a[0], b[2] - a[2]);
    let length = dx * dx + dz * dz;
    if length == 0.0 {
        return 1.0;
    }
    ((at[0] - a[0]) * dx + (at[2] - a[2]) * dz) / length
}

/// Horizontal distance from `at` to the segment from `a` to `b`.
fn off_segment(at: [f64; 3], a: [f64; 3], b: [f64; 3]) -> f64 {
    let share = along(at, a, b).clamp(0.0, 1.0);
    flat(
        at,
        [
            a[0] + (b[0] - a[0]) * share,
            at[1],
            a[2] + (b[2] - a[2]) * share,
        ],
    )
}

impl Walker {
    pub fn new(world: World, bridge: bool, gait: Gait) -> Self {
        Self {
            reader: PoseReader::new(world.clone(), bridge),
            world,
            bridge,
            gait,
            terrain: Terrain::Unread,
            camera: Camera::new(bridge),
            waypoints: Vec::new(),
            plan: None,
            next: 0,
            planned_for: None,
            replans: 0,
            planning: Duration::ZERO,
            retries: 0,
            walking: false,
            jump_until: None,
            progress: None,
            stuck: 0,
            notes: Vec::new(),
            last: None,
        }
    }

    /// Change how close counts as arrived, as a fight does between targets.
    pub fn set_radius(&mut self, radius: f64) {
        self.gait.radius = radius;
    }

    /// Whether the player can stand at `at` (a point at its feet' level): ground under it and
    /// room for its body, from the block source. True when no source answers, since nothing
    /// tells.
    pub fn ground_at(&mut self, at: [f64; 3]) -> bool {
        if matches!(self.terrain, Terrain::Unread) {
            self.terrain = self.first_source();
        }
        let Terrain::Grid(grid, _) = &mut self.terrain else {
            return true;
        };
        let cell = [
            at[0].floor() as i64,
            (at[1] + 0.05).floor() as i64,
            at[2].floor() as i64,
        ];
        path::stand(grid, cell, self.gait.water).map_or(true, |found| found.is_some())
    }

    /// Walk to a new goal from here on: the path and the retries start over, the blocks read
    /// so far and the camera stay.
    pub fn retarget(&mut self) {
        self.waypoints.clear();
        self.plan = None;
        self.next = 0;
        self.retries = 0;
        self.progress = None;
    }

    /// One step toward `goal` from `pose`: plan if needed, turn, walk, jump. `Some` once it
    /// arrived or cannot go on; the keys stay as they are until [`Walker::release`].
    pub fn step(
        &mut self,
        game: &mut dyn Game,
        pose: Pose,
        goal: [f64; 3],
    ) -> Result<Option<Walked>> {
        self.last = Some(pose);
        if flat(pose.feet, goal) <= self.gait.radius {
            return Ok(Some(Walked::Arrived));
        }
        if let Some(blocked) = self.keep_planned(pose, goal)? {
            return Ok(Some(blocked));
        }
        if matches!(self.terrain, Terrain::Straight)
            && !self.gait.water
            && self.bridge
            && let Some((id, block)) = self.liquid_ahead(pose, goal)?
        {
            return Ok(Some(Walked::Liquid(id, block)));
        }
        let aim = self.waypoints[self.next];
        let (yaw, _) = facing(pose.feet, aim);
        let look_at = [aim[0], aim[1] + EYE_HEIGHT - 0.4, aim[2]];
        let (_, pitch) = facing(pose.eye(), look_at);
        let pitch = pitch.clamp(-WALK_PITCH, WALK_PITCH);
        let left = self
            .camera
            .step(game, &self.world, pose.yaw, pose.pitch, (yaw, pitch))?;
        self.press(game, left.abs() < WALK_ANGLE || left.is_nan())?;
        self.jump(game, pose, aim)?;
        self.watch_progress(game, pose)
    }

    /// Plan when there is no plan yet, the goal moved, or the player left the path; then
    /// move on along the waypoints. `Some` when no way is left.
    fn keep_planned(&mut self, pose: Pose, goal: [f64; 3]) -> Result<Option<Walked>> {
        let moved = self
            .planned_for
            .is_some_and(|planned| flat(planned, goal) > (0.2 * flat(pose.feet, goal)).max(1.5));
        let off = self.plan.is_some()
            && off_segment(pose.feet, self.leg_start(), self.waypoints[self.next]) > OFF_PATH;
        if (self.waypoints.is_empty() || moved || off)
            && let Some(blocked) = self.plan(pose, goal, off)?
        {
            return Ok(Some(blocked));
        }
        while self.next + 1 < self.waypoints.len() && self.reached(pose) {
            self.next += 1;
            // Reaching a waypoint is progress: the retries count again from here.
            self.retries = 0;
        }
        let at_end = self.next + 1 == self.waypoints.len()
            && flat(pose.feet, self.waypoints[self.next]) < WAYPOINT_REACHED;
        let partial = self.plan.as_ref().is_some_and(|(_, plan, _)| !plan.reached);
        if at_end && partial {
            // The end of a path that stopped short: plan on from here.
            return self.plan(pose, goal, false);
        }
        if matches!(self.terrain, Terrain::Straight) {
            self.waypoints = vec![goal];
            self.next = 0;
        }
        Ok(None)
    }

    /// Where the current leg of the path starts: the plan's start or the waypoint before.
    fn leg_start(&self) -> [f64; 3] {
        match (self.next, &self.plan) {
            (0, Some((start, _, _))) => *start,
            (0, None) => self.waypoints[0],
            (next, _) => self.waypoints[next - 1],
        }
    }

    /// Whether the player is done with the current waypoint: close to it, or past it along
    /// its leg on its level (a jump or a turn can carry the player beyond it, and walking back
    /// would look odd).
    fn reached(&self, pose: Pose) -> bool {
        let point = self.waypoints[self.next];
        flat(pose.feet, point) < WAYPOINT_REACHED
            || (along(pose.feet, self.leg_start(), point) >= 1.0
                && (pose.feet[1] - point[1]).abs() < 0.6)
    }

    /// Plan from `pose` to `goal`, choosing the block source on the first plan; `retry` when
    /// the last plan failed the player (stuck, off the path). `Some` when no way leads on.
    fn plan(&mut self, pose: Pose, goal: [f64; 3], retry: bool) -> Result<Option<Walked>> {
        self.next = 0;
        if matches!(self.terrain, Terrain::Unread) {
            self.terrain = self.first_source();
        }
        if matches!(self.terrain, Terrain::Straight) {
            self.planned_for = Some(goal);
            self.waypoints = vec![goal];
            return Ok(None);
        }
        if retry {
            if self.retries >= MAX_REPLANS {
                return Ok(Some(Walked::Blocked(format!(
                    "the path was planned again {MAX_REPLANS} times and the player still did \
                     not get through"
                ))));
            }
            self.retries += 1;
        }
        if self.planned_for.is_some() {
            self.replans += 1;
        }
        self.planned_for = Some(goal);
        let start = [
            pose.feet[0].floor() as i64,
            (pose.feet[1] + 0.05).floor() as i64,
            pose.feet[2].floor() as i64,
        ];
        let target = Goal {
            at: goal,
            radius: self.gait.radius.max(0.5),
        };
        let found = loop {
            let Terrain::Grid(grid, source) = &mut self.terrain else {
                self.waypoints = vec![goal];
                self.plan = None;
                return Ok(None);
            };
            let source: &'static str = source;
            let commands = source == "commands";
            let limits = Limits {
                radius: PATH_RADIUS,
                max_nodes: if commands {
                    COMMAND_NODES
                } else {
                    BRIDGE_NODES
                },
                deadline: Instant::now()
                    + if commands {
                        COMMAND_PLAN_TIME
                    } else {
                        BRIDGE_PLAN_TIME
                    },
                water: self.gait.water,
            };
            let began = Instant::now();
            let water = self.gait.water;
            let found = path::find(grid, start, target, limits).and_then(|plan| {
                let route = path::waypoints(grid, &plan.cells, water)?;
                Ok((plan, route))
            });
            self.planning += began.elapsed();
            match found {
                Ok(found) => break found,
                Err(error) => {
                    let next = if commands {
                        Terrain::Straight
                    } else {
                        self.command_source()
                    };
                    self.notes.push(format!(
                        "Reading blocks with {source} failed ({error}), so {}.",
                        match next {
                            Terrain::Straight => "the player walks straight at the goal",
                            _ => "test commands read them instead",
                        }
                    ));
                    self.terrain = next;
                }
            }
        };
        let (found, route) = found;
        if found.cells.len() < 2 && !found.reached {
            let why = match found.bound {
                None => "every way from here is walled off within the search area".to_owned(),
                Some(bound) => format!("the search hit its {} bound", bound.name()),
            };
            self.plan = Some((pose.feet, found, route));
            return Ok(Some(Walked::Blocked(format!(
                "no path leads closer: {why}"
            ))));
        }
        self.waypoints = route
            .iter()
            .map(|[x, y, z]| [*x as f64 + 0.5, *y as f64, *z as f64 + 0.5])
            .collect();
        if self.waypoints.is_empty() {
            self.waypoints = vec![goal];
        }
        self.plan = Some((pose.feet, found, route));
        Ok(None)
    }

    /// The best block source: the Bridge pack's grid, else test commands.
    fn first_source(&self) -> Terrain {
        if self.bridge {
            Terrain::Grid(
                Grid::new(Box::new(BridgeBlocks {
                    world: self.world.clone(),
                })),
                "bridge",
            )
        } else {
            self.command_source()
        }
    }

    fn command_source(&self) -> Terrain {
        Terrain::Grid(
            Grid::new(Box::new(CommandBlocks {
                world: self.world.clone(),
            })),
            "commands",
        )
    }

    /// Hold or release key.forward (with key.sprint when asked).
    fn press(&mut self, game: &mut dyn Game, walk: bool) -> Result<()> {
        if walk == self.walking {
            return Ok(());
        }
        self.walking = walk;
        if walk {
            self.progress = None;
        }
        game.input(InputRequest {
            r#move: Some(Axes {
                forward: Some(if walk { 1.0 } else { 0.0 }),
                strafe: Some(0.0),
            }),
            sprint: self.gait.sprint.then_some(walk),
            ..InputRequest::default()
        })
    }

    /// Jump at a ledge ahead, and let go of key.jump once the jump is under way.
    fn jump(&mut self, game: &mut dyn Game, pose: Pose, aim: [f64; 3]) -> Result<()> {
        if self.gait.jump_held {
            return Ok(());
        }
        match self.jump_until {
            Some(until) if Instant::now() >= until => {
                game.input(InputRequest {
                    jump: Some(false),
                    ..InputRequest::default()
                })?;
                self.jump_until = None;
            }
            None if aim[1] > pose.feet[1] + 0.5 && flat(pose.feet, aim) < JUMP_DISTANCE => {
                self.hop(game)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn hop(&mut self, game: &mut dyn Game) -> Result<()> {
        game.input(InputRequest {
            jump: Some(true),
            ..InputRequest::default()
        })?;
        self.jump_until = Some(Instant::now() + JUMP_HOLD);
        Ok(())
    }

    /// Notice a walk that stopped moving: jump once, then plan again with the blocks around
    /// the player read afresh. Without a block source a stuck walk ends.
    fn watch_progress(&mut self, game: &mut dyn Game, pose: Pose) -> Result<Option<Walked>> {
        if !self.walking {
            self.progress = None;
            return Ok(None);
        }
        let Some((since, at)) = self.progress else {
            self.progress = Some((Instant::now(), pose.feet));
            return Ok(None);
        };
        if flat(at, pose.feet) >= STUCK_MOVE || (pose.feet[1] - at[1]).abs() >= STUCK_MOVE {
            self.progress = Some((Instant::now(), pose.feet));
            return Ok(None);
        }
        if since.elapsed() < STUCK_WINDOW {
            return Ok(None);
        }
        self.progress = Some((Instant::now(), pose.feet));
        self.stuck += 1;
        if self.stuck % 2 == 1 && self.jump_until.is_none() && !self.gait.jump_held {
            return self.hop(game).map(|()| None);
        }
        match &mut self.terrain {
            Terrain::Grid(grid, _) => {
                grid.forget_near(pose.feet.map(|axis| axis.floor() as i64), 2);
                self.notes.push(format!(
                    "The player got stuck near {} {} {}, so the path was planned again.",
                    rounded(pose.feet[0]),
                    rounded(pose.feet[1]),
                    rounded(pose.feet[2])
                ));
                let goal = self.planned_for.unwrap_or(pose.feet);
                self.plan(pose, goal, true)
            }
            _ => Ok(Some(Walked::Blocked(
                "the player stopped moving while walking straight at the goal".to_owned(),
            ))),
        }
    }

    /// Let go of the keys the walk holds.
    pub fn release(&mut self, game: &mut dyn Game) -> Result<()> {
        if self.walking || self.jump_until.is_some() {
            game.input(InputRequest {
                r#move: Some(Axes {
                    forward: Some(0.0),
                    strafe: Some(0.0),
                }),
                sprint: self.gait.sprint.then_some(false),
                jump: (self.jump_until.is_some() && !self.gait.jump_held).then_some(false),
                ..InputRequest::default()
            })?;
        }
        self.walking = false;
        self.jump_until = None;
        Ok(())
    }

    /// The water or lava block one step ahead of `pose` toward `goal`, at the feet or (over
    /// air) just below them, with its block coordinates; needs the Bridge pack.
    fn liquid_ahead(&self, pose: Pose, goal: [f64; 3]) -> Result<Option<(String, [i64; 3])>> {
        let (yaw, _) = facing(pose.feet, goal);
        let radians = yaw.to_radians();
        let (x, z) = (
            (pose.feet[0] - radians.sin() * LIQUID_LOOKAHEAD).floor() as i64,
            (pose.feet[2] + radians.cos() * LIQUID_LOOKAHEAD).floor() as i64,
        );
        let feet = pose.feet[1].floor() as i64;
        for y in [feet, feet - 1] {
            let block = self
                .world
                .bridge_command(&format!("re_mcp:block {x} {y} {z}"))?;
            let id = block["id"].as_str().unwrap_or_default();
            if id.ends_with("water") || id.ends_with("lava") {
                let state = self.world.bridge_command("re_mcp:state")?;
                if state["flags"]["inWater"] == Value::Bool(true) {
                    return Ok(None);
                }
                return Ok(Some((id.to_owned(), [x, y, z])));
            }
            if block["air"] != Value::Bool(true) {
                return Ok(None);
            }
        }
        Ok(None)
    }

    /// The path for the reply: {source, waypoints, length, reached, bound, expanded, replans}
    /// and the camera.
    pub fn evidence(&self) -> Value {
        let source = match &self.terrain {
            Terrain::Grid(_, source) => *source,
            Terrain::Unread | Terrain::Straight => "none",
        };
        let mut path = json!({
            "source": source,
            "replans": self.replans,
            "plan_ms": self.planning.as_millis() as u64,
            "camera": self.camera.to_json(),
        });
        if let Some((_, plan, route)) = &self.plan {
            path["waypoints"] = json!(route);
            path["length"] = rounded(plan.length()).into();
            path["reached"] = plan.reached.into();
            path["bound"] = plan.bound.map(Bound::name).into();
            path["expanded"] = plan.expanded.into();
        }
        if let Terrain::Grid(grid, _) = &self.terrain {
            path["loads"] = grid.loads.into();
        }
        path
    }

    /// Notes on how the walk went: the source fallbacks and the camera's.
    pub fn take_notes(&mut self) -> Vec<String> {
        let mut notes = std::mem::take(&mut self.notes);
        if matches!(self.terrain, Terrain::Straight) {
            notes.push(
                "No block source answered (neither the Bridge pack's re_mcp:blocks nor \
                 testforblock), so the player walked straight at the goal without a path."
                    .to_owned(),
            );
        }
        notes.append(&mut self.camera.notes);
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_to_a_segment_is_flat_and_clamped() {
        let (a, b) = ([0.0, 0.0, 0.0], [0.0, 0.0, 10.0]);
        assert_eq!(off_segment([3.0, 5.0, 5.0], a, b), 3.0);
        assert_eq!(off_segment([0.0, 0.0, -4.0], a, b), 4.0);
        assert_eq!(off_segment([0.0, 0.0, 14.0], a, b), 4.0);
    }
}
