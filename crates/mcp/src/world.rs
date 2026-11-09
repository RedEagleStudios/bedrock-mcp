//! In-world queries and actions over the WebSocket: a player's position and rotation,
//! nearby actors, time, chunk loading, and turning a player. Commands run as the player who
//! ran /connect, so the world must allow cheats.

pub mod vanilla;

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde_json::{Value, json};

use crate::{
    bridge::Actor,
    error::{Error, Result, bail},
    ws::GameSocket,
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
pub const NEARBY_RADIUS: u32 = 32;
pub const NEARBY_LIMIT: usize = 16;
/// Pitch is read by bisecting `testfor <selector>[rxm=..,rx=..]` to this precision.
pub const PITCH_PRECISION: f64 = 0.25;
/// Entity lists kept for paging without the bridge; older cursors expire.
const KEPT_ENTITY_LISTS: usize = 8;
/// Game modes as the Script API names them, with their `m=` selector values.
const GAME_MODES: [(&str, &str); 4] = [
    ("Survival", "survival"),
    ("Creative", "creative"),
    ("Adventure", "adventure"),
    ("Spectator", "spectator"),
];

/// A command argument: at most four decimals, no trailing zeros.
pub fn number(value: f64) -> String {
    let text = format!("{value:.4}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text == "-0" {
        "0".to_owned()
    } else {
        text.to_owned()
    }
}

/// Find the pitch in [-90, 90] by bisection; `within(lo, hi)` says whether lo <= pitch <= hi.
pub fn bisect_pitch(mut within: impl FnMut(f64, f64) -> Result<bool>) -> Result<f64> {
    let (mut low, mut high) = (-90.0_f64, 90.0_f64);
    while high - low > PITCH_PRECISION {
        let middle = (low + high) / 2.0;
        if within(low, middle)? {
            high = middle;
        } else {
            low = middle;
        }
    }
    Ok(((low + high) / 2.0 * 100.0).round() / 100.0)
}

/// Wrap a yaw difference into [-180, 180).
pub fn shortest_turn(from: f64, to: f64) -> f64 {
    (to - from + 180.0).rem_euclid(360.0) - 180.0
}

/// The yaw and pitch that look from `from` at `to`, in Bedrock degrees: yaw 0 faces +Z and
/// 90 faces -X; positive pitch looks down.
pub fn facing(from: [f64; 3], to: [f64; 3]) -> (f64, f64) {
    let [dx, dy, dz] = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];
    let yaw = (-dx).atan2(dz).to_degrees();
    let pitch = -dy.atan2(dx.hypot(dz)).to_degrees();
    (yaw, pitch)
}

/// The unit view direction of a Bedrock yaw and pitch.
pub fn direction(yaw: f64, pitch: f64) -> [f64; 3] {
    let (yaw, pitch) = (yaw.to_radians(), pitch.to_radians());
    [
        -yaw.sin() * pitch.cos(),
        -pitch.sin(),
        yaw.cos() * pitch.cos(),
    ]
}

/// `tp` commands for each step of a turn. Relative turns split the delta; absolute turns
/// interpolate from `start` along the shortest way. Another player is turned through
/// `execute as .. at @s`, so `~` stays relative to them.
pub fn look_steps(
    actor: &Actor,
    yaw: f64,
    pitch: f64,
    start: Option<(f64, f64)>,
    frames: u32,
) -> Vec<String> {
    let prefix = match actor {
        Actor::Me => String::new(),
        Actor::Named(_) => format!("execute as {} at @s run ", actor.selector()),
    };
    let steps = frames.max(1);
    (1..=steps)
        .map(|step| match start {
            None => format!(
                "{prefix}tp @s ~ ~ ~ ~{} ~{}",
                number(yaw / f64::from(steps)),
                number(pitch / f64::from(steps))
            ),
            Some((start_yaw, start_pitch)) => {
                let fraction = f64::from(step) / f64::from(steps);
                let turned = start_yaw + shortest_turn(start_yaw, yaw) * fraction;
                let raised = start_pitch + (pitch - start_pitch) * fraction;
                format!("{prefix}tp @s ~ ~ ~ {} {}", number(turned), number(raised))
            }
        })
        .collect()
}

/// The Script API name of a `querytarget` dimension number.
pub fn dimension_id(dimension: &Value) -> Value {
    match dimension.as_i64() {
        Some(0) => "minecraft:overworld".into(),
        Some(1) => "minecraft:nether".into(),
        Some(2) => "minecraft:the_end".into(),
        _ => dimension.clone(),
    }
}

/// One `querytarget` result. `position` is the eye position for players.
#[derive(Clone, Debug)]
pub struct Target {
    pub id: Value,
    /// The id as the Script API spells it (`Entity.id`).
    pub unique_id: String,
    pub dimension: Value,
    pub position: [f64; 3],
    pub yaw: f64,
}

pub fn parse_targets(details: &str) -> Result<Vec<Target>> {
    let list: Vec<Value> = serde_json::from_str(details)?;
    list.into_iter()
        .map(|target| {
            let position = &target["position"];
            let axis = |name: &str| {
                position[name]
                    .as_f64()
                    .ok_or_else(|| Error::msg("querytarget returned no position"))
            };
            let unique_id = match &target["uniqueId"] {
                Value::String(id) => id.clone(),
                _ => target["id"].to_string(),
            };
            Ok(Target {
                id: target["id"].clone(),
                unique_id,
                dimension: target["dimension"].clone(),
                position: [axis("x")?, axis("y")?, axis("z")?],
                yaw: target["yRot"].as_f64().unwrap_or_default(),
            })
        })
        .collect()
}

pub fn distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(a, b)| (a - b).powi(2))
        .sum::<f64>()
        .sqrt()
}

fn rounded(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// Entity lists answered by commands, kept so cursors page one fixed list.
#[derive(Default)]
struct EntityLists {
    last: u64,
    kept: VecDeque<(u64, Arc<Vec<Value>>)>,
}

#[derive(Clone)]
pub struct World {
    ws: Arc<GameSocket>,
    /// Whether the bridge answered, for the connection with this number.
    bridge: Arc<Mutex<Option<(u64, bool)>>>,
    entity_lists: Arc<Mutex<EntityLists>>,
}

impl World {
    pub fn new(ws: Arc<GameSocket>) -> Self {
        Self {
            ws,
            bridge: Arc::default(),
            entity_lists: Arc::default(),
        }
    }

    pub fn cached_bridge(&self, connection: u64) -> Option<bool> {
        let cached = *self
            .bridge
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        cached
            .filter(|(number, _)| *number == connection)
            .map(|(_, present)| present)
    }

    pub fn cache_bridge(&self, connection: u64, present: bool) {
        *self
            .bridge
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some((connection, present));
    }

    /// Keep an entity list for paging; returns its number.
    pub fn keep_entity_list(&self, entities: Vec<Value>) -> u64 {
        let mut lists = self
            .entity_lists
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        lists.last += 1;
        let number = lists.last;
        lists.kept.push_back((number, Arc::new(entities)));
        if lists.kept.len() > KEPT_ENTITY_LISTS {
            lists.kept.pop_front();
        }
        number
    }

    /// A kept entity list, unless it expired.
    pub fn entity_list(&self, number: u64) -> Option<Arc<Vec<Value>>> {
        let lists = self
            .entity_lists
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        lists
            .kept
            .iter()
            .find(|(kept, _)| *kept == number)
            .map(|(_, list)| Arc::clone(list))
    }

    pub fn connected(&self) -> bool {
        self.ws.connected()
    }

    pub fn socket(&self) -> &GameSocket {
        &self.ws
    }

    /// Run a command and return the response body, whatever its status.
    pub fn raw(&self, command: &str) -> Result<Value> {
        if !self.ws.connected() {
            bail!("Minecraft's WebSocket is not connected; join a world first.");
        }
        self.ws.command(command, COMMAND_TIMEOUT)
    }

    /// Run a command; a nonzero status is an error carrying the game's message.
    pub fn command(&self, command: &str) -> Result<Value> {
        let body = self.raw(command)?;
        if body["statusCode"].as_i64().unwrap_or(0) != 0 {
            bail!(
                "/{}: {}",
                command.trim_start_matches('/'),
                body["statusMessage"].as_str().unwrap_or("failed")
            );
        }
        Ok(body)
    }

    /// Whether a test command (testfor, testforblock) succeeds.
    pub fn test(&self, command: &str) -> Result<bool> {
        Ok(self.raw(command)?["statusCode"].as_i64().unwrap_or(0) == 0)
    }

    /// `querytarget` results; empty when nothing matched.
    pub fn targets(&self, selector: &str) -> Result<Vec<Target>> {
        let body = self.raw(&format!("querytarget {selector}"))?;
        if body["statusCode"].as_i64().unwrap_or(0) != 0 {
            let message = body["statusMessage"].as_str().unwrap_or("failed");
            if message.contains("No targets matched") {
                return Ok(Vec::new());
            }
            bail!("/querytarget {selector}: {message}");
        }
        parse_targets(body["details"].as_str().unwrap_or("[]"))
    }

    /// The actor's `querytarget` result.
    pub fn actor_target(&self, actor: &Actor) -> Result<Target> {
        self.targets(&actor.selector())?
            .into_iter()
            .next()
            .ok_or_else(|| match actor {
                Actor::Me => Error::msg("The player is not in a world."),
                Actor::Named(name) => {
                    Error::msg(format!("No player named {name:?} is in the world."))
                }
            })
    }

    /// The actor's pitch by bisecting `testfor` rotation ranges.
    pub fn pitch_of(&self, actor: &Actor) -> Result<f64> {
        let selector = actor.selector();
        let open = match selector.strip_suffix(']') {
            Some(open) => format!("{open},"),
            None => format!("{selector}["),
        };
        bisect_pitch(|low, high| {
            self.test(&format!(
                "testfor {open}rxm={},rx={}]",
                number(low),
                number(high)
            ))
        })
    }

    /// The actor's game mode by testing `m=` for each, as the Script API names it.
    pub fn game_mode_of(&self, actor: &Actor) -> Result<Option<&'static str>> {
        let selector = actor.selector();
        let open = match selector.strip_suffix(']') {
            Some(open) => format!("{open},"),
            None => format!("{selector}["),
        };
        for (name, mode) in GAME_MODES {
            if self.test(&format!("testfor {open}m={mode}]"))? {
                return Ok(Some(name));
            }
        }
        Ok(None)
    }

    /// Entities within `radius` of `centre` (excluding it) from querytarget, nearest first, with
    /// display names from `testfor` (which lists the same matches in the same order).
    pub fn entities_around(
        &self,
        centre: &Target,
        radius: f64,
        entity_type: Option<&str>,
    ) -> Result<Vec<Value>> {
        let [x, y, z] = centre.position.map(number);
        let filter = entity_type
            .map(|kind| format!(",type={kind}"))
            .unwrap_or_default();
        let selector = format!("@e[x={x},y={y},z={z},r={}{filter}]", number(radius));
        let targets = self.targets(&selector)?;
        let names = self.raw(&format!("testfor {selector}"))?["victim"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut found: Vec<(f64, Value)> = targets
            .iter()
            .enumerate()
            .filter(|(_, target)| target.unique_id != centre.unique_id)
            .map(|(index, target)| {
                let distance = distance(target.position, centre.position);
                let entity = json!({
                    "id": target.unique_id,
                    "type": entity_type,
                    "name": names.get(index),
                    "dimension": dimension_id(&target.dimension),
                    "location": target.position,
                    "distance": rounded(distance),
                });
                (distance, entity)
            })
            .collect();
        found.sort_by(|a, b| a.0.total_cmp(&b.0));
        Ok(found.into_iter().map(|(_, entity)| entity).collect())
    }

    /// Position, rotation, dimension, time and nearby actors: the bridge's `re_mcp:state`,
    /// `re_mcp:world` and `re_mcp:entities` when it is active, else querytarget and bisection.
    pub fn snapshot(&self) -> Result<Value> {
        if self.bridge(true)? {
            return self.bridge_snapshot();
        }
        let me = self.actor_target(&Actor::Me)?;
        let time = |query: &str| {
            self.command(&format!("time query {query}"))
                .map(|body| body["data"].clone())
        };
        let mut nearby = self.entities_around(&me, f64::from(NEARBY_RADIUS), None)?;
        nearby.truncate(NEARBY_LIMIT);
        Ok(json!({
            "id": me.id,
            "dimension": me.dimension,
            "eye_position": me.position,
            "yaw": rounded(me.yaw),
            "pitch": self.pitch_of(&Actor::Me)?,
            "time_of_day": time("daytime")?,
            "game_time": time("gametime")?,
            "nearby": nearby,
        }))
    }

    /// Turn the player who ran /connect; see [`World::look_as`].
    pub fn look(
        &self,
        yaw: f64,
        pitch: f64,
        relative: bool,
        frames: u32,
        wait_frame: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        self.look_as(&Actor::Me, yaw, pitch, relative, frames, wait_frame)
    }

    /// Turn a player: exactly with the bridge's `re_mcp:look` when it is active, else with
    /// /tp (cheats); snapping or over `frames` steps with `wait_frame` between them.
    pub fn look_as(
        &self,
        actor: &Actor,
        yaw: f64,
        pitch: f64,
        relative: bool,
        frames: u32,
        wait_frame: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        if self.bridge(false)? {
            return self.bridge_look(actor, yaw, pitch, relative, frames, wait_frame);
        }
        let start = if relative {
            None
        } else {
            Some((self.actor_target(actor)?.yaw, self.pitch_of(actor)?))
        };
        let steps = look_steps(actor, yaw, pitch, start, frames);
        let count = steps.len();
        for (index, step) in steps.iter().enumerate() {
            self.command(step)?;
            if index + 1 < count {
                wait_frame()?;
            }
        }
        Ok(())
    }

    /// Whether every chunk column within `radius` chunks of the player is loaded.
    pub fn chunks_loaded(&self, radius: u32) -> Result<bool> {
        let me = self.actor_target(&Actor::Me)?;
        let [x, y, z] = me.position.map(|value| value.floor() as i64);
        let radius = i64::from(radius);
        for dx in -radius..=radius {
            for dz in -radius..=radius {
                let body = self.raw(&format!(
                    "testforblock {} {y} {} air",
                    x + 16 * dx,
                    z + 16 * dz
                ))?;
                if body["statusMessage"]
                    .as_str()
                    .is_some_and(|message| message.contains("outside of the world"))
                {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// Whether an entity of type `matches` is within `radius` blocks.
    pub fn entity_near(&self, matches: &str, radius: f64) -> Result<bool> {
        self.test(&format!("testfor @e[type={matches},r={}]", number(radius)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_short_and_never_negative_zero() {
        assert_eq!(number(1.5), "1.5");
        assert_eq!(number(-90.0), "-90");
        assert_eq!(number(0.000_01), "0");
        assert_eq!(number(-0.000_01), "0");
        assert_eq!(number(12.345_678), "12.3457");
    }

    #[test]
    fn pitch_bisection_converges_to_a_quarter_degree() {
        for actual in [-90.0, -37.3, 0.0, 12.6, 45.0, 89.9, 90.0] {
            let mut tests = 0;
            let found = bisect_pitch(|low, high| {
                tests += 1;
                Ok(low <= actual && actual <= high)
            })
            .unwrap();
            assert!(
                (found - actual).abs() <= PITCH_PRECISION,
                "{actual} -> {found}"
            );
            assert!(tests <= 10);
        }
    }

    #[test]
    fn turns_take_the_short_way_and_split_over_frames() {
        assert_eq!(shortest_turn(170.0, -170.0), 20.0);
        assert_eq!(shortest_turn(-170.0, 170.0), -20.0);
        assert_eq!(
            look_steps(&Actor::Me, 10.0, -5.0, Some((0.0, 0.0)), 0),
            ["tp @s ~ ~ ~ 10 -5"]
        );
        assert_eq!(
            look_steps(&Actor::Me, -170.0, 10.0, Some((170.0, 0.0)), 2),
            ["tp @s ~ ~ ~ 180 5", "tp @s ~ ~ ~ 190 10"]
        );
        assert_eq!(
            look_steps(&Actor::Me, 30.0, -10.0, None, 2),
            ["tp @s ~ ~ ~ ~15 ~-5", "tp @s ~ ~ ~ ~15 ~-5"]
        );
        assert_eq!(
            look_steps(&Actor::Named("Bot 1".into()), 0.0, 0.0, Some((0.0, 0.0)), 1),
            ["execute as @a[name=\"Bot 1\"] at @s run tp @s ~ ~ ~ 0 0"]
        );
    }

    #[test]
    fn facing_and_direction_agree_with_bedrock_axes() {
        let close =
            |a: (f64, f64), b: (f64, f64)| (a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9;
        assert!(close(facing([0.0; 3], [0.0, 0.0, 5.0]), (0.0, 0.0)));
        assert!(close(facing([0.0; 3], [-5.0, 0.0, 0.0]), (90.0, 0.0)));
        assert_eq!(facing([0.0; 3], [0.0, -5.0, 0.0]).1, 90.0);
        for (yaw, pitch) in [(0.0, 0.0), (90.0, 30.0), (-135.0, -45.0)] {
            let [x, y, z] = direction(yaw, pitch);
            let back = facing([0.0; 3], [x, y, z]);
            assert!(close(back, (yaw, pitch)), "{yaw} {pitch} -> {back:?}");
        }
    }

    #[test]
    fn querytarget_details_parse() {
        let targets = parse_targets(
            r#"[{"dimension":0,"id":-4294967295,"position":{"x":1.5,"y":65.62,"z":-3},"uniqueId":"-4294967295","yRot":-90.5}]"#,
        )
        .unwrap();
        assert_eq!(targets[0].position, [1.5, 65.62, -3.0]);
        assert_eq!(targets[0].yaw, -90.5);
        assert_eq!(targets[0].unique_id, "-4294967295");
        assert_eq!(dimension_id(&targets[0].dimension), "minecraft:overworld");
        assert!(parse_targets(r#"[{"id":1}]"#).is_err());
    }
}
