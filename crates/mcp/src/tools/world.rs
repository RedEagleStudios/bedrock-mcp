//! The `world` toolset: entities, blocks, containers, items and raycasts.

use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    Handler, Hints, Tool, Toolset, arguments,
    player::{Dimension, Vector, actor_schema, vector_schema},
    reply,
    schema::{describe, object},
};
use crate::{
    bridge::{Actor, labelled, quoted},
    error::{Result, bail},
    session::{Session, world_state::EntityFilter},
    world::{
        World, dimension_id, direction, number,
        vanilla::{PROPERTY_PRECISION, PropertyRead},
    },
};

const DEFAULT_RADIUS: f64 = 32.0;
const DEFAULT_PAGE: usize = 16;
const MAX_PAGE: usize = 256;

const COMMAND_ENTITIES_NOTE: &str = "The bedrock-mcp Bridge pack is not active, so querytarget and \
     testfor answered (the world needs cheats): ids, locations (eye height for players) and \
     display names. type is only known when the list is filtered by type. Pages come from the \
     list read on the first call, so later pages do not see entities move.";
const NATIVE_PAGE_NOTE: &str = "Read natively from the game's local server (the bedrock-mcp \
     Bridge pack is not active). Pages come from the list read on the first call, so later \
     pages do not see entities move.";
const COMMAND_INSPECT_NOTE: &str = "Neither the bedrock-mcp Bridge pack nor the unstable layer \
     answered, so querytarget, testfor and tag list answered (the world needs cheats): id, name, \
     player, dimension, location (eye for players), yaw and tags. type, health, velocity, \
     families and flags need the pack or the unstable layer.";
const COMMAND_PROPERTIES_NOTE: &str = "The bedrock-mcp Bridge pack is not active, so testfor with \
     has_property probes answered (the world needs cheats): booleans by testing true and false, \
     numbers by bisection over the 32-bit integer range, enums only among enum_values. Each probe \
     is checked to have selected the entity, which is located again when it moved.";
const COMMAND_WORLD_NOTE: &str = "The bedrock-mcp Bridge pack is not active, so time query, \
     weather query and gamerule answered (the world needs cheats). moonPhase, difficulty and \
     defaultSpawn have no query command, so they are left out. gameRules keys are spelled as \
     gamerule prints them, which can differ from the Script API's camelCase names.";
const COMMAND_SCORES_NOTE: &str = "The bedrock-mcp Bridge pack is not active, so scoreboard \
     objectives list and scoreboard players list * answered (the world needs cheats). \
     participant is the name the game prints; type is null because commands do not say whether \
     it is a player, an entity or a fake player.";
/// Property names one command read probes at most: each takes up to about 90 commands (a
/// probe and a check that the spot still holds the entity).
const MAX_COMMAND_PROPERTIES: usize = 8;

/// The description bullet of a tool that only the Bridge pack answers.
macro_rules! needs_pack {
    () => {
        "- Needs the bedrock-mcp Bridge pack; without it the call fails with the install steps."
    };
}

/// A quoted entity id argument; ids are decimal numbers, as `Entity.id` spells them.
pub fn entity_argument(id: &str) -> Result<String> {
    let digits = id.strip_prefix('-').unwrap_or(id);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("Entity id {id:?} is not an entity id; use an id from entities_list.");
    }
    quoted(id)
}

/// The quoted name of an entity type or identifier: letters, digits, `_`, `.`, `:` and `-`.
fn identifier(text: &str) -> Result<&str> {
    if text.is_empty()
        || !text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte))
    {
        bail!("{text:?} is not an identifier such as minecraft:cow.");
    }
    Ok(text)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActorArgs {
    #[serde(default)]
    actor: Actor,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EntitiesArgs {
    #[serde(default)]
    actor: Actor,
    radius: Option<f64>,
    #[serde(rename = "type")]
    entity_type: Option<String>,
    limit: Option<usize>,
    cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EntityArgs {
    /// Actions name it `entity_id`; accept both.
    #[serde(alias = "entity_id")]
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PropertiesArgs {
    id: String,
    #[serde(default)]
    names: Vec<String>,
    #[serde(default)]
    enum_values: std::collections::BTreeMap<String, Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BlockArgs {
    location: Vector,
    dimension: Option<Dimension>,
}

#[derive(Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Include {
    Blocks,
    Entities,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RaycastArgs {
    #[serde(default)]
    actor: Actor,
    from: Option<Vector>,
    direction: Option<Vector>,
    yaw: Option<f64>,
    pitch: Option<f64>,
    max_distance: Option<f64>,
    include: Option<Vec<Include>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContainerArgs {
    location: Option<Vector>,
    dimension: Option<Dimension>,
    entity_id: Option<String>,
    offset: Option<u32>,
    limit: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ItemArgs {
    #[serde(default)]
    actor: Actor,
    slot: Option<u32>,
    equipment: Option<String>,
    location: Option<Vector>,
    dimension: Option<Dimension>,
    entity_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScoresArgs {
    objective: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DynamicPropertiesArgs {
    target: String,
    player: Option<String>,
    pack: Option<String>,
}

/// Run a bridge read and label it; the install hint without the pack.
fn bridge_read(world: &World, command: &str) -> Result<Value> {
    world.require_bridge()?;
    Ok(labelled(
        "bridge",
        Vec::new(),
        world.bridge_command(command)?,
    ))
}

pub fn tools() -> Vec<Tool> {
    let tool = |hints: Hints, title: &'static str, run: Handler, definition| {
        Tool::new(Toolset::World, hints, title, run, definition)
    };
    let id =
        json!({ "type": "string", "description": "An entity id from entities_list (Entity.id)" });
    let dimension = json!({ "enum": ["overworld", "nether", "the_end"], "description": "Default: the dimension of the player who ran /connect" });
    vec![
        tool(
            Hints::READ,
            "Read the crosshair target",
            |s, a| {
                let args: ActorArgs = arguments(a)?;
                reply(target_get(&s.ensure_world()?, &args.actor)?)
            },
            json!({
                "name": "target_get",
                "description": concat!(
                    "Use this to check what a player aims at before an attack or use.\n\n",
                    "Reads the block and the entity under the player's crosshair, up to 64 blocks away, with getBlockFromViewDirection and getEntitiesFromViewDirection.\n\n",
                    needs_pack!(),
                    "\n\nReturns {maxDistance, block: {id, location, states, ..., face, faceLocation, distance} or null, entity: {id, type, name, location, distance, health, tags} or null, miss}; miss is true when neither is hit."
                ),
                "inputSchema": object(json!({ "actor": actor_schema() }), &[])
            }),
        ),
        tool(
            Hints::READ,
            "List nearby entities",
            |s, a| {
                let args: EntitiesArgs = arguments(a)?;
                reply(list_entities(s, &args)?)
            },
            json!({
                "name": "entities_list",
                "description": concat!(
                    "Use this to find entities near a player and their ids.\n\n",
                    "Lists entities within radius blocks of the player, nearest first, without the player. The first call fixes the list; pass next_cursor back as cursor until it is null to read each listed entity once.\n\n",
                    "- Sources, in order: the Bridge pack (\"bridge\"), the unstable layer's world_state capability (\"native\", single player only, see unstable_status), then querytarget and testfor (\"command\", needs cheats).\n",
                    "- bridge entries carry health and tags, and an entity that despawned since the first call shows as {id, removed: true}.\n",
                    "- native entries carry health. command entries carry id, location (eye height for players) and name, and type only when filtered by type.\n",
                    "- native and command pages come from the first call's list, so they miss later moves.\n",
                    "- bridge and native lists keep the nearest 4096 entities. An expired cursor fails; list again.\n\n",
                    "Returns {total, offset, entities: [{id, type, name, location, distance, ...}], next_cursor, source}."
                ),
                "inputSchema": object(json!({
                    "actor": actor_schema(),
                    "radius": { "type": "number", "exclusiveMinimum": 0, "description": "Search radius in blocks around the player (default 32)" },
                    "type": { "type": "string", "description": "Only this entity type, e.g. minecraft:cow" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": MAX_PAGE, "description": "Entities per page (default 16)" },
                    "cursor": { "type": "string", "description": "next_cursor from the previous page; radius and type are ignored with it" }
                }), &[])
            }),
        ),
        tool(
            Hints::READ,
            "Inspect an entity",
            |s, a| {
                let args: EntityArgs = arguments(a)?;
                reply(inspect_entity(s, &args.id)?)
            },
            json!({
                "name": "entity_inspect",
                "description": concat!(
                    "Use this to check an entity's position, health, tags or flags.\n\n",
                    "Reads one entity's identity and state by id.\n\n",
                    "- The Bridge pack (source \"bridge\") fills every field.\n",
                    "- Without it, the unstable layer's world_state capability (source \"native\", single player only, see unstable_status) reads id, type, name, nameTag, player, dimension, location, yaw, pitch and health; tags come from tag list when the WebSocket is connected (needs cheats).\n",
                    "- With neither, vanilla commands answer (source \"command\", needs cheats): querytarget, testfor and tag list give id, name, player, dimension, location (eye for players), yaw and tags.\n",
                    "- Commands cannot name an entity by id, so they select it by a spot around where querytarget saw it (a player by name), up to 2 blocks wide but under half the way to the nearest other entity. Each read is checked to have selected it, and an entity that moved is located again up to 5 times. They fail when another entity stands too close or it is in another dimension than the player.\n\n",
                    "Returns {id, type, name, nameTag, player, dimension, location, eye, velocity, yaw, pitch, viewDirection, health, tags, families, flags: {onGround, inWater, swimming, falling, climbing, sleeping, sneaking, sprinting}, source}."
                ),
                "inputSchema": object(json!({ "id": id }), &["id"])
            }),
        ),
        tool(
            Hints::READ,
            "Read an entity's components",
            |s, a| {
                let args: EntityArgs = arguments(a)?;
                let command = format!("re_mcp:entity_components {}", entity_argument(&args.id)?);
                reply(bridge_read(&s.ensure_world()?, &command)?)
            },
            json!({
                "name": "entity_components_get",
                "description": concat!(
                    "Use this to check the component values your add-on gives an entity.\n\n",
                    "Reads an entity's Script API components. Each component's fields hold every readable property that is a number, string, boolean, vector or a list of them; objects with an id show as the id.\n\n",
                    needs_pack!(),
                    "\n\nReturns {id, type, components: [{type, fields}]}."
                ),
                "inputSchema": object(json!({ "id": id }), &["id"])
            }),
        ),
        tool(
            Hints::READ,
            "Read an entity's properties",
            |s, a| {
                let args: PropertiesArgs = arguments(a)?;
                reply(entity_properties(s, &args)?)
            },
            json!({
                "name": "entity_properties_get",
                "description": concat!(
                    "Use this to check the entity properties your add-on defines in an entity's behavior file; for dynamic properties set by scripts, use dynamic_properties_get.\n\n",
                    "Reads the named entity properties (defined in the entity's behavior file, e.g. minecraft:has_nectar) with getProperty.\n\n",
                    "- The stable Script API cannot list a type's properties, so name the ones to read.\n",
                    "- Entity properties are not private to a pack, so this reads the add-on's; dynamic properties are, which is why dynamic_properties_get reads them from the game instead.\n",
                    "- Without the Bridge pack, testfor has_property probes answer (source \"command\", needs cheats; entity selected as in entity_inspect, by type too when the unstable layer reads it): booleans by testing true and false, ints and floats by bisection between -2147483648 and 2147483647 (floats to 0.001), enums only among enum_values. At most 8 names; unread says why a name was not read.\n",
                    "- A refused command (no cheats) or a reply in another language than English fails the call with the reason.\n\n",
                    "Returns {id, type (from commands: the unstable layer's type, else null), properties: {name: value}, undefinedProperties (bridge: names the type does not define), unread (command: {name: reason}), source}."
                ),
                "inputSchema": object(json!({
                    "id": id,
                    "names": { "type": "array", "items": { "type": "string" }, "description": "Entity property names to read" },
                    "enum_values": { "type": "object", "additionalProperties": { "type": "array", "items": { "type": "string" } }, "description": "Without the Bridge pack: candidate values per enum property name, e.g. {\"wiki:color\": [\"red\", \"blue\"]}; these names are read too" }
                }), &["id"])
            }),
        ),
        tool(
            Hints::READ,
            "Inspect a block",
            |s, a| {
                let args: BlockArgs = arguments(a)?;
                let at = args.location;
                let mut command = format!(
                    "re_mcp:block {} {} {}",
                    number(at.x),
                    number(at.y),
                    number(at.z)
                );
                if let Some(dimension) = args.dimension {
                    command.push_str(&format!(" {}", quoted(dimension.name())?));
                }
                reply(bridge_read(&s.ensure_world()?, &command)?)
            },
            json!({
                "name": "block_inspect",
                "description": concat!(
                    "Use this to check a block's id, states and contents after an action.\n\n",
                    "Reads the block at a location.\n\n",
                    "- The stable Script API has no solid flag, so none is returned.\n",
                    needs_pack!(),
                    "\n\nReturns {id, location, dimension, states (permutation states), tags, air, liquid, waterlogged, redstonePower (null when the block carries none), light, skyLight, components, inventory (slot count, or null)}."
                ),
                "inputSchema": object(json!({
                    "location": vector_schema("Block coordinates"),
                    "dimension": dimension
                }), &["location"])
            }),
        ),
        tool(
            Hints::READ,
            "Cast a ray",
            |s, a| {
                let args: RaycastArgs = arguments(a)?;
                reply(raycast(&s.ensure_world()?, &args)?)
            },
            json!({
                "name": "raycast",
                "description": concat!(
                    "Use this to check what lies along a line, such as whether a path is clear.\n\n",
                    "Casts a ray with Dimension.getBlockFromRay and getEntitiesFromRay from a point along a direction vector, or along a Bedrock yaw and pitch.\n\n",
                    "- The player the ray starts from is left out of entities.\n",
                    "- A zero direction fails.\n",
                    needs_pack!(),
                    "\n\nReturns {from, direction, maxDistance, block: {..., face, faceLocation, distance} or null, entities: [{id, type, name, location, distance, ...}]}."
                ),
                "inputSchema": object(json!({
                    "actor": actor_schema(),
                    "from": vector_schema("Start point; default the actor's eye"),
                    "direction": vector_schema("Direction vector; or give yaw and pitch"),
                    "yaw": { "type": "number", "description": "Bedrock degrees: 0 faces +Z (south), 90 faces -X (west); give with pitch instead of direction" },
                    "pitch": { "type": "number", "minimum": -90, "maximum": 90, "description": "Degrees, positive looks down; give with yaw instead of direction" },
                    "max_distance": { "type": "number", "exclusiveMinimum": 0, "description": "Ray length in blocks (default 64)" },
                    "include": { "type": "array", "items": { "enum": ["blocks", "entities"] }, "description": "What the ray may hit (default both)" }
                }), &[])
            }),
        ),
        tool(
            Hints::READ,
            "Read a container",
            |s, a| {
                let args: ContainerArgs = arguments(a)?;
                let mut spec =
                    target_spec(args.location, args.dimension, args.entity_id.as_deref())?;
                if spec.is_empty() {
                    bail!("Give location or entity_id.");
                }
                spec.extend(args.offset.map(|offset| format!("offset={offset}")));
                spec.extend(args.limit.map(|limit| format!("limit={limit}")));
                let command = format!("re_mcp:container {}", quoted(&spec.join(";"))?);
                reply(bridge_read(&s.ensure_world()?, &command)?)
            },
            json!({
                "name": "container_get",
                "description": concat!(
                    "Use this to check what a chest or other container holds.\n\n",
                    "Reads a block's or an entity's inventory without opening its screen, one page of slots at a time.\n\n",
                    "- Give location or entity_id, not both.\n",
                    "- Empty slots are left out; pass next back as offset until it is null.\n",
                    needs_pack!(),
                    "\n\nReturns {size, emptySlots, offset, slots: [{slot, id, amount, ...item details as in item_inspect}], next}."
                ),
                "inputSchema": object(json!({
                    "location": vector_schema("A container block such as a chest"),
                    "dimension": dimension,
                    "entity_id": id,
                    "offset": { "type": "integer", "minimum": 0, "description": "First slot (default 0)" },
                    "limit": { "type": "integer", "minimum": 1, "description": "Slots per page (default 27)" }
                }), &[])
            }),
        ),
        tool(
            Hints::READ,
            "Inspect an item",
            |s, a| {
                let args: ItemArgs = arguments(a)?;
                reply(item_inspect(&s.ensure_world()?, &args)?)
            },
            json!({
                "name": "item_inspect",
                "description": concat!(
                    "Use this to check one item stack's components, durability or lore in detail.\n\n",
                    "Reads one slot of the player's inventory, one of its equipment slots, or one slot of a container block or entity.\n\n",
                    "- Give slot or equipment, and at most one of location and entity_id. equipment reads the actor's own, so it takes neither.\n",
                    needs_pack!(),
                    "\n\nReturns {slot or equipment, item: {id, amount, maxAmount, stackable, name, lore, tags, canDestroy, canPlaceOn, keepOnDeath, lockMode, components, durability: {damage, max, unbreakable}, enchantable: {slots, enchantments}, food: {nutrition, saturationModifier, canAlwaysEat, usingConvertsTo}, cooldown: {category, ticks, remaining}, potion: {effect, durationTicks, delivery}}, or null for an empty slot}."
                ),
                "inputSchema": object(json!({
                    "actor": actor_schema(),
                    "slot": { "type": "integer", "minimum": 0, "description": "Inventory or container slot" },
                    "equipment": { "enum": ["Head", "Chest", "Legs", "Feet", "Mainhand", "Offhand"], "description": "An equipment slot of the actor instead of slot" },
                    "location": vector_schema("Read the slot of this container block"),
                    "dimension": dimension,
                    "entity_id": describe(id.clone(), "Read the slot of this entity's inventory")
                }), &[])
            }),
        ),
        tool(
            Hints::READ,
            "Read world info",
            |s, _| reply(world_info(&s.ensure_world()?)?),
            json!({
                "name": "world_info",
                "description": concat!(
                    "Use this to check the time, weather, difficulty and game rules.\n\n",
                    "Reads the world's state.\n\n",
                    "- The bedrock-mcp Bridge pack (source \"bridge\") fills every field.\n",
                    "- Without it, time query, weather query and gamerule answer (source \"command\", needs cheats) with timeOfDay, absoluteTime, day, weather, dimension and gameRules; moonPhase, difficulty and defaultSpawn are left out.\n",
                    "- From commands, gameRules keys are spelled as gamerule prints them, not as the Script API's camelCase names.\n",
                    "- A refused command (no cheats) or a reply in another language than English fails the call with the reason.\n\n",
                    "Returns {timeOfDay, absoluteTime, day, moonPhase, weather, difficulty, defaultSpawn, dimension (of the player who ran /connect), gameRules, source}."
                ),
                "inputSchema": object(json!({}), &[])
            }),
        ),
        tool(
            Hints::READ,
            "Read scoreboards",
            |s, a| {
                let args: ScoresArgs = arguments(a)?;
                reply(scoreboard_get(
                    &s.ensure_world()?,
                    args.objective.as_deref(),
                )?)
            },
            json!({
                "name": "scoreboard_get",
                "description": concat!(
                    "Use this to check the scores your add-on keeps.\n\n",
                    "Reads scoreboard objectives and their scores: every objective, or the one named.\n\n",
                    "- A named objective that does not exist gives an empty list.\n",
                    "- The bedrock-mcp Bridge pack answers when present (source \"bridge\").\n",
                    "- Without it, scoreboard objectives list and scoreboard players list * answer (source \"command\", needs cheats). participant is the name the game prints and type is null.\n",
                    "- A refused command (no cheats) or a reply in another language than English fails the call with the reason.\n\n",
                    "Returns {objectives: [{id, displayName, criteria (command only), scores: [{participant, type, score}]}], source}."
                ),
                "inputSchema": object(json!({ "objective": { "type": "string", "description": "Objective id to read; default all objectives" } }), &[])
            }),
        ),
        tool(
            Hints::READ,
            "Read dynamic properties",
            |s, a| {
                let args: DynamicPropertiesArgs = arguments(a)?;
                reply(dynamic_properties(s, &args)?)
            },
            json!({
                "name": "dynamic_properties_get",
                "description": concat!(
                    "Use this to check the dynamic properties your add-on stored.\n\n",
                    "Reads every pack's dynamic properties on the world, a player or an entity from the game's own local server. A pack's Script API reads only its own; this needs no Bridge pack, cheats or Beta APIs.\n\n",
                    "- Needs the unstable layer's world_state capability (see unstable_status), so single player only; otherwise it fails and says why.\n",
                    "- truncated is true when a size bound cut the read or a number was not finite.\n\n",
                    "Returns {entity, collections: {<name as the game stores it>: {byteCount, properties: {<key>: {type: double|float|boolean|string|vector, value}}}}, truncated, tick (game tick of the read), readMicros}."
                ),
                "inputSchema": object(json!({
                    "target": { "type": "string", "description": "\"world\", \"player\", or an entity id from entities_list" },
                    "player": { "type": "string", "description": "With target \"player\": the player's name (default the first player)" },
                    "pack": { "type": "string", "description": "Only this collection" }
                }), &["target"])
            }),
        ),
    ]
}

/// `x,y,z,dimension` or `entity` options naming a block or an entity.
fn target_spec(
    location: Option<Vector>,
    dimension: Option<Dimension>,
    entity_id: Option<&str>,
) -> Result<Vec<String>> {
    let mut spec = Vec::new();
    match (location, entity_id) {
        (Some(_), Some(_)) => bail!("Give location or entity_id, not both."),
        (Some(at), None) => {
            spec.extend([
                format!("x={}", number(at.x)),
                format!("y={}", number(at.y)),
                format!("z={}", number(at.z)),
            ]);
            spec.extend(dimension.map(|dimension| format!("dimension={}", dimension.name())));
        }
        (None, Some(id)) => {
            entity_argument(id)?;
            spec.push(format!("entity={id}"));
        }
        (None, None) => {}
    }
    Ok(spec)
}

fn target_get(world: &World, actor: &Actor) -> Result<Value> {
    let mut result = bridge_read(world, &format!("re_mcp:target{}", actor.argument()))?;
    result["miss"] = (result["block"].is_null() && result["entity"].is_null()).into();
    Ok(result)
}

/// A cursor: which list answered (`bridge`, `native` or `command`), its number and the next
/// offset.
fn parse_cursor(cursor: &str) -> Result<(&str, u64, usize)> {
    let mut parts = cursor.split(':');
    if let (Some(kind @ ("bridge" | "native" | "command")), Some(list), Some(offset), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
        && let (Ok(list), Ok(offset)) = (list.parse(), offset.parse())
    {
        return Ok((kind, list, offset));
    }
    bail!("{cursor:?} is not a cursor from entities_list.")
}

/// `entities_list`: the Bridge pack when the WebSocket is connected and the pack answers, else
/// the unstable layer's native read, else querytarget and testfor.
fn list_entities(session: &mut Session, args: &EntitiesArgs) -> Result<Value> {
    let limit = args.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
    let cursor = args.cursor.as_deref().map(parse_cursor).transpose()?;
    if let Some(("native", list, offset)) = cursor {
        let notes = vec![NATIVE_PAGE_NOTE.to_owned()];
        return kept_page(&session.world()?, "native", list, offset, limit, notes);
    }
    let world = session.world()?;
    let mut notes = Vec::new();
    // A bridge or command cursor continues its own list.
    let stable = cursor.is_some() || (world.connected() && world.bridge(true)?);
    if !stable && session.native_world(&mut notes) {
        if !world.connected() {
            notes.push(
                "Minecraft's WebSocket is not connected, so the Bridge pack was not asked."
                    .to_owned(),
            );
        }
        let player = match &args.actor {
            Actor::Me => None,
            Actor::Named(name) => Some(name.as_str()),
        };
        let filter = EntityFilter {
            player,
            radius: args.radius.unwrap_or(DEFAULT_RADIUS),
            entity_type: args.entity_type.as_deref().map(identifier).transpose()?,
        };
        let (entries, found) = session.native_entities(&filter)?;
        notes.splice(0..0, found);
        let list = world.keep_entity_list(entries);
        return kept_page(&world, "native", list, 0, limit, notes);
    }
    entities_list(&session.ensure_world()?, args)
}

/// One page of a list kept by [`World::keep_entity_list`].
fn kept_page(
    world: &World,
    kind: &str,
    list: u64,
    offset: usize,
    limit: usize,
    notes: Vec<String>,
) -> Result<Value> {
    let Some(entities) = world.entity_list(list) else {
        bail!("The cursor's entity list expired; call entities_list without cursor again.");
    };
    let end = (offset + limit).min(entities.len());
    let page = entities.get(offset..end).unwrap_or_default();
    let next = (end < entities.len()).then(|| format!("{kind}:{list}:{end}"));
    Ok(labelled(
        kind,
        notes,
        json!({ "total": entities.len(), "offset": offset, "entities": page, "next_cursor": next }),
    ))
}

fn entities_list(world: &World, args: &EntitiesArgs) -> Result<Value> {
    let limit = args.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
    let cursor = args.cursor.as_deref().map(parse_cursor).transpose()?;
    let bridge = match cursor {
        Some((kind, ..)) => kind == "bridge",
        None => world.bridge(true)?,
    };
    if bridge {
        let player = args.actor.argument();
        let page = match cursor {
            Some((_, list, offset)) => world.bridge_command(&format!(
                "re_mcp:entities_page {list} {offset} {limit}{player}"
            ))?,
            None => {
                let kind = args
                    .entity_type
                    .as_deref()
                    .map(identifier)
                    .transpose()?
                    .unwrap_or("*");
                world.bridge_command(&format!(
                    "re_mcp:entities {} {} {limit}{player}",
                    number(args.radius.unwrap_or(DEFAULT_RADIUS)),
                    quoted(kind)?
                ))?
            }
        };
        let mut notes = Vec::new();
        if page["truncated"] == true {
            notes.push(
                "The bridge keeps the nearest 4096 entities; farther ones are left out.".to_owned(),
            );
        }
        let next = page["next"]
            .as_u64()
            .map(|next| format!("bridge:{}:{next}", page["snapshot"]));
        return Ok(labelled(
            "bridge",
            notes,
            json!({ "total": page["total"], "offset": page["offset"], "entities": page["entities"], "next_cursor": next }),
        ));
    }
    let (list, offset) = match cursor {
        Some((_, list, offset)) => (list, offset),
        None => {
            let kind = args.entity_type.as_deref().map(identifier).transpose()?;
            let centre = world.actor_target(&args.actor)?;
            let found =
                world.entities_around(&centre, args.radius.unwrap_or(DEFAULT_RADIUS), kind)?;
            (world.keep_entity_list(found), 0)
        }
    };
    kept_page(
        world,
        "command",
        list,
        offset,
        limit,
        vec![COMMAND_ENTITIES_NOTE.to_owned()],
    )
}

/// `entity_inspect`: the Bridge pack when the WebSocket is connected and the pack answers,
/// else the unstable layer's native read (with tags from commands), else vanilla commands.
fn inspect_entity(session: &mut Session, id: &str) -> Result<Value> {
    let command = format!("re_mcp:entity {}", entity_argument(id)?);
    let world = session.world()?;
    let mut notes = Vec::new();
    if !(world.connected() && world.bridge(true)?) && session.native_world(&mut notes) {
        let mut entity = session.native_entity(id)?;
        add_command_tags(&world, id, &mut entity);
        return Ok(entity);
    }
    let world = session.ensure_world()?;
    if world.bridge(true)? {
        return bridge_read(&world, &command);
    }
    let mut entity = command_entity(&world, id)?;
    if let Some(list) = entity["notes"].as_array_mut() {
        list.extend(notes.into_iter().map(Value::from));
    }
    Ok(entity)
}

/// Add `tags` from `tag list` to a native entity read, which has none; the notes say how it
/// went. Never connects the WebSocket for it.
fn add_command_tags(world: &World, id: &str, entity: &mut Value) {
    let note = if !world.connected() {
        "tags were not read: Minecraft's WebSocket is not connected, and the native read has \
         none."
            .to_owned()
    } else {
        let kind = entity["type"].as_str();
        let read = world.locate(id, kind).and_then(|mut located| {
            world.on_located(&mut located, |selector| world.tags(selector))
        });
        match read {
            Ok(tags) => {
                entity["tags"] = json!(tags);
                "tags came from the tag list command (source command).".to_owned()
            }
            Err(error) => format!("tags were not read with the tag list command: {error}"),
        }
    };
    if let Some(list) = entity["notes"].as_array_mut() {
        list.push(note.into());
    }
}

/// `entity_inspect` from querytarget, testfor and tag list.
fn command_entity(world: &World, id: &str) -> Result<Value> {
    let mut located = world.locate(id, None)?;
    let player_name = located.player.clone();
    let (name, tags) = world.on_located(&mut located, |selector| {
        let name = match &player_name {
            Some(name) => Some(name.clone()),
            None => world.display_name(selector)?,
        };
        Ok((name, world.tags(selector)?))
    })?;
    let [x, y, z] = located.target.position;
    let spot = json!({ "x": x, "y": y, "z": z });
    let player = located.player.is_some();
    Ok(labelled(
        "command",
        vec![COMMAND_INSPECT_NOTE.to_owned()],
        json!({
            "id": id,
            "name": name,
            "player": player,
            "dimension": dimension_id(&located.target.dimension),
            "location": if player { Value::Null } else { spot.clone() },
            "eye": if player { spot } else { Value::Null },
            "yaw": located.target.yaw,
            "tags": tags,
        }),
    ))
}

/// An enum property value as entity files may spell it (Microsoft Learn, "Introduction to
/// Entity Properties": 1 to 32 characters, a letter first, then letters, digits or `_`).
fn enum_value(text: &str) -> Result<&str> {
    let mut chars = text.chars();
    if text.len() > 32
        || !chars
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic())
        || !chars.all(|char| char.is_ascii_alphanumeric() || char == '_')
    {
        bail!("{text:?} is not an enum property value: a letter, then letters, digits or _.");
    }
    Ok(text)
}

/// `entity_properties_get`: the Bridge pack, else `has_property` probes.
fn entity_properties(session: &mut Session, args: &PropertiesArgs) -> Result<Value> {
    let argument = entity_argument(&args.id)?;
    let mut names = args
        .names
        .iter()
        .map(|name| identifier(name))
        .collect::<Result<Vec<_>>>()?;
    for (name, values) in &args.enum_values {
        let name = identifier(name)?;
        for value in values {
            enum_value(value)?;
        }
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let world = session.ensure_world()?;
    if world.bridge(true)? {
        let mut command = format!("re_mcp:entity_properties {argument}");
        if !names.is_empty() {
            command.push(' ');
            command.push_str(&quoted(&names.join(","))?);
        }
        return bridge_read(&world, &command);
    }
    let mut notes = Vec::new();
    let kind = if session.native_world(&mut notes) {
        match session.native_entity(&args.id) {
            Ok(entity) => entity["type"].as_str().map(str::to_owned),
            Err(error) => {
                notes.push(format!(
                    "The unstable layer did not read the entity's type: {error}"
                ));
                None
            }
        }
    } else {
        None
    };
    let mut read = command_properties(&world, &args.id, kind, &names, &args.enum_values)?;
    if let Some(list) = read["notes"].as_array_mut() {
        list.extend(notes.into_iter().map(Value::from));
    }
    Ok(read)
}

/// Entity properties from `has_property` probes; see [`World::property`]. `kind` is the
/// entity's type when the unstable layer read it.
fn command_properties(
    world: &World,
    id: &str,
    kind: Option<String>,
    names: &[&str],
    enum_values: &std::collections::BTreeMap<String, Vec<String>>,
) -> Result<Value> {
    if names.is_empty() {
        bail!(
            "Name the properties to read: without the bedrock-mcp Bridge pack, commands can only \
             test named properties."
        );
    }
    if names.len() > MAX_COMMAND_PROPERTIES {
        bail!(
            "Without the bedrock-mcp Bridge pack each property takes dozens of commands, so name \
             at most {MAX_COMMAND_PROPERTIES} per call."
        );
    }
    let mut located = world.locate(id, kind.as_deref())?;
    let mut properties = serde_json::Map::new();
    let mut unread = serde_json::Map::new();
    let mut approximate = false;
    for (index, name) in names.iter().enumerate() {
        if index > 0 {
            // A fresh spot for each property: the entity may have wandered during the last.
            located = world.locate(id, kind.as_deref())?;
        }
        let candidates = enum_values.get(*name).map(Vec::as_slice);
        match world.property(&mut located, name, candidates)? {
            PropertyRead::Value { value, exact } => {
                approximate |= !exact;
                properties.insert((*name).to_owned(), value);
            }
            PropertyRead::Unread(reason) => {
                unread.insert((*name).to_owned(), reason.into());
            }
        }
    }
    let mut notes = vec![COMMAND_PROPERTIES_NOTE.to_owned()];
    if approximate {
        notes.push(format!(
            "A float between two integers was found to within {PROPERTY_PRECISION}."
        ));
    }
    Ok(labelled(
        "command",
        notes,
        json!({ "id": id, "type": kind, "properties": properties, "unread": unread }),
    ))
}

/// `world_info`: the Bridge pack, else time query, weather query and gamerule.
fn world_info(world: &World) -> Result<Value> {
    if world.bridge(true)? {
        return bridge_read(world, "re_mcp:world");
    }
    Ok(labelled(
        "command",
        vec![COMMAND_WORLD_NOTE.to_owned()],
        world.command_world_info()?,
    ))
}

/// `scoreboard_get`: the Bridge pack, else the scoreboard list commands.
fn scoreboard_get(world: &World, objective: Option<&str>) -> Result<Value> {
    if world.bridge(true)? {
        let command = match objective {
            Some(objective) => format!("re_mcp:scores {}", quoted(objective)?),
            None => "re_mcp:scores".to_owned(),
        };
        let mut result = bridge_read(world, &command)?;
        if let Some(fields) = result.as_object_mut()
            && let Some(list) = fields.remove("result")
        {
            fields.insert("objectives".to_owned(), list);
        }
        return Ok(result);
    }
    Ok(labelled(
        "command",
        vec![COMMAND_SCORES_NOTE.to_owned()],
        json!({ "objectives": world.command_scores(objective)? }),
    ))
}

fn dynamic_properties(session: &mut Session, args: &DynamicPropertiesArgs) -> Result<Value> {
    let target = args.target.trim();
    if target.is_empty() {
        bail!("Give target: \"world\", \"player\" or an entity id from entities_list.");
    }
    if !matches!(target, "world" | "player") {
        entity_argument(target)?;
    }
    if args.player.is_some() && target != "player" {
        bail!("player names the player for target \"player\" only.");
    }
    session.dynamic_properties_get(target, args.player.as_deref(), args.pack.clone())
}

fn raycast(world: &World, args: &RaycastArgs) -> Result<Value> {
    let towards = match (args.direction, args.yaw, args.pitch) {
        (Some(vector), None, None) => vector.array(),
        (None, Some(yaw), Some(pitch)) => direction(yaw, pitch),
        _ => bail!("Give direction, or yaw and pitch together."),
    };
    if towards.iter().all(|axis| *axis == 0.0) {
        bail!("The direction must not be zero.");
    }
    let include = args
        .include
        .clone()
        .unwrap_or_else(|| vec![Include::Blocks, Include::Entities]);
    let mut spec = vec![
        format!("dx={}", number(towards[0])),
        format!("dy={}", number(towards[1])),
        format!("dz={}", number(towards[2])),
        format!("blocks={}", u8::from(include.contains(&Include::Blocks))),
        format!(
            "entities={}",
            u8::from(include.contains(&Include::Entities))
        ),
    ];
    if let Some(max) = args.max_distance {
        spec.push(format!("max={}", number(max)));
    }
    if let Some(from) = args.from {
        spec.extend([
            format!("x={}", number(from.x)),
            format!("y={}", number(from.y)),
            format!("z={}", number(from.z)),
        ]);
    }
    bridge_read(
        world,
        &format!(
            "re_mcp:raycast {}{}",
            quoted(&spec.join(";"))?,
            args.actor.argument()
        ),
    )
}

fn item_inspect(world: &World, args: &ItemArgs) -> Result<Value> {
    let mut spec = target_spec(args.location, args.dimension, args.entity_id.as_deref())?;
    match (args.slot, &args.equipment) {
        (Some(slot), None) => spec.push(format!("slot={slot}")),
        (None, Some(equipment)) if spec.is_empty() => {
            spec.push(format!("equipment={}", identifier(equipment)?));
        }
        (None, Some(_)) => {
            bail!("equipment reads the actor's own equipment; leave out location and entity_id.")
        }
        _ => bail!("Give slot or equipment."),
    }
    bridge_read(
        world,
        &format!(
            "re_mcp:item {}{}",
            quoted(&spec.join(";"))?,
            args.actor.argument()
        ),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::bridge::fake::{bridge_basics, failure, game, json_reply, unknown};

    const IDS: [&str; 5] = ["-5", "-6", "-7", "-8", "-9"];

    /// No bridge; the player stands at the origin among five entities at distances 5, 1, 4, 2, 3.
    fn commands(line: &str) -> Value {
        if line.starts_with("re_mcp:") {
            return unknown(line);
        }
        let me =
            r#"{"dimension":0,"id":-1,"uniqueId":"-1","position":{"x":0,"y":0,"z":0},"yRot":0}"#;
        match line {
            "querytarget @s" => json!({"statusCode": 0, "details": format!("[{me}]")}),
            _ if line.starts_with("querytarget @e[x=0,y=0,z=0,r=32") => {
                let mut list = vec![me.to_owned()];
                for (id, x) in IDS.iter().zip([5, 1, 4, 2, 3]) {
                    list.push(format!(r#"{{"dimension":0,"id":{id},"uniqueId":"{id}","position":{{"x":{x},"y":0,"z":0}},"yRot":0}}"#));
                }
                json!({"statusCode": 0, "details": format!("[{}]", list.join(","))})
            }
            _ if line.starts_with("testfor @e[") => {
                json!({"statusCode": 0, "victim": ["Me", "A", "B", "C", "D", "E"]})
            }
            _ => failure(&format!("unexpected {line}")),
        }
    }

    fn page(world: &World, cursor: Option<String>, actor: &str) -> Value {
        let args: EntitiesArgs =
            serde_json::from_value(json!({ "actor": actor, "limit": 2, "cursor": cursor }))
                .unwrap();
        entities_list(world, &args).unwrap()
    }

    fn collect(world: &World, actor: &str) -> (Vec<String>, Value) {
        let mut ids = Vec::new();
        let mut cursor = None;
        let mut first = Value::Null;
        loop {
            let result = page(world, cursor, actor);
            if first.is_null() {
                first = result.clone();
            }
            ids.extend(
                result["entities"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|entity| entity["id"].as_str().unwrap().to_owned()),
            );
            match result["next_cursor"].as_str() {
                Some(next) => cursor = Some(next.to_owned()),
                None => return (ids, first),
            }
        }
    }

    #[test]
    fn command_pages_cover_every_entity_once_nearest_first() {
        let (world, _) = game(commands);
        let (ids, first) = collect(&world, "self");
        assert_eq!(first["source"], "command");
        assert!(first["notes"][0].as_str().unwrap().contains("Bridge"));
        assert_eq!(first["total"], 5);
        assert_eq!(first["entities"][0]["name"], "B");
        assert_eq!(ids, ["-6", "-8", "-9", "-7", "-5"]);
    }

    /// A bridge whose snapshot 7 holds the five ids in order.
    fn bridge(line: &str) -> Value {
        let page = |offset: usize, limit: usize| {
            let end = (offset + limit).min(IDS.len());
            json_reply(json!({
                "snapshot": 7, "total": IDS.len(), "offset": offset,
                "entities": IDS[offset..end].iter().map(|id| json!({"id": id})).collect::<Vec<_>>(),
                "next": (end < IDS.len()).then_some(end),
            }))
        };
        if line == "re_mcp:entities 32 \"*\" 2 \"Bot\"" {
            return page(0, 2);
        }
        if let Some(rest) = line.strip_prefix("re_mcp:entities_page 7 ") {
            let rest = rest.strip_suffix(" \"Bot\"").unwrap();
            let (offset, limit) = rest.split_once(' ').unwrap();
            return page(offset.parse().unwrap(), limit.parse().unwrap());
        }
        match line {
            "re_mcp:target \"Bot\"" => {
                json_reply(json!({"maxDistance": 64, "block": null, "entity": null}))
            }
            _ if line.starts_with("re_mcp:raycast ") => {
                json_reply(json!({"block": null, "entities": []}))
            }
            _ => bridge_basics(line).unwrap_or_else(|| failure(&format!("unexpected {line}"))),
        }
    }

    #[test]
    fn bridge_pages_follow_the_snapshot_for_the_named_actor() {
        let (world, seen) = game(bridge);
        let (ids, first) = collect(&world, "Bot");
        assert_eq!(first["source"], "bridge");
        assert_eq!(ids, IDS);
        assert_eq!(ids.iter().collect::<HashSet<_>>().len(), IDS.len());
        assert_eq!(
            seen.lock().unwrap()[1..],
            [
                "re_mcp:entities 32 \"*\" 2 \"Bot\"",
                "re_mcp:entities_page 7 2 2 \"Bot\"",
                "re_mcp:entities_page 7 4 2 \"Bot\"",
            ]
        );
    }

    #[test]
    fn target_get_reports_an_explicit_miss() {
        let (world, _) = game(bridge);
        let result = target_get(&world, &Actor::Named("Bot".into())).unwrap();
        assert_eq!(result["source"], "bridge");
        assert_eq!(result["miss"], true);
    }

    #[test]
    fn raycast_turns_yaw_and_pitch_into_a_direction() {
        let (world, seen) = game(bridge);
        let args: RaycastArgs = serde_json::from_value(
            json!({"yaw": 90, "pitch": 0, "include": ["entities"], "max_distance": 10}),
        )
        .unwrap();
        raycast(&world, &args).unwrap();
        assert_eq!(
            seen.lock().unwrap()[1],
            "re_mcp:raycast \"dx=-1;dy=0;dz=0;blocks=0;entities=1;max=10\""
        );
    }

    #[test]
    fn bridge_only_reads_explain_the_install_without_it() {
        let (world, _) = game(commands);
        let error = target_get(&world, &Actor::Me).unwrap_err().to_string();
        assert!(
            error.contains("rgl run") && error.contains("/reload all"),
            "{error}"
        );
        assert!(entity_argument("12; say hi").is_err());
        assert_eq!(entity_argument("-4294967295").unwrap(), "\"-4294967295\"");
    }

    /// No bridge; a cow with tag "pet" and wiki:tame true at (1, 64, 2), the player elsewhere.
    /// A selector may name the cow's type.
    fn vanilla(line: &str) -> Value {
        let line = line.replace("type=minecraft:cow,", "");
        let line = line.as_str();
        let me = r#"{"dimension":0,"id":-1,"uniqueId":"-1","position":{"x":0,"y":65.62,"z":0},"yRot":0}"#;
        let cow =
            r#"{"dimension":0,"id":-9,"uniqueId":"-9","position":{"x":1,"y":64,"z":2},"yRot":90}"#;
        // Under half the way to the player's feet at (0, 64, 0).
        let spot = "@e[x=1,y=64,z=2,r=1.0062";
        let details = |list: &str| json!({"statusCode": 0, "details": format!("[{list}]")});
        if line.starts_with("re_mcp:") {
            return unknown(line);
        }
        match line {
            "querytarget @a" | "querytarget @s" => return details(me),
            "querytarget @e" => return details(&format!("{me},{cow}")),
            "time query daytime" => {
                return json!({"statusCode": 0, "statusMessage": "Daytime is 1000"});
            }
            "time query gametime" => {
                return json!({"statusCode": 0, "statusMessage": "Gametime is 25000"});
            }
            "time query day" => return json!({"statusCode": 0, "statusMessage": "Day is 1"}),
            "weather query" => {
                return json!({"statusCode": 0, "statusMessage": "Weather state is: clear"});
            }
            "gamerule" => return json!({"statusCode": 0, "statusMessage": "keepinventory = true"}),
            "scoreboard objectives list" => {
                return json!({"statusCode": 0, "statusMessage": "There are no objectives on the scoreboard"});
            }
            _ => {}
        }
        if line == format!("querytarget {spot}]") {
            return details(cow);
        }
        if line == format!("testfor {spot}]") {
            return json!({"statusCode": 0, "victim": ["Cow"]});
        }
        if line == format!("tag {spot}] list") {
            return json!({"statusCode": 0, "statusMessage": "Cow has 1 tags: pet"});
        }
        if line == format!("testfor {spot},has_property={{wiki:tame=true}}]") {
            return json!({"statusCode": 0, "victim": ["Cow"]});
        }
        if line.starts_with(&format!("testfor {spot},has_property=")) {
            return failure("No targets matched selector");
        }
        failure(&format!("unexpected {line}"))
    }

    #[test]
    fn without_the_bridge_reads_fall_back_to_commands() {
        let (world, _) = game(vanilla);
        let info = world_info(&world).unwrap();
        assert_eq!(info["source"], "command");
        assert_eq!(info["weather"], "Clear");
        assert_eq!(info["gameRules"]["keepinventory"], true);
        assert!(info["notes"][0].as_str().unwrap().contains("difficulty"));
        let scores = scoreboard_get(&world, None).unwrap();
        assert_eq!(scores["source"], "command");
        assert_eq!(scores["objectives"], json!([]));
        let entity = command_entity(&world, "-9").unwrap();
        assert_eq!(entity["source"], "command");
        assert_eq!(entity["name"], "Cow");
        assert_eq!(entity["tags"], json!(["pet"]));
        assert_eq!(entity["location"], json!({"x": 1.0, "y": 64.0, "z": 2.0}));
        let colors =
            std::collections::BTreeMap::from([("wiki:color".to_owned(), vec!["red".to_owned()])]);
        let read = command_properties(
            &world,
            "-9",
            Some("minecraft:cow".to_owned()),
            &["wiki:tame", "wiki:color"],
            &colors,
        )
        .unwrap();
        assert_eq!(read["type"], "minecraft:cow");
        assert_eq!(read["source"], "command");
        assert_eq!(read["properties"], json!({"wiki:tame": true}));
        assert!(
            read["unread"]["wiki:color"]
                .as_str()
                .unwrap()
                .contains("red")
        );
        assert!(command_properties(&world, "-9", None, &[], &colors).is_err());
        assert!(enum_value("red_2").is_ok() && enum_value("2red").is_err());
    }

    #[test]
    fn with_the_bridge_world_info_stays_on_it() {
        let (world, seen) = game(|line| match line {
            "re_mcp:world" => json_reply(json!({"timeOfDay": 6000})),
            _ => bridge_basics(line).unwrap_or_else(|| failure(&format!("unexpected {line}"))),
        });
        let info = world_info(&world).unwrap();
        assert_eq!(info["source"], "bridge");
        assert_eq!(*seen.lock().unwrap(), ["re_mcp:ping", "re_mcp:world"]);
    }
}
