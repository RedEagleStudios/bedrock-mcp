//! The `player` toolset: a player's state, inventory, looking, moving and the hotbar.
//!
//! Every tool takes an optional `actor`: "self" (the player who ran /connect) or the name of
//! another player, such as a GameTest simulated player.

use std::{thread, time::Duration};

use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    Handler, Hints, Tool, Toolset, arguments, reply,
    schema::{describe, object},
    world::entity_argument,
};
use crate::{
    bridge::{Actor, INSTALL_HINT, labelled},
    drivers,
    error::{Error, Result, bail},
    input::InputRequest,
    session::Session,
    world::{World, dimension_id, facing, number},
};

/// One game tick: the pace of multi-step turns.
const TICK: Duration = Duration::from_millis(50);

const COMMAND_NOTE: &str = "The bedrock-mcp Bridge pack is not active, so vanilla commands answered \
     (the world needs cheats): querytarget for the id, dimension, eye position and yaw, testfor \
     bisection for the pitch (0.25 degree steps), testfor m= for the game mode and tag list for \
     the tags. Location, velocity, health, hunger, selected slot, effects, flags, spawn point and \
     level need the pack.";

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Vector {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Vector {
    pub fn array(self) -> [f64; 3] {
        [self.x, self.y, self.z]
    }
}

/// The schema of a [`Vector`].
pub fn vector_schema(description: &str) -> Value {
    describe(
        object(
            json!({ "x": { "type": "number" }, "y": { "type": "number" }, "z": { "type": "number" } }),
            &["x", "y", "z"],
        ),
        description,
    )
}

pub fn actor_schema() -> Value {
    json!({
        "type": "string",
        "description": "\"self\" (default) for the player who ran /connect, or the name of another player such as a GameTest simulated player"
    })
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dimension {
    #[serde(alias = "minecraft:overworld")]
    Overworld,
    #[serde(alias = "minecraft:nether")]
    Nether,
    #[serde(alias = "minecraft:the_end")]
    TheEnd,
}

impl Dimension {
    /// The name `execute in` takes; the Script API takes it too.
    pub fn name(self) -> &'static str {
        match self {
            Self::Overworld => "overworld",
            Self::Nether => "nether",
            Self::TheEnd => "the_end",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum GameMode {
    Survival,
    Creative,
    Adventure,
    Spectator,
}

impl GameMode {
    /// The `/gamemode` name.
    fn command(self) -> &'static str {
        match self {
            Self::Survival => "survival",
            Self::Creative => "creative",
            Self::Adventure => "adventure",
            Self::Spectator => "spectator",
        }
    }

    /// The Script API `GameMode` value.
    fn script(self) -> &'static str {
        match self {
            Self::Survival => "Survival",
            Self::Creative => "Creative",
            Self::Adventure => "Adventure",
            Self::Spectator => "Spectator",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActorArgs {
    #[serde(default)]
    actor: Actor,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rotation {
    yaw: Option<f64>,
    pitch: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlayerSetArgs {
    #[serde(default)]
    actor: Actor,
    location: Option<Vector>,
    rotation: Option<Rotation>,
    dimension: Option<Dimension>,
    game_mode: Option<GameMode>,
    health: Option<f64>,
    velocity: Option<Vector>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HotbarArgs {
    slot: u8,
    #[serde(default)]
    actor: Actor,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LookAtArgs {
    #[serde(default)]
    actor: Actor,
    yaw: Option<f64>,
    pitch: Option<f64>,
    position: Option<Vector>,
    entity_id: Option<String>,
    #[serde(default)]
    frames: u32,
}

pub fn tools() -> Vec<Tool> {
    let tool = |hints: Hints, title: &'static str, run: Handler, definition| {
        Tool::new(Toolset::Player, hints, title, run, definition)
    };
    vec![
        tool(
            Hints::READ,
            "Read a player",
            |s, a| {
                let args: ActorArgs = arguments(a)?;
                reply(player_get(s, &args.actor)?)
            },
            json!({
                "name": "player_get",
                "description": concat!(
                    "Use this to check a player's position, health, effects or other state after an action.\n\n",
                    "Reads a player's state from the bedrock-mcp Bridge pack, else from the game's local server through the unstable layer, else with vanilla commands.\n\n",
                    "- The Bridge pack (source \"bridge\") fills every field. It is asked only while Minecraft's WebSocket is connected.\n",
                    "- Without it, in single player with the unstable layer's world_state capability (source \"native\", no cheats needed): name, id, dimension, location, yaw, pitch, health, hunger, saturation, selectedSlot and effects. While the WebSocket is connected, querytarget, testfor and tag list (cheats) add eye, gameMode and tags.\n",
                    "- Otherwise (source \"command\", needs cheats), querytarget, testfor and tag list give only id, dimension, eye, yaw, pitch (in 0.25 degree steps), gameMode and tags.\n",
                    "- Native reads find the player by name, or take the only player; with several players and no name they fall back to commands. Notes say which fields are missing and why.\n\n",
                    "Returns {name, id, dimension, location (feet), eye, velocity, yaw, pitch, health: {current, max}, hunger, saturation (native only), gameMode, selectedSlot, effects: [{id, amplifier, ticks}], tags, flags: {sneaking, sprinting, onGround, flying, gliding, swimming, inWater, sleeping, falling, climbing}, spawnPoint, level, source, notes}."
                ),
                "inputSchema": object(json!({ "actor": actor_schema() }), &[])
            }),
        ),
        tool(
            Hints::SET,
            "Set a player's state",
            |s, a| {
                let args: PlayerSetArgs = arguments(a)?;
                reply(player_set(&s.ensure_world()?, &args)?)
            },
            json!({
                "name": "player_set",
                "description": concat!(
                    "Use this to move a player or set its game mode, health or velocity for a test.\n\n",
                    "Writes the given fields once, then reads the player back.\n\n",
                    "- With the Bridge pack the Script API writes on the next tick: a teleport for location, rotation and dimension, setGameMode, the health component's setCurrentValue, and clearVelocity plus applyImpulse for velocity.\n",
                    "- A write the game refuses goes into rejected and notes with its reason; the rest still apply. applyImpulse can throw (Microsoft Learn's Entity reference), so velocity may land in rejected.\n",
                    "- Without the pack (needs cheats), /tp (with execute in for the dimension) and /gamemode set location, rotation, dimension and game mode; health and velocity fail.\n",
                    "- Fails when no field is given.\n\n",
                    "Returns {applied, rejected: {field: reason}, player (the player_get result)}."
                ),
                "inputSchema": object(json!({
                    "actor": actor_schema(),
                    "location": vector_schema("Feet position to teleport to"),
                    "rotation": describe(object(json!({ "yaw": { "type": "number" }, "pitch": { "type": "number", "minimum": -90, "maximum": 90 } }), &[]), "Bedrock degrees; a missing angle keeps its current value"),
                    "dimension": { "enum": ["overworld", "nether", "the_end"], "description": "Teleport into this dimension (at location, or the same coordinates)" },
                    "game_mode": { "enum": ["survival", "creative", "adventure", "spectator"], "description": "Game mode to set" },
                    "health": { "type": "number", "minimum": 0, "description": "Current health to set; needs the Bridge pack" },
                    "velocity": vector_schema("Velocity in blocks per tick; needs the Bridge pack")
                }), &[])
            }),
        ),
        tool(
            Hints::READ,
            "Read a player's inventory",
            |s, a| {
                let args: ActorArgs = arguments(a)?;
                let world = s.ensure_world()?;
                world.require_bridge()?;
                let inventory =
                    world.bridge_command(&format!("re_mcp:inventory{}", args.actor.argument()))?;
                reply(labelled("bridge", Vec::new(), inventory))
            },
            json!({
                "name": "inventory_get",
                "description": concat!(
                    "Use this to check which items a player holds after a give, pickup or craft.\n\n",
                    "Reads a player's inventory and equipment.\n\n",
                    "- Empty slots are left out.\n",
                    "- Needs the bedrock-mcp Bridge pack; without it the call fails with the install steps.\n\n",
                    "Returns {size, selectedSlot, slots: [{slot, id, amount, maxAmount, stackable, name, lore, tags, canDestroy, canPlaceOn, keepOnDeath, lockMode, components, durability, enchantable, food, cooldown, potion}], equipment: {Head, Chest, Legs, Feet, Mainhand, Offhand}}."
                ),
                "inputSchema": object(json!({ "actor": actor_schema() }), &[])
            }),
        ),
        tool(
            Hints::SET,
            "Select a hotbar slot",
            |s, a| {
                let args: HotbarArgs = arguments(a)?;
                reply(hotbar_select_session(s, args.slot, &args.actor)?)
            },
            json!({
                "name": "hotbar_select",
                "description": concat!(
                    "Use this to hold a different hotbar item; inside a fight, use a hotbar_select step in action_sequence instead.\n\n",
                    "Sets the player's selectedSlotIndex through the Bridge pack and reads the slot back.\n\n",
                    "- Without the pack, actor self presses the slot key (key.hotbar.N, as input_send {\"hotbar\": slot + 1} does) and confirms it with the native selectedSlot read (unstable world_state capability, single player; source native). A note says when the read is unavailable or shows another slot.\n",
                    "- Simulated players need the Bridge pack's GameTest; without it the call fails.\n\n",
                    "Returns {selectedSlot}, the slot read back (null when no read answered)."
                ),
                "inputSchema": object(json!({
                    "slot": { "type": "integer", "minimum": 0, "maximum": 8, "description": "Hotbar slot to select, 0 (leftmost) to 8" },
                    "actor": actor_schema()
                }), &["slot"])
            }),
        ),
        tool(
            Hints::SET,
            "Turn a player",
            |s, a| {
                let args: LookAtArgs = arguments(a)?;
                reply(look_at_actor(s, &args)?)
            },
            json!({
                "name": "look_at",
                "description": concat!(
                    "Use this to aim a player before an action that needs a direction.\n\n",
                    "Turns a player, measured from its eye, to a yaw and pitch, toward a position, or toward an entity's eyes. frames spreads the turn over that many game ticks (50 ms each) along the shortest way.\n\n",
                    "- Give exactly one of yaw and pitch, position or entity_id.\n",
                    "- Each step is re_mcp:look with the Bridge pack (a teleport in place, no chat output, no cheats), else /tp (needs cheats).\n",
                    "- Without the pack an entity is aimed at its querytarget position, which is at eye height only for players.\n",
                    "- A simulated player gets a GameTest look instead, since a teleport's pitch does not last on it. It holds until its next look, turns at its own pace and ignores frames.\n\n",
                    "Returns {requested: {yaw, pitch}, confirmed (simulated players only: the rotation read back matches), player (the player_get result read afterwards)}."
                ),
                "inputSchema": object(json!({
                    "actor": actor_schema(),
                    "yaw": { "type": "number", "description": "Bedrock degrees: 0 faces +Z (south), 90 faces -X (west); give with pitch" },
                    "pitch": { "type": "number", "minimum": -90, "maximum": 90, "description": "Positive looks down; give with yaw" },
                    "position": vector_schema("Look at this point"),
                    "entity_id": { "type": "string", "description": "Look at this entity's eyes (its id from entities_list)" },
                    "frames": { "type": "integer", "minimum": 0, "description": "Steps, one per tick; 0 or 1 snaps" }
                }), &[])
            }),
        ),
    ]
}

fn eye_of(state: &Value) -> Option<[f64; 3]> {
    let eye = &state["eye"];
    Some([eye["x"].as_f64()?, eye["y"].as_f64()?, eye["z"].as_f64()?])
}

/// `player_get`: the Bridge pack while the WebSocket is connected and the pack answers, else
/// the native read (with eye and game mode from commands while connected), else commands.
pub(super) fn player_get(session: &mut Session, actor: &Actor) -> Result<Value> {
    let world = session.world()?;
    let mut notes = Vec::new();
    if !(world.connected() && world.bridge(true)?)
        && let Some(mut state) = session.native_player_state(actor, &mut notes)
    {
        if world.connected() {
            command_fields(&world, actor, &mut state, &mut notes);
        } else {
            notes.push(
                "Minecraft's WebSocket is not connected, so the Bridge pack was not asked, and \
                 eye, gameMode and tags, which need commands, are missing."
                    .to_owned(),
            );
        }
        return Ok(labelled("native", notes, state));
    }
    let world = session.ensure_world()?;
    if world.bridge(true)? {
        return stable_player(&world, actor);
    }
    let mut state = stable_player(&world, actor)?;
    if let Some(command_notes) = state["notes"].as_array_mut() {
        command_notes.extend(notes.into_iter().map(Value::from));
    }
    Ok(state)
}

/// Adds eye, gameMode and tags to a native read: without the Bridge pack only commands give
/// them.
fn command_fields(world: &World, actor: &Actor, state: &mut Value, notes: &mut Vec<String>) {
    match world.actor_target(actor) {
        Ok(target) => {
            let [x, y, z] = target.position;
            state["eye"] = json!({ "x": x, "y": y, "z": z });
        }
        Err(error) => notes.push(format!("querytarget gave no eye position: {error}")),
    }
    match world.game_mode_of(actor) {
        Ok(mode) => state["gameMode"] = json!(mode),
        Err(error) => notes.push(format!("testfor m= gave no gameMode: {error}")),
    }
    state["tags"] = command_tags(world, actor, notes);
    notes.push(
        "eye came from querytarget, gameMode from testfor m= and tags from tag list.".to_owned(),
    );
}

/// The actor's tags from `tag <selector> list`; null, with the reason in `notes`, when the
/// reply cannot be read.
fn command_tags(world: &World, actor: &Actor, notes: &mut Vec<String>) -> Value {
    match world.tags(&actor.selector()) {
        Ok(tags) => json!(tags),
        Err(error) => {
            notes.push(format!("tag list gave no tags: {error}"));
            Value::Null
        }
    }
}

/// The player through the WebSocket: the Bridge pack, else querytarget, testfor and tag list.
fn stable_player(world: &World, actor: &Actor) -> Result<Value> {
    if world.bridge(true)? {
        let state = world.bridge_command(&format!("re_mcp:state{}", actor.argument()))?;
        return Ok(labelled("bridge", Vec::new(), state));
    }
    let target = world.actor_target(actor)?;
    let [x, y, z] = target.position;
    let name = match actor {
        Actor::Me => Value::Null,
        Actor::Named(name) => name.as_str().into(),
    };
    let mut notes = vec![COMMAND_NOTE.to_owned()];
    let tags = command_tags(world, actor, &mut notes);
    Ok(labelled(
        "command",
        notes,
        json!({
            "name": name,
            "id": target.unique_id,
            "dimension": dimension_id(&target.dimension),
            "eye": { "x": x, "y": y, "z": z },
            "yaw": (target.yaw * 100.0).round() / 100.0,
            "pitch": world.pitch_of(actor)?,
            "gameMode": world.game_mode_of(actor)?,
            "tags": tags,
        }),
    ))
}

/// The player read through the WebSocket without its labels, for nesting in a write's result.
fn readback(world: &World, actor: &Actor) -> Result<(String, Vec<String>, Value)> {
    let mut state = stable_player(world, actor)?;
    let source = state["source"].as_str().unwrap_or_default().to_owned();
    let notes = serde_json::from_value(state["notes"].take()).unwrap_or_default();
    if let Some(fields) = state.as_object_mut() {
        fields.remove("source");
        fields.remove("notes");
    }
    Ok((source, notes, state))
}

fn player_set(world: &World, args: &PlayerSetArgs) -> Result<Value> {
    if args.location.is_none()
        && args.rotation.is_none()
        && args.dimension.is_none()
        && args.game_mode.is_none()
        && args.health.is_none()
        && args.velocity.is_none()
    {
        bail!("Give at least one of location, rotation, dimension, game_mode, health or velocity.");
    }
    let actor = &args.actor;
    if world.bridge(true)? {
        let mut spec = Vec::new();
        if let Some(at) = args.location {
            spec.extend([
                ("x", number(at.x)),
                ("y", number(at.y)),
                ("z", number(at.z)),
            ]);
        }
        if let Some(rotation) = args.rotation {
            spec.extend(rotation.yaw.map(|yaw| ("yaw", number(yaw))));
            spec.extend(rotation.pitch.map(|pitch| ("pitch", number(pitch))));
        }
        if let Some(dimension) = args.dimension {
            spec.push(("dimension", dimension.name().to_owned()));
        }
        if let Some(mode) = args.game_mode {
            spec.push(("game_mode", mode.script().to_owned()));
        }
        if let Some(health) = args.health {
            spec.push(("health", number(health)));
        }
        if let Some(velocity) = args.velocity {
            spec.extend([
                ("vx", number(velocity.x)),
                ("vy", number(velocity.y)),
                ("vz", number(velocity.z)),
            ]);
        }
        let spec: Vec<String> = spec
            .into_iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect();
        let outcome = world.bridge_write(&format!(
            "re_mcp:player_set \"{}\"{}",
            spec.join(";"),
            actor.argument()
        ))?;
        let rejected = outcome["rejected"].as_object().cloned().unwrap_or_default();
        let (source, mut notes, player) = readback(world, actor)?;
        notes.extend(rejected.iter().map(|(key, reason)| {
            format!(
                "The game refused {key}: {}",
                reason.as_str().unwrap_or_default()
            )
        }));
        return Ok(labelled(
            &source,
            notes,
            json!({ "applied": outcome["applied"], "rejected": rejected, "player": player }),
        ));
    }
    if args.health.is_some() || args.velocity.is_some() {
        bail!(
            "Vanilla commands cannot set health or velocity exactly, so player_set needs the Bridge \
             pack for them. {INSTALL_HINT}"
        );
    }
    let mut applied = Vec::new();
    if args.location.is_some() || args.rotation.is_some() || args.dimension.is_some() {
        let position = match args.location {
            Some(at) => format!("{} {} {}", number(at.x), number(at.y), number(at.z)),
            None => "~ ~ ~".to_owned(),
        };
        let rotation = match args.rotation {
            Some(rotation) => {
                let angle = |value: Option<f64>| value.map_or_else(|| "~".to_owned(), number);
                format!(" {} {}", angle(rotation.yaw), angle(rotation.pitch))
            }
            None => String::new(),
        };
        let dimension = args
            .dimension
            .map(|dimension| format!(" in {}", dimension.name()))
            .unwrap_or_default();
        world.command(&format!(
            "execute as {} at @s{dimension} run tp @s {position}{rotation}",
            actor.selector()
        ))?;
        applied.push("teleport");
    }
    if let Some(mode) = args.game_mode {
        world.command(&format!("gamemode {} {}", mode.command(), actor.selector()))?;
        applied.push("game_mode");
    }
    let (source, notes, player) = readback(world, actor)?;
    Ok(labelled(
        &source,
        notes,
        json!({ "applied": applied, "rejected": {}, "player": player }),
    ))
}

/// Native reads (one server tick each) a pressed slot key gets to show.
const SLOT_READS: usize = 10;

/// `hotbar_select` with the session's world (connected or not) and the session as the game.
fn hotbar_select_session(session: &mut Session, slot: u8, actor: &Actor) -> Result<Value> {
    let world = session.world()?;
    hotbar_select(session, &world, slot, actor)
}

/// The Bridge pack's re_mcp:hotbar while the WebSocket is connected and the pack answers,
/// else actor self's slot key.
fn hotbar_select(
    game: &mut dyn drivers::Game,
    world: &World,
    slot: u8,
    actor: &Actor,
) -> Result<Value> {
    if slot > 8 {
        bail!("Hotbar slot {slot} is outside 0 to 8.");
    }
    if world.connected() && world.bridge(true)? {
        let outcome = world.bridge_write(&format!("re_mcp:hotbar {slot}{}", actor.argument()))?;
        return Ok(labelled(
            "bridge",
            Vec::new(),
            json!({ "selectedSlot": outcome["selectedSlot"] }),
        ));
    }
    if let Actor::Named(name) = actor {
        bail!(
            "Vanilla commands cannot change the selected hotbar slot, so hotbar_select for {name} \
             needs the Bridge pack's GameTest; only actor self can press the slot key. \
             {INSTALL_HINT}"
        );
    }
    press_slot(game, slot)
}

/// Selects `slot` for actor self by pressing its key, confirmed by the native selectedSlot read.
fn press_slot(game: &mut dyn drivers::Game, slot: u8) -> Result<Value> {
    game.input(InputRequest {
        hotbar: Some(slot + 1),
        ..InputRequest::default()
    })?;
    let mut notes = vec![format!(
        "The Bridge pack is not active, so the slot key (key.hotbar.{}) was pressed and the \
         selected slot read natively from the game's local server.",
        slot + 1
    )];
    let mut read = None;
    for _ in 0..SLOT_READS {
        match game.native_player() {
            Ok(player) => {
                read = player.selected_slot;
                if read == Some(slot) {
                    break;
                }
            }
            Err(error) => {
                notes.push(format!(
                    "The native read could not confirm the slot: {error}."
                ));
                break;
            }
        }
    }
    match read {
        Some(read) if read != slot => notes.push(format!(
            "The native read showed slot {read} after {SLOT_READS} reads; the key may not have \
             reached the game (a screen open, or the window without focus)."
        )),
        None if notes.len() == 1 => {
            notes.push("The native read did not give the selected slot.".to_owned());
        }
        _ => {}
    }
    Ok(labelled("native", notes, json!({ "selectedSlot": read })))
}

/// The yaw and pitch a look_at request asks for.
fn look_angles(world: &World, args: &LookAtArgs) -> Result<(f64, f64)> {
    let forms = [
        args.yaw.is_some() || args.pitch.is_some(),
        args.position.is_some(),
        args.entity_id.is_some(),
    ];
    if forms.iter().filter(|given| **given).count() != 1 {
        bail!("Give exactly one of yaw and pitch, position or entity_id.");
    }
    if let (Some(yaw), Some(pitch)) = (args.yaw, args.pitch) {
        return Ok((yaw, pitch));
    }
    if args.yaw.is_some() || args.pitch.is_some() {
        bail!("Give yaw and pitch together.");
    }
    let state = stable_player(world, &args.actor)?;
    let eye = eye_of(&state).ok_or_else(|| Error::msg("The player has no eye position"))?;
    let target = match (&args.position, &args.entity_id) {
        (Some(position), _) => position.array(),
        (None, Some(id)) => entity_eye(world, id)?,
        (None, None) => unreachable!("one form is given"),
    };
    Ok(facing(eye, target))
}

/// An entity's eye position: from the bridge, or the querytarget position without it.
fn entity_eye(world: &World, id: &str) -> Result<[f64; 3]> {
    let argument = entity_argument(id)?;
    if world.bridge(false)? {
        let entity = world.bridge_command(&format!("re_mcp:entity {argument}"))?;
        return eye_of(&entity)
            .ok_or_else(|| Error::msg(format!("Entity {id} has no eye position")));
    }
    world
        .targets("@e")?
        .into_iter()
        .find(|target| target.unique_id == id)
        .map(|target| target.position)
        .ok_or_else(|| Error::msg(format!("No entity with id {id} matched querytarget @e.")))
}

/// look_at, turning a simulated player with a GameTest look so its pitch lasts.
fn look_at_actor(session: &mut Session, args: &LookAtArgs) -> Result<Value> {
    let world = session.ensure_world()?;
    if let Actor::Named(name) = &args.actor {
        let (yaw, pitch) = look_angles(&world, args)?;
        let pitch = pitch.clamp(-90.0, 90.0);
        if let Some(outcome) = drivers::look_simulated(session, name, yaw, pitch)? {
            let (source, mut notes, player) = readback(&world, &args.actor)?;
            notes.extend(outcome.notes);
            if args.frames > 1 {
                notes.push(
                    "A simulated player turns at its own pace, so frames was ignored.".to_owned(),
                );
            }
            return Ok(labelled(
                &source,
                notes,
                json!({
                    "requested": { "yaw": yaw, "pitch": pitch },
                    "confirmed": outcome.confirmed,
                    "player": player,
                }),
            ));
        }
    }
    look_at(&world, args)
}

fn look_at(world: &World, args: &LookAtArgs) -> Result<Value> {
    let (yaw, pitch) = look_angles(world, args)?;
    let pitch = pitch.clamp(-90.0, 90.0);
    world.look_as(&args.actor, yaw, pitch, false, args.frames, &mut || {
        thread::sleep(TICK);
        Ok(())
    })?;
    let (source, notes, player) = readback(world, &args.actor)?;
    Ok(labelled(
        &source,
        notes,
        json!({ "requested": { "yaw": yaw, "pitch": pitch }, "player": player }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::fake::{bridge_basics, failure, game, json_reply, unknown};

    fn bot() -> Actor {
        Actor::Named("Bot".into())
    }

    fn bridge(line: &str) -> Value {
        match line {
            "re_mcp:state \"Bot\"" => json_reply(json!({
                "name": "Bot", "yaw": 90.0, "pitch": 0.0,
                "eye": {"x": 0.0, "y": 1.62, "z": 0.0},
                "health": {"current": 18, "max": 20}, "selectedSlot": 2
            })),
            _ if line.starts_with("re_mcp:player_set ") => json_reply(json!({"write": 9})),
            "re_mcp:result 9" => json_reply(json!({
                "pending": false, "applied": ["teleport"], "rejected": {"velocity": "Error: not supported on players"}
            })),
            _ if line.starts_with("re_mcp:look ") || line.starts_with("re_mcp:hotbar ") => {
                json_reply(json!({"write": 1}))
            }
            "re_mcp:result 1" => json_reply(json!({"pending": false, "selectedSlot": 4})),
            _ => bridge_basics(line).unwrap_or_else(|| failure(&format!("unexpected {line}"))),
        }
    }

    /// No bridge; querytarget puts the player's eye at (1, 65.62, 2) facing yaw 45 and pitch
    /// 30, in creative.
    fn commands(line: &str) -> Value {
        if line.starts_with("re_mcp:") {
            return unknown(line);
        }
        if line == "querytarget @s" {
            return json!({"statusCode": 0, "details": r#"[{"dimension":1,"id":-12,"uniqueId":"-12","position":{"x":1,"y":65.62,"z":2},"yRot":45}]"#});
        }
        if line == "tag @s list" {
            return json!({"statusCode": 0, "statusMessage": "Steve has 1 tags: pet"});
        }
        if let Some(range) = line.strip_prefix("testfor @s[rxm=") {
            let (low, high) = range.trim_end_matches(']').split_once(",rx=").unwrap();
            let (low, high): (f64, f64) = (low.parse().unwrap(), high.parse().unwrap());
            let code = if low <= 30.0 && 30.0 <= high { 0 } else { 1 };
            return json!({"statusCode": code});
        }
        if line.starts_with("testfor @s[m=") {
            return json!({"statusCode": if line == "testfor @s[m=creative]" { 0 } else { 1 }});
        }
        if line.starts_with("execute as @s at @s") || line.starts_with("gamemode ") {
            return json!({"statusCode": 0});
        }
        failure(&format!("unexpected {line}"))
    }

    #[test]
    fn player_get_reads_the_named_player_through_the_bridge() {
        let (world, seen) = game(bridge);
        let state = stable_player(&world, &bot()).unwrap();
        assert_eq!(state["source"], "bridge");
        assert_eq!(state["notes"], json!([]));
        assert_eq!(state["health"]["current"], 18);
        assert_eq!(seen.lock().unwrap()[1], "re_mcp:state \"Bot\"");
    }

    #[test]
    fn player_get_falls_back_to_querytarget_and_bisection() {
        let (world, _) = game(commands);
        let state = stable_player(&world, &Actor::Me).unwrap();
        assert_eq!(state["source"], "command");
        assert!(state["notes"][0].as_str().unwrap().contains("Bridge"));
        assert_eq!(state["dimension"], "minecraft:nether");
        assert_eq!(state["eye"]["y"], 65.62);
        assert_eq!(state["gameMode"], "Creative");
        assert_eq!(state["tags"], json!(["pet"]));
        assert!((state["pitch"].as_f64().unwrap() - 30.0).abs() <= 0.25);
    }

    #[test]
    fn player_set_reports_refused_writes_and_reads_back() {
        let (world, seen) = game(bridge);
        let args: PlayerSetArgs = serde_json::from_value(json!({
            "actor": "Bot", "location": {"x": 1, "y": 64, "z": -2.5}, "velocity": {"x": 0, "y": 1, "z": 0}
        }))
        .unwrap();
        let result = player_set(&world, &args).unwrap();
        assert_eq!(result["source"], "bridge");
        assert_eq!(result["applied"], json!(["teleport"]));
        assert!(
            result["rejected"]["velocity"]
                .as_str()
                .unwrap()
                .contains("players")
        );
        assert!(result["notes"][0].as_str().unwrap().contains("velocity"));
        assert_eq!(result["player"]["name"], "Bot");
        assert!(
            seen.lock().unwrap().contains(
                &"re_mcp:player_set \"x=1;y=64;z=-2.5;vx=0;vy=1;vz=0\" \"Bot\"".to_owned()
            )
        );
    }

    #[test]
    fn player_set_without_the_bridge_teleports_with_commands() {
        let (world, seen) = game(commands);
        let args: PlayerSetArgs = serde_json::from_value(json!({
            "rotation": {"pitch": 10}, "dimension": "nether", "game_mode": "creative"
        }))
        .unwrap();
        let result = player_set(&world, &args).unwrap();
        assert_eq!(result["source"], "command");
        assert_eq!(result["applied"], json!(["teleport", "game_mode"]));
        let seen = seen.lock().unwrap();
        assert!(seen.contains(&"execute as @s at @s in nether run tp @s ~ ~ ~ ~ 10".to_owned()));
        assert!(seen.contains(&"gamemode creative @s".to_owned()));
        drop(seen);
        let velocity: PlayerSetArgs =
            serde_json::from_value(json!({"velocity": {"x": 0, "y": 1, "z": 0}})).unwrap();
        let error = player_set(&world, &velocity).unwrap_err().to_string();
        assert!(error.contains("rgl run"), "{error}");
    }

    /// A game whose native reads show these selected slots in turn (None: the read fails),
    /// recording the input it got.
    struct Keys {
        world: World,
        reads: Vec<Option<u8>>,
        inputs: Vec<InputRequest>,
    }

    impl drivers::Game for Keys {
        fn world(&mut self) -> Result<World> {
            Ok(self.world.clone())
        }

        fn cursor(&mut self) -> Result<crate::events::EventCursor> {
            bail!("no events")
        }

        fn wait_event(
            &mut self,
            _after: &crate::events::EventCursor,
            _timeout: Duration,
            _matches: &dyn Fn(&crate::events::HubEvent) -> bool,
        ) -> Result<(Option<crate::events::HubEvent>, Vec<String>)> {
            bail!("no events")
        }

        fn input(&mut self, request: InputRequest) -> Result<()> {
            self.inputs.push(request);
            Ok(())
        }

        fn native_player(&mut self) -> Result<protocol::unstable::NativePlayer> {
            let read = if self.reads.len() > 1 {
                self.reads.remove(0)
            } else {
                self.reads[0]
            };
            let mut player = crate::unstable::fake::native_player();
            player.selected_slot = Some(read.ok_or_else(|| Error::msg("no unstable layer"))?);
            Ok(player)
        }
    }

    fn fake_keys(
        answer: fn(&str) -> Value,
        reads: &[Option<u8>],
    ) -> (Keys, crate::bridge::fake::Seen) {
        let (world, seen) = game(answer);
        let keys = Keys {
            world,
            reads: reads.to_vec(),
            inputs: Vec::new(),
        };
        (keys, seen)
    }

    #[test]
    fn hotbar_select_uses_the_bridge_and_reads_the_slot_back() {
        let (mut keys, seen) = fake_keys(bridge, &[Some(2)]);
        let world = keys.world.clone();
        let result = hotbar_select(&mut keys, &world, 4, &bot()).unwrap();
        assert_eq!(result["source"], "bridge");
        assert_eq!(result["selectedSlot"], 4);
        assert_eq!(seen.lock().unwrap()[1], "re_mcp:hotbar 4 \"Bot\"");
        assert!(keys.inputs.is_empty());
    }

    #[test]
    fn hotbar_select_without_the_bridge_presses_the_key_and_reads_natively() {
        // The slot shows on the third native read.
        let (mut keys, _) = fake_keys(commands, &[Some(2), Some(2), Some(4)]);
        let world = keys.world.clone();
        let result = hotbar_select(&mut keys, &world, 4, &Actor::Me).unwrap();
        assert_eq!(result["source"], "native");
        assert_eq!(result["selectedSlot"], 4);
        assert_eq!(result["notes"].as_array().unwrap().len(), 1, "{result}");
        assert_eq!(
            keys.inputs
                .iter()
                .map(|input| input.hotbar)
                .collect::<Vec<_>>(),
            [Some(5)]
        );
        // A key that never shows, and a native read that cannot answer, say so in notes.
        let (mut keys, _) = fake_keys(commands, &[Some(2)]);
        let result = hotbar_select(&mut keys, &world, 4, &Actor::Me).unwrap();
        assert_eq!(result["selectedSlot"], 2);
        assert!(
            result["notes"][1]
                .as_str()
                .unwrap()
                .contains("showed slot 2"),
            "{result}"
        );
        let (mut keys, _) = fake_keys(commands, &[None]);
        let result = hotbar_select(&mut keys, &world, 4, &Actor::Me).unwrap();
        assert_eq!(result["selectedSlot"], Value::Null);
        assert!(
            result["notes"][1]
                .as_str()
                .unwrap()
                .contains("no unstable layer"),
            "{result}"
        );
        // Simulated players still need the pack's GameTest.
        let error = hotbar_select(&mut keys, &world, 4, &bot())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("GameTest") && error.contains("rgl run"),
            "{error}"
        );
        assert_eq!(keys.inputs.len(), 1);
    }

    #[test]
    fn look_at_a_position_turns_from_the_eye() {
        let (world, seen) = game(bridge);
        let args: LookAtArgs = serde_json::from_value(json!({
            "actor": "Bot", "position": {"x": -5, "y": 1.62, "z": 0}
        }))
        .unwrap();
        let result = look_at(&world, &args).unwrap();
        assert!((result["requested"]["yaw"].as_f64().unwrap() - 90.0).abs() < 1e-9);
        assert_eq!(result["player"]["name"], "Bot");
        assert!(
            seen.lock()
                .unwrap()
                .contains(&"re_mcp:look 90 0 \"Bot\"".to_owned())
        );
        let both: LookAtArgs =
            serde_json::from_value(json!({"yaw": 1, "pitch": 2, "entity_id": "-5"})).unwrap();
        assert!(look_at(&world, &both).is_err());
    }
}
