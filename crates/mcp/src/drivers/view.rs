//! A human camera for walking and fighting: the view turns toward where the player goes in
//! eased steps with a capped turn rate, sent as raw mouse motion, so the view never snaps and
//! a walking player keeps its speed (a look command is a teleport, which stops a walk for a
//! moment). Aiming that must be exact keeps using the exact look.
//!
//! How many mouse counts make a degree depends on the player's sensitivity, so the first turn
//! sends a small fixed motion and measures the result; later cameras reuse the measure. When
//! the view does not move, the camera falls back to exact looks and says so.

use std::{
    collections::VecDeque,
    sync::Mutex,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use super::{Game, rounded};
use crate::{
    bridge::Actor,
    error::Result,
    input::{InputRequest, MouseMotion},
    world::{World, number, shortest_turn},
};

/// How fast a camera turns: the share of the turn left that one step takes, and the largest
/// turn per step in degrees.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pace {
    ease: f64,
    cap: f64,
}

impl Pace {
    /// Walking: about 85% of a turn is done after four steps, at most 15 degrees a step (300
    /// degrees a second with a step each tick), and the motion slows as it lands.
    pub const WALK: Self = Self {
        ease: 0.4,
        cap: 15.0,
    };
}

/// How long a read may not show a mouse turn yet, until a fight measures it: the game turns on
/// its next frame, sends the view to its server on a tick, and the read answers on a later
/// one.
const DEFAULT_READ_LAG: Duration = Duration::from_millis(150);
const BRIDGE_READ_LAG: Duration = Duration::from_millis(50);
/// Turns smaller than this are left alone, in degrees, so the view does not jitter.
const DEAD_ZONE: f64 = 0.5;
/// The motion the first turn sends to measure the sensitivity, in counts.
const PROBE_COUNTS: f64 = 60.0;
/// A probe that turned the view less than this, in degrees, shows the mouse does not turn it.
const PROBE_MIN_TURN: f64 = 0.3;
/// A probe that turned the view more than this, in degrees, measured something else (a look
/// command, a hit), so it goes out again.
const PROBE_MAX_TURN: f64 = 45.0;
/// How many reads after the probe may still show the old view: the game applies motion on its
/// next frame, and a read can come before that.
const PROBE_READS: u8 = 3;
/// How many probes may measure something else before the camera gives up on the mouse.
const PROBE_TRIES: u8 = 3;
/// How far off the camera without mouse turning lets the view get before an exact look.
const LOOK_TOLERANCE: f64 = 15.0;

/// The counts per degree the last probe measured: a player's sensitivity rarely changes, so
/// later cameras start turning at once instead of probing.
static MEASURED: Mutex<Option<f64>> = Mutex::new(None);
/// The read lag the last measure found; the same game answers alike, so later cameras use it.
static MEASURED_LAG: Mutex<Option<Duration>> = Mutex::new(None);

/// The next eased turn from `current` toward `target` (yaw and pitch in degrees) at `pace`:
/// a share of what is left, at most the pace's cap on each axis, along the short way round.
pub fn eased(current: (f64, f64), target: (f64, f64), pace: Pace) -> (f64, f64) {
    let step = |left: f64| {
        if left.abs() < DEAD_ZONE {
            0.0
        } else {
            (left * pace.ease).clamp(-pace.cap, pace.cap)
        }
    };
    (
        step(shortest_turn(current.0, target.0)),
        step(target.1 - current.1),
    )
}

/// How an exact turn went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exact {
    /// One mouse motion for the whole turn went out.
    Sent,
    /// The sensitivity is not measured yet: wait for the eased turns to measure it.
    NotYet,
    /// Mouse motion does not turn the view, so only a look command can.
    NoMouse,
}

/// Turn the connected player exactly: the Bridge pack's `re_mcp:look`, else one /tp in place
/// (a snap, which needs cheats and shows in chat).
pub fn exact(world: &World, bridge: bool, yaw: f64, pitch: f64) -> Result<()> {
    if bridge {
        return world.look_as(&Actor::Me, yaw, pitch, false, 0, &mut || Ok(()));
    }
    world
        .command(&format!("tp @s ~ ~ ~ {} {}", number(yaw), number(pitch)))
        .map(drop)
}

/// How the camera turns the view.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    /// Not measured yet; `tries` probes measured something else.
    Unknown { tries: u8 },
    /// The probe went out when the view was at this yaw; `reads` came back since.
    Probing {
        yaw: f64,
        sign: f64,
        reads: u8,
        tries: u8,
    },
    /// Mouse motion, at this many counts per degree.
    Mouse(f64),
    /// Exact looks (a teleport each), because mouse motion does not turn the view.
    Look,
}

/// Turns the connected player's view in eased steps.
pub struct Camera {
    mode: Mode,
    bridge: bool,
    pace: Pace,
    /// Mouse turns sent within the last read lag (when, yaw, pitch): the reads may not show
    /// them yet, so the view now is the read plus these.
    in_flight: VecDeque<(Instant, f64, f64)>,
    read_lag: Duration,
    /// The pitch the camera last set, for when the read gives none (querytarget).
    pitch: Option<f64>,
    pub notes: Vec<String>,
}

impl Camera {
    pub fn new(bridge: bool) -> Self {
        let measured = *MEASURED.lock().unwrap_or_else(|poison| poison.into_inner());
        Self {
            mode: measured.map_or(Mode::Unknown { tries: 0 }, Mode::Mouse),
            bridge,
            pace: Pace::WALK,
            in_flight: VecDeque::new(),
            // The pack's reads come from a command on the next tick: about one tick behind.
            read_lag: if bridge {
                BRIDGE_READ_LAG
            } else {
                MEASURED_LAG
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .unwrap_or(DEFAULT_READ_LAG)
            },
            pitch: None,
            notes: Vec::new(),
        }
    }

    /// A camera whose mouse turns `per_degree` counts a degree, as if a probe measured it,
    /// with the default read lag.
    #[cfg(test)]
    pub fn measured(per_degree: f64) -> Self {
        Self {
            mode: Mode::Mouse(per_degree),
            read_lag: DEFAULT_READ_LAG,
            ..Self::new(false)
        }
    }

    /// One step toward `target` from the view the player has now (`pitch` `None` when the
    /// read gives none). Returns the yaw left to turn after this step, so callers can wait
    /// to walk until the view roughly faces the way.
    pub fn step(
        &mut self,
        game: &mut dyn Game,
        world: &World,
        yaw: f64,
        pitch: Option<f64>,
        target: (f64, f64),
    ) -> Result<f64> {
        if yaw.is_nan() {
            // Without the player's yaw nothing tells which way to turn.
            return Ok(f64::NAN);
        }
        let pitch = pitch.or(self.pitch).unwrap_or(target.1);
        let (yaw, pitch) = self.predicted(yaw, pitch);
        let left = shortest_turn(yaw, target.0);
        match self.mode {
            Mode::Unknown { tries } => {
                if left.abs() < DEAD_ZONE {
                    return Ok(left);
                }
                let sign = left.signum();
                self.mouse(game, sign * PROBE_COUNTS, 0.0)?;
                self.mode = Mode::Probing {
                    yaw,
                    sign,
                    reads: 0,
                    tries,
                };
                Ok(left)
            }
            Mode::Probing {
                yaw: before,
                sign,
                reads,
                tries,
            } => {
                let turned = shortest_turn(before, yaw) * sign;
                if turned < PROBE_MIN_TURN && reads + 1 < PROBE_READS {
                    self.mode = Mode::Probing {
                        yaw: before,
                        sign,
                        reads: reads + 1,
                        tries,
                    };
                    return Ok(left);
                }
                let measured_else = turned > PROBE_MAX_TURN;
                if measured_else && tries + 1 < PROBE_TRIES {
                    self.mode = Mode::Unknown { tries: tries + 1 };
                    return Ok(left);
                }
                if turned < PROBE_MIN_TURN || measured_else {
                    self.mode = Mode::Look;
                    self.notes.push(format!(
                        "Mouse motion did not turn the view measurably ({PROBE_COUNTS} counts \
                         turned it {} degrees), so the camera turned with exact looks, which \
                         snap and slow a walk for a moment.",
                        rounded(turned)
                    ));
                    return self.step(game, world, yaw, Some(pitch), target);
                }
                let per_degree = PROBE_COUNTS / turned;
                *MEASURED.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(per_degree);
                self.mode = Mode::Mouse(per_degree);
                self.step(game, world, yaw, Some(pitch), target)
            }
            Mode::Mouse(per_degree) => {
                let (turn_yaw, turn_pitch) = eased((yaw, pitch), target, self.pace);
                if turn_yaw != 0.0 || turn_pitch != 0.0 {
                    self.turn(game, per_degree, turn_yaw, turn_pitch)?;
                    self.pitch = Some(pitch + turn_pitch);
                }
                Ok(left - turn_yaw)
            }
            Mode::Look => {
                if left.abs() > LOOK_TOLERANCE || (target.1 - pitch).abs() > LOOK_TOLERANCE {
                    exact(world, self.bridge, target.0, target.1)?;
                    self.pitch = Some(target.1);
                    return Ok(0.0);
                }
                Ok(left)
            }
        }
    }

    /// Something else turned the view exactly to `pitch` (an aim before a click): a probe in
    /// flight would measure that turn, so it starts over.
    pub fn snapped(&mut self, pitch: f64) {
        self.pitch = Some(pitch);
        if let Mode::Probing { tries, .. } = self.mode {
            self.mode = Mode::Unknown { tries };
        }
    }

    /// Turn the whole way from the view read as `now` (yaw, pitch) to `target` with one
    /// mouse motion at the measured sensitivity: the exact aim of a player who flicks onto a
    /// mob, without a look command. Turns still in flight count, so a correction right after
    /// it turns only what is left.
    pub fn exact_turn(
        &mut self,
        game: &mut dyn Game,
        now: (f64, f64),
        target: (f64, f64),
    ) -> Result<Exact> {
        let per_degree = match self.mode {
            Mode::Mouse(per_degree) => per_degree,
            Mode::Look => return Ok(Exact::NoMouse),
            Mode::Unknown { .. } | Mode::Probing { .. } => return Ok(Exact::NotYet),
        };
        let from = self.predicted(now.0, now.1);
        let (yaw, pitch) = (shortest_turn(from.0, target.0), target.1 - from.1);
        self.turn(game, per_degree, yaw, pitch)?;
        self.pitch = Some(target.1);
        Ok(Exact::Sent)
    }

    /// How long a read may not show a mouse turn yet.
    pub fn read_lag(&self) -> Duration {
        self.read_lag
    }

    /// Whether the read lag was measured in this session.
    pub fn lag_measured() -> bool {
        MEASURED_LAG
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .is_some()
    }

    /// Use `lag`, as measured, from now on, here and in later cameras.
    pub fn set_read_lag(&mut self, lag: Duration) {
        self.read_lag = lag;
        *MEASURED_LAG
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(lag);
    }

    /// The view now, from a read of `yaw` and `pitch`: the read plus the turns sent since
    /// that the read may not show yet.
    pub fn predicted(&mut self, yaw: f64, pitch: f64) -> (f64, f64) {
        let now = Instant::now();
        let lag = self.read_lag;
        self.in_flight
            .retain(|(at, _, _)| now.duration_since(*at) < lag);
        self.in_flight
            .iter()
            .fold((yaw, pitch), |(yaw, pitch), (_, turn_yaw, turn_pitch)| {
                (yaw + turn_yaw, (pitch + turn_pitch).clamp(-90.0, 90.0))
            })
    }

    /// Send a turn of `yaw` and `pitch` degrees and remember it until reads show it.
    fn turn(&mut self, game: &mut dyn Game, per_degree: f64, yaw: f64, pitch: f64) -> Result<()> {
        self.mouse(game, yaw * per_degree, pitch * per_degree)?;
        self.in_flight.push_back((Instant::now(), yaw, pitch));
        Ok(())
    }

    fn mouse(&self, game: &mut dyn Game, x: f64, y: f64) -> Result<()> {
        game.input(InputRequest {
            mouse: Some(MouseMotion { x, y }),
            ..InputRequest::default()
        })
    }

    /// How the camera turned, for the reply.
    pub fn to_json(&self) -> Value {
        match self.mode {
            Mode::Mouse(per_degree) => {
                json!({ "turns": "mouse", "counts_per_degree": rounded(per_degree) })
            }
            Mode::Look => json!({ "turns": "look" }),
            Mode::Unknown { .. } | Mode::Probing { .. } => json!({ "turns": "none" }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turns_ease_in_capped_steps_the_short_way() {
        let mut view = (170.0, 0.0);
        let target = (-150.0, 20.0);
        let mut steps = Vec::new();
        for _ in 0..20 {
            let (yaw, pitch) = eased(view, target, Pace::WALK);
            if yaw == 0.0 && pitch == 0.0 {
                break;
            }
            assert!(yaw.abs() <= Pace::WALK.cap && pitch.abs() <= Pace::WALK.cap);
            // The short way from 170 to -150 is +40, across 180.
            assert!(yaw >= 0.0);
            steps.push(yaw);
            view = (view.0 + yaw, view.1 + pitch);
        }
        assert!(steps.len() >= 4, "{steps:?}");
        assert!(steps.windows(2).all(|pair| pair[1] <= pair[0]), "{steps:?}");
        assert!(shortest_turn(view.0, target.0).abs() < 1.0);
        assert!((view.1 - target.1).abs() < 1.0);

        // A big turn is capped, then slows as it lands.
        let (first, _) = eased((0.0, 0.0), (120.0, 0.0), Pace::WALK);
        assert_eq!(first, Pace::WALK.cap);
        assert_eq!(eased((10.0, 5.0), (10.2, 5.1), Pace::WALK), (0.0, 0.0));
    }
}
