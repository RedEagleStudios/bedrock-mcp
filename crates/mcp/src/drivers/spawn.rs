//! Summon an entity relative to an actor and find its id. With the Bridge pack: the pack's
//! entity_spawn event after the /summon, or the nearest entity of that type at the spot when
//! no event came. Without it (cheats on): the entity of that type at the spot that
//! `querytarget` lists after the /summon and did not list before it.

use std::{
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Expect, Game, Outcome, TICK, confirm_after, first, point_of, rounded};
use crate::{
    bridge::{Actor, quoted},
    error::{Error, Result, bail},
    events::HubEvent,
    gametest::Vec3,
    world::{Target, World, distance, number},
};

/// Eye height of a standing player: `querytarget` reports a player's eye position.
const EYE_HEIGHT: f64 = 1.62;

/// How far from the requested spot the spawned entity may stand when it is matched.
const MATCH_RADIUS: f64 = 2.0;
/// The longest effect, in seconds.
const MAX_EFFECT_SECONDS: u32 = 1_000_000;
/// The default effect length, in seconds.
const DEFAULT_EFFECT_SECONDS: u32 = 600;
/// Blocks that a spawned mob can stand in: plants, thin and flat blocks.
const PASSABLE: [&str; 24] = [
    "short_grass",
    "tall_grass",
    "tallgrass",
    "fern",
    "flower",
    "tulip",
    "dandelion",
    "poppy",
    "orchid",
    "allium",
    "bluet",
    "daisy",
    "lily_of_the_valley",
    "sapling",
    "dead_bush",
    "deadbush",
    "torch",
    "snow_layer",
    "carpet",
    "rail",
    "button",
    "pressure_plate",
    "vine",
    "seagrass",
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Effect {
    id: String,
    seconds: Option<u32>,
    amplifier: Option<u8>,
    #[serde(default)]
    hide: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spawn {
    #[serde(default)]
    pub(super) actor: Actor,
    #[serde(rename = "type")]
    kind: String,
    ahead: Option<f64>,
    right: Option<f64>,
    up: Option<f64>,
    position: Option<Vec3>,
    name: Option<String>,
    #[serde(default)]
    effects: Vec<Effect>,
}

/// A namespaced id: `zombie` is `minecraft:zombie`.
fn namespaced(id: &str) -> String {
    if id.contains(':') {
        id.to_owned()
    } else {
        format!("minecraft:{id}")
    }
}

/// Whether `id` may go into a command unquoted.
fn plain_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_:.-".contains(c))
}

impl Spawn {
    pub(super) fn check(&self) -> Result<()> {
        if !plain_id(&self.kind) {
            bail!("type must be an entity type id such as minecraft:zombie.");
        }
        let offsets = self.ahead.is_some() || self.right.is_some() || self.up.is_some();
        if offsets && self.position.is_some() {
            bail!("Give position, or ahead, right and up, not both.");
        }
        if let Some(name) = &self.name {
            quoted(name)?;
        }
        for effect in &self.effects {
            if !plain_id(&effect.id) {
                bail!("effect id must be an effect id such as slowness.");
            }
            if effect
                .seconds
                .is_some_and(|seconds| seconds > MAX_EFFECT_SECONDS)
            {
                bail!("effect seconds is at most {MAX_EFFECT_SECONDS}.");
            }
        }
        Ok(())
    }
}

/// Whether a spawned mob can stand in the block `id`.
fn passable(block: &Value) -> bool {
    if block["air"] == true || block["liquid"] == true {
        return true;
    }
    let id = block["id"].as_str().unwrap_or_default();
    let name = id.split(':').next_back().unwrap_or(id);
    PASSABLE.iter().any(|kind| name.contains(kind)) && !name.ends_with("_block")
}

/// Run `first` on another thread while `second` runs here; both results.
fn together<A: Send, B>(first: impl FnOnce() -> A + Send, second: impl FnOnce() -> B) -> (A, B) {
    thread::scope(|scope| {
        let first = scope.spawn(first);
        let second = second();
        (first.join().expect("a game command does not panic"), second)
    })
}

/// Where to summon: `position`, or `ahead`, `right` and `up` blocks from feet `feet` facing
/// `yaw` degrees.
fn spot_of(args: &Spawn, feet: [f64; 3], yaw: f64) -> [f64; 3] {
    if let Some(at) = &args.position {
        return [at.x, at.y, at.z].map(rounded);
    }
    let yaw = yaw.to_radians();
    let (ahead, right, up) = (
        args.ahead.unwrap_or(0.0),
        args.right.unwrap_or(0.0),
        args.up.unwrap_or(0.0),
    );
    // Bedrock yaw 0 faces +z; the right hand then points to -x.
    [
        feet[0] - yaw.sin() * ahead - yaw.cos() * right,
        feet[1] + up,
        feet[2] + yaw.cos() * ahead - yaw.sin() * right,
    ]
    .map(rounded)
}

/// The /summon command for the spot.
fn summon_command(args: &Spawn, kind: &str, spot: [f64; 3]) -> Result<String> {
    let name = match &args.name {
        Some(name) => format!(" {}", quoted(name)?),
        None => String::new(),
    };
    let [x, y, z] = spot.map(number);
    Ok(format!("summon {kind}{name} {x} {y} {z}"))
}

/// Summon `args.kind` at the spot, find its id, apply the effects and report whether the
/// spot has a clear line to the actor's eyes.
pub fn spawn(game: &mut dyn Game, args: &Spawn, confirm: Duration) -> Result<Outcome> {
    args.check()?;
    let world = game.world()?;
    if !world.bridge(false)? {
        return spawn_by_commands(&world, args, confirm);
    }
    let kind = namespaced(&args.kind);
    let mut outcome = Outcome::new(&args.actor, "command");
    // Each command takes a game tick or two to answer, so commands that do not depend on each
    // other go out together.
    let state_command = format!("re_mcp:state{}", args.actor.argument());
    let (state, cursor) = together(|| world.bridge_command(&state_command), || game.cursor());
    let (state, cursor) = (state?, cursor?);
    let feet =
        point_of(&state["location"]).ok_or_else(|| Error::msg("re_mcp:state has no location."))?;
    let eye = point_of(&state["eye"]).unwrap_or([feet[0], feet[1] + EYE_HEIGHT, feet[2]]);
    let spot = spot_of(args, feet, state["yaw"].as_f64().unwrap_or(0.0));
    outcome.evidence["requested_at"] = json!({ "x": spot[0], "y": spot[1], "z": spot[2] });
    // The mob stands in the block at its feet and the one above.
    let cell = |dy: f64| [spot[0], spot[1] + dy, spot[2]].map(|axis| axis.floor() as i64);
    let read = |[x, y, z]: [i64; 3]| world.bridge_command(&format!("re_mcp:block {x} {y} {z}"));
    let (feet_block, head_block) = together(|| read(cell(0.0)), || read(cell(1.0)));
    for (part, at, block) in [
        ("feet", cell(0.0), feet_block),
        ("head", cell(1.0), head_block),
    ] {
        let [x, y, z] = at;
        let block = match block {
            Ok(block) => block,
            Err(error) if error.to_string().contains("not loaded") => bail!(
                "The spot {x} {y} {z} is outside the loaded chunks, so nothing was summoned. \
                 Spawn closer to the player."
            ),
            Err(error) => return Err(error),
        };
        if !passable(&block) {
            bail!(
                "The spot is inside a solid block ({} at {x} {y} {z}, the entity's {part}), so \
                 nothing was summoned. Pick another spot, for example with up: 1.",
                block["id"].as_str().unwrap_or("unknown")
            );
        }
    }
    let middle = [spot[0], spot[1] + 1.0, spot[2]];
    let gap = distance(eye, middle);
    let along = [0, 1, 2].map(|axis| (middle[axis] - eye[axis]) / gap.max(f64::EPSILON));
    let sight = format!(
        "re_mcp:raycast {}{}",
        quoted(&format!(
            "x={};y={};z={};dx={};dy={};dz={};max={};entities=0",
            number(eye[0]),
            number(eye[1]),
            number(eye[2]),
            number(along[0]),
            number(along[1]),
            number(along[2]),
            number(gap)
        ))?,
        args.actor.argument()
    );
    let summon = summon_command(args, &kind, spot)?;
    let (ray, summoned) = together(|| world.bridge_command(&sight), || world.command(&summon));
    summoned?;
    let ray = ray?;
    outcome.evidence["line_of_sight"] = match ray["block"]["id"].as_str() {
        Some(id) => json!({
            "clear": false,
            "blocked_by": { "id": id, "location": ray["block"]["location"] },
        }),
        None => json!({ "clear": true }),
    };
    let wanted = kind.clone();
    let near_spot = move |event: &HubEvent| {
        event.body["entity"]["type"].as_str() == Some(wanted.as_str())
            && point_of(&event.body["entity"]["location"])
                .is_some_and(|at| distance(at, spot) <= MATCH_RADIUS)
    };
    let expect = Expect {
        bridge: &["entity_spawn"],
        ws: &[],
        player: None,
        matches: &near_spot,
    };
    // The effects go out while the spawn event is read: the entity stands at the spot by now.
    let effects = || apply_effects(&world, args, &kind, spot);
    let (applied, found) = together(effects, || {
        confirm_after(game, true, &cursor, confirm, confirm, &expect, &first)
    });
    let found = found?;
    if let Some(applied) = applied? {
        outcome.evidence["effects"] = applied;
    }
    let (id, at) = match found.events.first() {
        Some(event) => {
            outcome.tick = event.tick;
            outcome.evidence["matched_by"] = "entity_spawn".into();
            (
                event.body["entity"]["id"].as_str().map(str::to_owned),
                point_of(&event.body["entity"]["location"]).unwrap_or(spot),
            )
        }
        None => {
            outcome.notes.extend(found.notes);
            // No event: take the nearest entity of the type at the spot.
            let listed = world.bridge_command(&format!(
                "re_mcp:entities 64 {kind} 64{}",
                args.actor.argument()
            ))?;
            let nearest = listed["entities"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|entity| Some((distance(point_of(&entity["location"])?, spot), entity)))
                .filter(|(gap, _)| *gap <= MATCH_RADIUS)
                .min_by(|a, b| a.0.total_cmp(&b.0));
            match nearest {
                Some((_, entity)) => {
                    outcome.evidence["matched_by"] = "nearest".into();
                    outcome.notes.push(
                        "No entity_spawn event named it, so this is the nearest entity of the \
                         type at the spot; it may be an older one."
                            .to_owned(),
                    );
                    (
                        entity["id"].as_str().map(str::to_owned),
                        point_of(&entity["location"]).unwrap_or(spot),
                    )
                }
                None => (None, spot),
            }
        }
    };
    let Some(id) = id else {
        outcome.reason = Some(("no_event", format!("No {kind} appeared at the spot.")));
        outcome
            .notes
            .push(format!("/summon ran, but no {kind} appeared at the spot."));
        return Ok(outcome);
    };
    outcome.confirmed = true;
    outcome.source = "bridge";
    outcome.evidence["id"] = id.as_str().into();
    outcome.evidence["type"] = kind.as_str().into();
    outcome.evidence["location"] = json!({ "x": at[0], "y": at[1], "z": at[2] });
    Ok(outcome)
}

/// The target nearest `spot` among `targets`.
fn nearest<'a>(targets: impl Iterator<Item = &'a Target>, spot: [f64; 3]) -> Option<&'a Target> {
    targets
        .map(|target| (distance(target.position, spot), target))
        .filter(|(gap, _)| *gap <= MATCH_RADIUS)
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, target)| target)
}

/// [`spawn`] without the Bridge pack, through vanilla commands (cheats): the actor's place
/// from `querytarget`, then the /summon, then the entity of the type at the spot that
/// `querytarget` did not list before it, looked for until `confirm` passes.
fn spawn_by_commands(world: &World, args: &Spawn, confirm: Duration) -> Result<Outcome> {
    let kind = namespaced(&args.kind);
    let mut outcome = Outcome::new(&args.actor, "command");
    let actor = world.actor_target(&args.actor).map_err(|error| {
        Error::msg(format!(
            "spawn needs the Bridge pack or a world with cheats (querytarget, /summon): {error}"
        ))
    })?;
    let [x, y, z] = actor.position;
    let spot = spot_of(args, [x, y - EYE_HEIGHT, z], actor.yaw);
    outcome.evidence["requested_at"] = json!({ "x": spot[0], "y": spot[1], "z": spot[2] });
    let [sx, sy, sz] = spot.map(number);
    let selector = format!(
        "@e[type={kind},x={sx},y={sy},z={sz},r={}]",
        number(MATCH_RADIUS)
    );
    let before: Vec<String> = world
        .targets(&selector)?
        .into_iter()
        .map(|target| target.unique_id)
        .collect();
    world.command(&summon_command(args, &kind, spot)?)?;
    if let Some(applied) = apply_effects(world, args, &kind, spot)? {
        outcome.evidence["effects"] = applied;
    }
    outcome.evidence["line_of_sight"] = json!({ "clear": null });
    outcome.notes.push(
        "The Bridge pack is not active, so line_of_sight is unknown (it needs the pack's \
         raycast) and the spot was not checked for solid blocks."
            .to_owned(),
    );
    let deadline = Instant::now() + confirm;
    let after = loop {
        let after = world.targets(&selector)?;
        let fresh = after
            .iter()
            .any(|target| !before.contains(&target.unique_id));
        if fresh || Instant::now() >= deadline {
            break after;
        }
        thread::sleep(TICK);
    };
    let fresh = nearest(
        after
            .iter()
            .filter(|target| !before.contains(&target.unique_id)),
        spot,
    );
    let found = match fresh {
        Some(target) => {
            outcome.evidence["matched_by"] = "querytarget".into();
            Some(target)
        }
        None => {
            let older = nearest(after.iter(), spot);
            if older.is_some() {
                outcome.evidence["matched_by"] = "nearest".into();
                outcome.notes.push(format!(
                    "querytarget listed no new {kind} at the spot, so this is the nearest one \
                     there; it was there before the /summon."
                ));
            }
            older
        }
    };
    let Some(target) = found else {
        outcome.reason = Some(("no_event", format!("No {kind} appeared at the spot.")));
        outcome.notes.push(format!(
            "/summon ran, but querytarget found no {kind} at the spot within {} ms.",
            confirm.as_millis()
        ));
        return Ok(outcome);
    };
    let [ax, ay, az] = target.position;
    outcome.confirmed = true;
    outcome.evidence["id"] = target.unique_id.as_str().into();
    outcome.evidence["type"] = kind.as_str().into();
    outcome.evidence["location"] = json!({ "x": ax, "y": ay, "z": az });
    Ok(outcome)
}

/// Give the entity just summoned at `at` its effects; `None` without any. Vanilla selectors
/// cannot name an entity id, so each picks the nearest entity of the type at the spot, which
/// is the new one unless another stands as close.
fn apply_effects(world: &World, args: &Spawn, kind: &str, at: [f64; 3]) -> Result<Option<Value>> {
    if args.effects.is_empty() {
        return Ok(None);
    }
    let [x, y, z] = at.map(number);
    let named = match &args.name {
        Some(name) => format!(",name={}", quoted(name)?),
        None => String::new(),
    };
    let selector = format!("@e[type={kind},x={x},y={y},z={z},r={MATCH_RADIUS},c=1{named}]");
    let applied: Vec<Value> = thread::scope(|scope| {
        let jobs: Vec<_> = args
            .effects
            .iter()
            .map(|effect| {
                let id = effect.id.trim_start_matches("minecraft:").to_owned();
                let command = format!(
                    "effect {selector} {id} {} {} {}",
                    effect.seconds.unwrap_or(DEFAULT_EFFECT_SECONDS),
                    effect.amplifier.unwrap_or(0),
                    effect.hide
                );
                scope.spawn(move || match world.command(&command) {
                    Ok(_) => json!({ "id": id, "applied": true }),
                    Err(error) => json!({ "id": id, "applied": false, "error": error.to_string() }),
                })
            })
            .collect();
        jobs.into_iter()
            .map(|job| job.join().expect("an effect command does not panic"))
            .collect()
    });
    Ok(Some(applied.into()))
}
