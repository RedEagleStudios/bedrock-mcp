//! Where pack files define names and where they refer to them. The places follow the file
//! formats in Microsoft Learn's add-on reference and the public vanilla packs
//! (bedrock-samples).

use super::{Content, Doc, Kind, PackKind, Reference, commands};
use crate::docs::jsonpos::{Node, Value};

/// A file names `kind` at `path` below a component (a key found at any depth, so component
/// groups and permutations count too). `*` steps into every member; an array on the way is
/// stepped through item by item.
struct Field {
    component: &'static str,
    path: &'static [&'static str],
    target: Target,
}

#[derive(Clone, Copy)]
enum Target {
    Name(Kind),
    /// A slash command, as in `run_command` or `queue_command`.
    Command,
}

const fn field(component: &'static str, path: &'static [&'static str], kind: Kind) -> Field {
    Field {
        component,
        path,
        target: Target::Name(kind),
    }
}

/// Event responses that run commands, in items, blocks and entities.
const COMMANDS: &[Field] = &[
    Field {
        component: "run_command",
        path: &["command"],
        target: Target::Command,
    },
    Field {
        component: "queue_command",
        path: &["command"],
        target: Target::Command,
    },
];

/// An item's icon: an item_texture.json key, alone or per variant. RP items (format 1.10)
/// use the same component.
const ICON_FIELDS: &[Field] = &[
    field("minecraft:icon", &[], Kind::ItemTexture),
    field("minecraft:icon", &["texture"], Kind::ItemTexture),
    field("minecraft:icon", &["textures", "*"], Kind::ItemTexture),
];

const ITEM_FIELDS: &[Field] = &[
    field("minecraft:block_placer", &["block"], Kind::Block),
    field("minecraft:block_placer", &["use_on"], Kind::Block),
    field("minecraft:entity_placer", &["entity"], Kind::Entity),
    field("minecraft:entity_placer", &["use_on"], Kind::Block),
    field("minecraft:entity_placer", &["dispense_on"], Kind::Block),
    field("minecraft:shooter", &["ammunition", "item"], Kind::Item),
    field("minecraft:projectile", &["projectile_entity"], Kind::Entity),
    field(
        "minecraft:repairable",
        &["repair_items", "items"],
        Kind::Item,
    ),
    field(
        "minecraft:digger",
        &["destroy_speeds", "block"],
        Kind::Block,
    ),
    field("minecraft:food", &["using_converts_to"], Kind::Item),
    field("minecraft:seed", &["crop_result"], Kind::Block),
];

const BLOCK_FIELDS: &[Field] = &[
    field(
        "minecraft:material_instances",
        &["*", "texture"],
        Kind::TerrainTexture,
    ),
    field("minecraft:geometry", &[], Kind::Geometry),
    field("minecraft:geometry", &["identifier"], Kind::Geometry),
    field("minecraft:loot", &[], Kind::LootTable),
    field(
        "minecraft:placement_filter",
        &["conditions", "block_filter"],
        Kind::Block,
    ),
];

const ENTITY_FIELDS: &[Field] = &[
    field("minecraft:loot", &["table"], Kind::LootTable),
    field("minecraft:equipment", &["table"], Kind::LootTable),
    field("minecraft:barter", &["barter_table"], Kind::LootTable),
    field("minecraft:trade_table", &["table"], Kind::TradeTable),
    field(
        "minecraft:economy_trade_table",
        &["table"],
        Kind::TradeTable,
    ),
    field(
        "minecraft:spawn_entity",
        &["entities", "spawn_entity"],
        Kind::Entity,
    ),
    field(
        "minecraft:spawn_entity",
        &["entities", "spawn_item"],
        Kind::Item,
    ),
    field(
        "minecraft:breedable",
        &["breeds_with", "mate_type"],
        Kind::Entity,
    ),
    field(
        "minecraft:breedable",
        &["breeds_with", "baby_type"],
        Kind::Entity,
    ),
    field("minecraft:breedable", &["breed_items"], Kind::Item),
    field("minecraft:transformation", &["into"], Kind::Entity),
    field("minecraft:shooter", &["def"], Kind::Entity),
    field("minecraft:tameable", &["tame_items"], Kind::Item),
    field("minecraft:healable", &["items", "item"], Kind::Item),
    field(
        "minecraft:interact",
        &["interactions", "spawn_items", "table"],
        Kind::LootTable,
    ),
    field(
        "minecraft:interact",
        &["interactions", "transform_to_item"],
        Kind::Item,
    ),
    field("minecraft:behavior.tempt", &["items"], Kind::Item),
    field("minecraft:ageable", &["feed_items"], Kind::Item),
];

const RECIPE_FIELDS: &[Field] = &[
    field("minecraft:recipe_shaped", &["key", "*"], Kind::Item),
    field("minecraft:recipe_shaped", &["result"], Kind::Item),
    field("minecraft:recipe_shapeless", &["ingredients"], Kind::Item),
    field("minecraft:recipe_shapeless", &["result"], Kind::Item),
    field("minecraft:recipe_furnace", &["input"], Kind::Item),
    field("minecraft:recipe_furnace", &["output"], Kind::Item),
    field("minecraft:recipe_brewing_container", &["input"], Kind::Item),
    field(
        "minecraft:recipe_brewing_container",
        &["reagent"],
        Kind::Item,
    ),
    field(
        "minecraft:recipe_brewing_container",
        &["output"],
        Kind::Item,
    ),
    // A brewing mix's input and output are potion types, not items.
    field("minecraft:recipe_brewing_mix", &["reagent"], Kind::Item),
    field(
        "minecraft:recipe_smithing_transform",
        &["template"],
        Kind::Item,
    ),
    field("minecraft:recipe_smithing_transform", &["base"], Kind::Item),
    field(
        "minecraft:recipe_smithing_transform",
        &["addition"],
        Kind::Item,
    ),
    field(
        "minecraft:recipe_smithing_transform",
        &["result"],
        Kind::Item,
    ),
    field("minecraft:recipe_smithing_trim", &["template"], Kind::Item),
    field("minecraft:recipe_smithing_trim", &["base"], Kind::Item),
    field("minecraft:recipe_smithing_trim", &["addition"], Kind::Item),
];

const SPAWN_RULE_FIELDS: &[Field] = &[
    field("minecraft:spawns_on_block_filter", &[], Kind::Block),
    field(
        "minecraft:spawns_on_block_prevented_filter",
        &[],
        Kind::Block,
    ),
    field(
        "minecraft:spawns_above_block_filter",
        &["blocks"],
        Kind::Block,
    ),
    field("minecraft:permute_type", &["entity_type"], Kind::Entity),
];

/// What a client entity (`entity/`) or attachable describes, below `description`.
const CLIENT_FIELDS: &[Field] = &[
    field("textures", &["*"], Kind::Texture),
    field("geometry", &["*"], Kind::Geometry),
    field("animations", &["*"], Kind::Animation),
    field("sound_effects", &["*"], Kind::Sound),
    field("particle_effects", &["*"], Kind::Particle),
    field("spawn_egg", &["texture"], Kind::ItemTexture),
];

/// The names `doc` refers to.
pub fn references(pack: PackKind, doc: &Doc) -> Vec<Reference> {
    let mut found = Vec::new();
    match &doc.content {
        Content::Function(text) => commands::function_references(text, &mut found),
        Content::Json(node) => match pack {
            PackKind::Behavior => behavior(doc.path, node, &mut found),
            PackKind::Resource => resource(doc.path, node, &mut found),
        },
    }
    found
}

fn behavior(path: &str, node: &Node, found: &mut Vec<Reference>) {
    let fields = |tables: &[&[Field]], found: &mut Vec<Reference>| {
        for table in tables {
            components(node, table, found);
        }
    };
    if path == "functions/tick.json" {
        names(node.get("values"), Kind::Function, found);
    } else if path.starts_with("items/") {
        fields(&[ICON_FIELDS, ITEM_FIELDS, COMMANDS], found);
    } else if path.starts_with("blocks/") {
        fields(&[BLOCK_FIELDS, COMMANDS], found);
    } else if path.starts_with("entities/") {
        fields(&[ENTITY_FIELDS, COMMANDS], found);
        let description = node.at(&["minecraft:entity", "description", "animations"]);
        for member in description.map_or(&[][..], Node::members) {
            names(Some(&member.value), Kind::BehaviorAnimation, found);
        }
    } else if path.starts_with("recipes/") {
        fields(&[RECIPE_FIELDS], found);
    } else if path.starts_with("loot_tables/") {
        loot_entries(node, found);
    } else if path.starts_with("trading/") {
        keyed(node, "item", Kind::Item, found);
    } else if path.starts_with("spawn_rules/") {
        let entity = node.at(&["minecraft:spawn_rules", "description", "identifier"]);
        names(entity, Kind::Entity, found);
        fields(&[SPAWN_RULE_FIELDS], found);
    } else if path.starts_with("animation_controllers/") {
        for controller in members(node.get("animation_controllers")) {
            for state in members(controller.get("states")) {
                for key in ["on_entry", "on_exit"] {
                    slash_commands(state.get(key), found);
                }
            }
        }
    } else if path.starts_with("animations/") {
        for animation in members(node.get("animations")) {
            for moment in members(animation.get("timeline")) {
                slash_commands(Some(moment), found);
            }
        }
    }
}

fn resource(path: &str, node: &Node, found: &mut Vec<Reference>) {
    match path {
        "textures/terrain_texture.json" | "textures/item_texture.json" => {
            for texture in members(node.get("texture_data")) {
                texture_paths(texture.get("textures"), found);
            }
        }
        "textures/flipbook_textures.json" => {
            for flipbook in node.items() {
                names(flipbook.get("flipbook_texture"), Kind::Texture, found);
                names(flipbook.get("atlas_tile"), Kind::TerrainTexture, found);
            }
        }
        "blocks.json" => {
            for block in node.members() {
                for key in ["textures", "carried_textures"] {
                    match block.value.get(key) {
                        Some(faces) if matches!(faces.value, Value::Object(_)) => {
                            for face in faces.members() {
                                names(Some(&face.value), Kind::TerrainTexture, found);
                            }
                        }
                        other => names(other, Kind::TerrainTexture, found),
                    }
                }
            }
        }
        "sounds.json" => sound_events(node, found),
        "sounds/sound_definitions.json" => {
            let definitions = node.get("sound_definitions").unwrap_or(node);
            for definition in definitions.members() {
                for sound in definition.value.get("sounds").map_or(&[][..], Node::items) {
                    match sound.get("name") {
                        Some(name) => names(Some(name), Kind::SoundFile, found),
                        None => names(Some(sound), Kind::SoundFile, found),
                    }
                }
            }
        }
        _ if path.starts_with("entity/") || path.starts_with("attachables/") => {
            client_description(node, found);
        }
        _ if path.starts_with("items/") => components(node, ICON_FIELDS, found),
        _ if path.starts_with("particles/") => {
            let texture = node.at(&[
                "particle_effect",
                "description",
                "basic_render_parameters",
                "texture",
            ]);
            // `atlas.terrain` and `atlas.items` name the game's texture atlases, not files.
            if !texture
                .and_then(Node::as_str)
                .is_some_and(|name| name.starts_with("atlas."))
            {
                names(texture, Kind::Texture, found);
            }
        }
        _ => {}
    }
}

/// A client entity's or attachable's description: its textures, models, render
/// controllers, animations, sounds and particles, and for a client entity the entity it
/// draws. An attachable's identifier is not checked: vanilla ones such as
/// `minecraft:diamond_boots.player` name no item.
fn client_description(node: &Node, found: &mut Vec<Reference>) {
    for root in ["minecraft:client_entity", "minecraft:attachable"] {
        let Some(description) = node.at(&[root, "description"]) else {
            continue;
        };
        if root == "minecraft:client_entity" {
            names(description.get("identifier"), Kind::Entity, found);
        }
        for field in CLIENT_FIELDS {
            if let Some(value) = description.get(field.component) {
                follow(value, field.path, field.target, found);
            }
        }
        for controller in description
            .get("render_controllers")
            .map_or(&[][..], Node::items)
        {
            match &controller.value {
                Value::String(name) => {
                    found.push(reference(Kind::RenderController, name, controller))
                }
                // `{"controller.render.x": "<condition>"}`
                Value::Object(members) => {
                    for member in members {
                        found.push(Reference {
                            kind: Kind::RenderController,
                            name: member.key.clone(),
                            pos: member.key_pos,
                        });
                    }
                }
                _ => {}
            }
        }
    }
}

/// `sounds.json`: every `sound` field, and `events` whose value is the sound itself.
fn sound_events(node: &Node, found: &mut Vec<Reference>) {
    for member in node.members() {
        match (member.key.as_str(), &member.value.value) {
            ("sound", Value::String(_)) => names(Some(&member.value), Kind::Sound, found),
            ("events", Value::Object(events)) => {
                for event in events {
                    if let Value::String(_) = event.value.value {
                        names(Some(&event.value), Kind::Sound, found);
                    } else {
                        sound_events(&event.value, found);
                    }
                }
            }
            _ => sound_events(&member.value, found),
        }
    }
}

/// The paths of a terrain_texture.json or item_texture.json entry: a path, a list of paths
/// or `{path}` objects, or one `{path, variations: [{path}]}`.
fn texture_paths(node: Option<&Node>, found: &mut Vec<Reference>) {
    let Some(node) = node else { return };
    match &node.value {
        Value::String(_) => names(Some(node), Kind::Texture, found),
        Value::Array(items) => items
            .iter()
            .for_each(|item| texture_paths(Some(item), found)),
        Value::Object(_) => {
            names(node.get("path"), Kind::Texture, found);
            texture_paths(node.get("variations"), found);
        }
        _ => {}
    }
}

/// Loot entries at any depth: `type: item` names an item, `type: loot_table` a table.
fn loot_entries(node: &Node, found: &mut Vec<Reference>) {
    let kind = match node.get("type").and_then(Node::as_str) {
        Some("item") => Some(Kind::Item),
        Some("loot_table") => Some(Kind::LootTable),
        _ => None,
    };
    if let Some(kind) = kind {
        names(node.get("name"), kind, found);
    }
    for child in children(node) {
        loot_entries(child, found);
    }
}

/// Every string under a `key` member at any depth.
fn keyed(node: &Node, key: &str, kind: Kind, found: &mut Vec<Reference>) {
    for member in node.members() {
        if member.key == key {
            names(Some(&member.value), kind, found);
        }
    }
    for child in children(node) {
        keyed(child, key, kind, found);
    }
}

/// Apply `table` to every member, at any depth, whose key is one of its components.
fn components(node: &Node, table: &[Field], found: &mut Vec<Reference>) {
    for member in node.members() {
        for field in table.iter().filter(|field| field.component == member.key) {
            follow(&member.value, field.path, field.target, found);
        }
    }
    for child in children(node) {
        components(child, table, found);
    }
}

fn follow(node: &Node, path: &[&str], target: Target, found: &mut Vec<Reference>) {
    if let Value::Array(items) = &node.value
        && !path.is_empty()
    {
        return items
            .iter()
            .for_each(|item| follow(item, path, target, found));
    }
    match (path.split_first(), target) {
        (None, Target::Name(kind)) => names(Some(node), kind, found),
        (None, Target::Command) => {
            for command in strings(node) {
                found.extend(commands::json_references(command));
            }
        }
        (Some((&"*", rest)), _) => {
            for member in node.members() {
                follow(&member.value, rest, target, found);
            }
        }
        (Some((key, rest)), _) => {
            if let Some(child) = node.get(key) {
                follow(child, rest, target, found);
            }
        }
    }
}

/// The names in `node`: a string, a list of them, or for items and blocks a descriptor
/// object (`{"item": ...}`, `{"name": ...}`); tags and Molang descriptors name nothing.
fn names(node: Option<&Node>, kind: Kind, found: &mut Vec<Reference>) {
    let Some(node) = node else { return };
    match &node.value {
        Value::String(name) => found.push(reference(kind, name, node)),
        Value::Array(items) => items.iter().for_each(|item| names(Some(item), kind, found)),
        Value::Object(_) if matches!(kind, Kind::Item | Kind::Block) => {
            names(node.get("item").or_else(|| node.get("name")), kind, found);
        }
        _ => {}
    }
}

fn reference(kind: Kind, name: &str, node: &Node) -> Reference {
    Reference {
        kind,
        name: name.to_owned(),
        pos: node.pos,
    }
}

/// The strings that start with `/` in a string or list: commands in animation controllers
/// and animation timelines, where other strings are events (`@s demo:go`) or Molang.
fn slash_commands(node: Option<&Node>, found: &mut Vec<Reference>) {
    for text in node.into_iter().flat_map(strings) {
        if text.1.trim_start().starts_with('/') {
            found.extend(commands::json_references(text));
        }
    }
}

/// A string node, or the string items of an array, with the node.
fn strings(node: &Node) -> Vec<(&Node, &str)> {
    match &node.value {
        Value::String(text) => vec![(node, text.as_str())],
        Value::Array(items) => items.iter().flat_map(strings).collect(),
        _ => Vec::new(),
    }
}

fn members(node: Option<&Node>) -> impl Iterator<Item = &Node> {
    node.map_or(&[][..], Node::members)
        .iter()
        .map(|member| &member.value)
}

fn children(node: &Node) -> Box<dyn Iterator<Item = &Node> + '_> {
    match &node.value {
        Value::Object(members) => Box::new(members.iter().map(|member| &member.value)),
        Value::Array(items) => Box::new(items.iter()),
        _ => Box::new(std::iter::empty()),
    }
}

/// A name a file defines by existing: a function, a loot or trade table, an image or a
/// sound file.
pub fn file_definition(pack: PackKind, path: &str) -> Option<(Kind, String)> {
    let extension = path
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase());
    match (pack, extension.as_deref()?) {
        (PackKind::Behavior, "mcfunction") => {
            let name = path
                .strip_prefix("functions/")?
                .strip_suffix(".mcfunction")?;
            Some((Kind::Function, name.to_owned()))
        }
        (PackKind::Behavior, "json") if path.starts_with("loot_tables/") => {
            Some((Kind::LootTable, path.to_owned()))
        }
        (PackKind::Behavior, "json") if path.starts_with("trading/") => {
            Some((Kind::TradeTable, path.to_owned()))
        }
        (PackKind::Resource, "png" | "tga" | "jpg" | "jpeg") => {
            Some((Kind::Texture, path.to_owned()))
        }
        (PackKind::Resource, "ogg" | "wav" | "fsb" | "mp3") => {
            Some((Kind::SoundFile, path.to_owned()))
        }
        _ => None,
    }
}

/// The names a JSON file defines.
pub fn definitions(pack: PackKind, path: &str, node: &Node) -> Vec<(Kind, String)> {
    let mut found = Vec::new();
    let mut add = |kind: Kind, name: Option<&str>| {
        if let Some(name) = name {
            found.push((kind, name.to_owned()));
        }
    };
    let identifier = |root: &str| {
        node.at(&[root, "description", "identifier"])
            .and_then(Node::as_str)
    };
    let keys = |key: &str| {
        node.get(key)
            .map_or(&[][..], Node::members)
            .iter()
            .map(|member| member.key.as_str())
    };
    match pack {
        PackKind::Behavior if path.starts_with("items/") => {
            add(Kind::Item, identifier("minecraft:item"))
        }
        PackKind::Behavior if path.starts_with("blocks/") => {
            add(Kind::Block, identifier("minecraft:block"));
            add(Kind::Item, identifier("minecraft:block"));
        }
        PackKind::Behavior if path.starts_with("entities/") => {
            let entity = identifier("minecraft:entity");
            add(Kind::Entity, entity);
            // The game adds a spawn egg item for every entity.
            add(
                Kind::Item,
                entity
                    .map(|entity| format!("{entity}_spawn_egg"))
                    .as_deref(),
            );
        }
        PackKind::Behavior if path.starts_with("animations/") => {
            keys("animations").for_each(|name| add(Kind::BehaviorAnimation, Some(name)));
        }
        PackKind::Behavior if path.starts_with("animation_controllers/") => {
            keys("animation_controllers").for_each(|name| add(Kind::BehaviorAnimation, Some(name)));
        }
        PackKind::Resource => match path {
            "textures/terrain_texture.json" => {
                keys("texture_data").for_each(|name| add(Kind::TerrainTexture, Some(name)));
            }
            "textures/item_texture.json" => {
                keys("texture_data").for_each(|name| add(Kind::ItemTexture, Some(name)));
            }
            "sounds/sound_definitions.json" => {
                let definitions = node.get("sound_definitions").unwrap_or(node);
                for member in definitions.members() {
                    if matches!(member.value.value, Value::Object(_)) {
                        add(Kind::Sound, Some(&member.key));
                    }
                }
            }
            _ if path.starts_with("models/") => {
                for geometry in node.get("minecraft:geometry").map_or(&[][..], Node::items) {
                    add(
                        Kind::Geometry,
                        geometry
                            .at(&["description", "identifier"])
                            .and_then(Node::as_str),
                    );
                }
                // The 1.8 and 1.10 formats: `"geometry.name:geometry.parent": {...}`.
                for member in node
                    .members()
                    .iter()
                    .filter(|member| member.key.starts_with("geometry."))
                {
                    add(Kind::Geometry, member.key.split(':').next());
                }
            }
            _ if path.starts_with("render_controllers/") => {
                keys("render_controllers").for_each(|name| add(Kind::RenderController, Some(name)));
            }
            _ if path.starts_with("animations/") => {
                keys("animations").for_each(|name| add(Kind::Animation, Some(name)));
            }
            _ if path.starts_with("animation_controllers/") => {
                keys("animation_controllers").for_each(|name| add(Kind::Animation, Some(name)));
            }
            _ if path.starts_with("particles/") => {
                add(Kind::Particle, identifier("particle_effect"));
            }
            _ => {}
        },
        _ => {}
    }
    found
}
