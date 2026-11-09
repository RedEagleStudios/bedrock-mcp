//! Turns MCP `input_send` requests into agent requests and tracks held controls.

use std::time::Duration;

use protocol::{Request, mouse};
use serde::{Deserialize, Deserializer};

use crate::{
    agent::Agent,
    clock::Clock,
    error::{Result, bail},
    keys::{self, Bindings, Control, Kind},
    world::World,
};

/// GameInput reports one wheel notch as 120 units.
pub const WHEEL_NOTCH: i64 = 120;
/// Frames to wait after opening chat before typing.
const CHAT_OPEN_FRAMES: u64 = 10;
/// Frames between typing a chat line and pressing Enter, and how long Enter stays down.
const CHAT_SUBMIT_DELAY_FRAMES: u64 = 3;
const CHAT_ENTER_FRAMES: u64 = 5;
/// How long a moved pointer hovers before a mouse button goes down.
pub const HOVER_DELAY: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Point {
    Object { x: f64, y: f64 },
    Pair([f64; 2]),
}

impl Point {
    fn xy(self) -> (f64, f64) {
        match self {
            Self::Object { x, y } => (x, y),
            Self::Pair([x, y]) => (x, y),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Movement {
    pub forward: Option<f64>,
    pub strafe: Option<f64>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WheelUnit {
    #[default]
    Line,
    Pixel,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Wheel {
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
    #[serde(default)]
    pub unit: WheelUnit,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scroll {
    #[serde(default)]
    pub x: f64,
    pub y: f64,
    #[serde(default)]
    pub pixels: bool,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Look {
    pub yaw: f64,
    pub pitch: f64,
    #[serde(default)]
    pub relative: bool,
    #[serde(default)]
    pub frames: u32,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MouseMotion {
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
}

/// Distinguishes an absent field from an explicit `null`.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputRequest {
    #[serde(default)]
    pub hold: Vec<String>,
    #[serde(default)]
    pub release: Vec<String>,
    #[serde(default)]
    pub press: Vec<String>,
    pub press_frames: Option<u64>,
    pub press_ms: Option<u64>,
    pub hold_ms: Option<u64>,
    pub r#move: Option<Movement>,
    pub jump: Option<bool>,
    pub sneak: Option<bool>,
    pub sprint: Option<bool>,
    #[serde(default, deserialize_with = "present")]
    pub pointer: Option<Option<Point>>,
    #[serde(default, deserialize_with = "present")]
    pub cursor: Option<Option<Point>>,
    pub wheel: Option<Wheel>,
    pub scroll: Option<Scroll>,
    pub hotbar: Option<u8>,
    pub text: Option<String>,
    pub look: Option<Look>,
    pub mouse: Option<MouseMotion>,
    #[serde(default)]
    pub release_all: bool,
    #[serde(default)]
    pub release_control: bool,
}

/// What applying input needs besides the agent.
pub struct InputEnv<'a> {
    pub bindings: &'a Bindings,
    /// Physical pixels per logical pixel of the game window.
    pub scale: &'a dyn Fn() -> Result<f64>,
    /// The WebSocket world, for `look`.
    pub world: Option<&'a World>,
    pub clock: &'a dyn Clock,
}

/// Wait until the game has read the mouse `frames` more times (it reads once per frame).
pub fn wait_frames(agent: &mut dyn Agent, clock: &dyn Clock, frames: u64) -> Result<()> {
    if frames == 0 {
        return Ok(());
    }
    let target = agent.status()?.mouse_readings + frames;
    let deadline = clock.now() + Duration::from_secs(2) + Duration::from_millis(100 * frames);
    while agent.status()?.mouse_readings < target {
        if clock.now() > deadline {
            bail!(
                "Minecraft stopped rendering frames; is it paused, minimised or on another workspace?"
            );
        }
        clock.sleep(Duration::from_millis(2));
    }
    Ok(())
}

/// Held controls, by name, in the order they were pressed.
#[derive(Debug, Default)]
pub struct Controller {
    held: Vec<Control>,
}

impl Controller {
    pub fn held(&self) -> Vec<&str> {
        self.held
            .iter()
            .map(|control| control.name.as_str())
            .collect()
    }

    fn buttons(&self) -> u32 {
        self.held
            .iter()
            .filter_map(|control| match control.kind {
                Kind::Button(bit) => Some(bit),
                Kind::Key(_) => None,
            })
            .fold(0, |mask, bit| mask | bit)
    }

    fn set(&mut self, agent: &mut dyn Agent, controls: &[Control], down: bool) -> Result<()> {
        let mut buttons_changed = false;
        for control in controls {
            match control.kind {
                Kind::Button(_) => buttons_changed = true,
                Kind::Key(key) => agent.send(Request::Key {
                    down,
                    vk: key.vk,
                    scan: key.scan,
                    extended: key.extended,
                })?,
            }
            self.held.retain(|held| held.name != control.name);
            if down {
                self.held.push(control.clone());
            }
        }
        if buttons_changed {
            agent.send(Request::Buttons {
                mask: self.buttons() & mouse::ALL,
            })?;
        }
        Ok(())
    }

    /// Press `controls` for `frames` frames, then release them.
    pub fn press(
        &mut self,
        agent: &mut dyn Agent,
        clock: &dyn Clock,
        controls: &[Control],
        frames: u64,
    ) -> Result<()> {
        self.set(agent, controls, true)?;
        wait_frames(agent, clock, frames)?;
        self.set(agent, controls, false)
    }

    /// Apply one request in a fixed order: release_all, pointer/cursor, release, movement,
    /// hold, hotbar, press, look, mouse, wheel/scroll, text, hold_ms, release_control.
    pub fn apply(
        &mut self,
        agent: &mut dyn Agent,
        env: &InputEnv,
        request: &InputRequest,
    ) -> Result<()> {
        let resolve = |names: &[String]| -> Result<Vec<Control>> {
            names
                .iter()
                .map(|name| keys::resolve(name, env.bindings))
                .collect()
        };
        let release = resolve(&request.release)?;
        let hold = resolve(&request.hold)?;
        let mut press = resolve(&request.press)?;
        let (mut movement_down, mut movement_up) = (Vec::new(), Vec::new());
        if let Some(movement) = request.r#move {
            for (value, positive, negative) in [
                (movement.forward, "key.forward", "key.back"),
                (movement.strafe, "key.right", "key.left"),
            ] {
                let Some(value) = value else { continue };
                let down = if value > 0.0 {
                    Some(positive)
                } else if value < 0.0 {
                    Some(negative)
                } else {
                    None
                };
                for name in [positive, negative] {
                    if down == Some(name) {
                        &mut movement_down
                    } else {
                        &mut movement_up
                    }
                    .push(name.to_owned());
                }
            }
        }
        for (flag, name) in [
            (request.jump, "key.jump"),
            (request.sneak, "key.sneak"),
            (request.sprint, "key.sprint"),
        ] {
            match flag {
                Some(true) => movement_down.push(name.to_owned()),
                Some(false) => movement_up.push(name.to_owned()),
                None => {}
            }
        }
        let (movement_down, movement_up) = (resolve(&movement_down)?, resolve(&movement_up)?);
        if let Some(slot) = request.hotbar {
            if !(1..=9).contains(&slot) {
                bail!("hotbar must be 1 to 9.");
            }
            press.extend(resolve(&[format!("key.hotbar.{slot}")])?);
        }
        if request.look.is_some() && env.world.is_none() {
            bail!(
                "look turns the player with /tp over the WebSocket; join a world with cheats, or use mouse."
            );
        }

        if request.release_all {
            self.release_all(agent)?;
        }
        if let Some(pointer) = request.pointer.or(request.cursor) {
            match pointer {
                None => agent.send(Request::PointerRelease)?,
                Some(point) => {
                    let (x, y) = point.xy();
                    let scale = (env.scale)()?;
                    agent.send(Request::Pointer {
                        x: (x * scale).round() as i64,
                        y: (y * scale).round() as i64,
                    })?;
                }
            }
            wait_frames(agent, env.clock, 1)?;
            let clicks = hold
                .iter()
                .chain(&press)
                .any(|control| matches!(control.kind, Kind::Button(_)));
            if pointer.is_some() && clicks {
                // Some screens (pause menu, world list) ignore a click before they saw the hover.
                env.clock.sleep(HOVER_DELAY);
            }
        }
        let releasing: Vec<Control> = release
            .iter()
            .chain(&movement_up)
            .filter(|control| self.held.iter().any(|held| held.name == control.name))
            .cloned()
            .collect();
        if !releasing.is_empty() {
            self.set(agent, &releasing, false)?;
        }
        let holding: Vec<Control> = hold.iter().chain(&movement_down).cloned().collect();
        if !holding.is_empty() {
            self.set(agent, &holding, true)?;
        }
        if !press.is_empty() {
            self.set(agent, &press, true)?;
            match request.press_ms {
                Some(ms) => env.clock.sleep(Duration::from_millis(ms)),
                None => wait_frames(agent, env.clock, request.press_frames.unwrap_or(1))?,
            }
            let up: Vec<Control> = press
                .into_iter()
                .filter(|control| !hold.iter().any(|held| held.name == control.name))
                .collect();
            self.set(agent, &up, false)?;
        }
        if let (Some(look), Some(world)) = (request.look, env.world) {
            let clock = env.clock;
            world.look(
                look.yaw,
                look.pitch,
                look.relative,
                look.frames,
                &mut || wait_frames(agent, clock, 1),
            )?;
        }
        if let Some(motion) = request.mouse {
            agent.send(Request::Move {
                dx: motion.x.round() as i64,
                dy: motion.y.round() as i64,
            })?;
        }
        if let Some(wheel) = request.wheel {
            self.wheel(
                agent,
                env.clock,
                wheel.x,
                wheel.y,
                wheel.unit == WheelUnit::Pixel,
            )?;
        }
        if let Some(scroll) = request.scroll {
            self.wheel(agent, env.clock, scroll.x, scroll.y, scroll.pixels)?;
        }
        if let Some(text) = &request.text {
            type_text(agent, text)?;
        }
        if let Some(ms) = request.hold_ms.filter(|_| !hold.is_empty()) {
            env.clock.sleep(Duration::from_millis(ms));
            self.set(agent, &hold, false)?;
        }
        if request.release_control {
            self.release_all(agent)?;
        }
        Ok(())
    }

    /// Scroll; positive y scrolls up. Lines are whole notches, one per frame, because the
    /// game turns each reading's wheel change into at most one notch. Raw values go out in
    /// one reading as GameInput wheel units.
    fn wheel(
        &mut self,
        agent: &mut dyn Agent,
        clock: &dyn Clock,
        x: f64,
        y: f64,
        raw: bool,
    ) -> Result<()> {
        if raw {
            agent.send(Request::Wheel {
                dx: x.round() as i64,
                dy: y.round() as i64,
            })?;
            return wait_frames(agent, clock, 1);
        }
        for (notches, horizontal) in [(x.round() as i64, true), (y.round() as i64, false)] {
            let step = WHEEL_NOTCH * notches.signum();
            for _ in 0..notches.unsigned_abs() {
                let (dx, dy) = if horizontal { (step, 0) } else { (0, step) };
                agent.send(Request::Wheel { dx, dy })?;
                wait_frames(agent, clock, 1)?;
            }
        }
        Ok(())
    }

    /// Open chat with the first `key.chat` binding, clear the stray character the chat key
    /// can leave, type `text` and send it.
    pub fn send_chat(
        &mut self,
        agent: &mut dyn Agent,
        clock: &dyn Clock,
        bindings: &Bindings,
        text: &str,
    ) -> Result<()> {
        let chat = keys::resolve("key.chat", bindings)?;
        self.press(agent, clock, &[chat], 1)?;
        wait_frames(agent, clock, CHAT_OPEN_FRAMES)?;
        let backspace = keys::resolve("Backspace", bindings)?;
        self.press(agent, clock, &[backspace], 1)?;
        type_text(agent, text)?;
        // An Enter right after typing is dropped; let the field take the text first.
        wait_frames(agent, clock, CHAT_SUBMIT_DELAY_FRAMES)?;
        let enter = keys::resolve("Enter", bindings)?;
        self.press(agent, clock, &[enter], CHAT_ENTER_FRAMES)
    }

    pub fn release_all(&mut self, agent: &mut dyn Agent) -> Result<()> {
        self.held.clear();
        agent.send(Request::Reset)
    }

    /// Forget held controls without telling the agent (its connection is gone).
    pub fn forget(&mut self) {
        self.held.clear();
    }
}

fn type_text(agent: &mut dyn Agent, text: &str) -> Result<()> {
    text.chars().try_for_each(|character| {
        agent.send(Request::Char {
            code: u32::from(character),
        })
    })
}

#[cfg(test)]
mod tests;
