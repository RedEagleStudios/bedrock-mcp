//! Native world reads through the unstable layer's `world_state` capability: entities, the
//! player and dynamic properties straight from the game's local server, without the Bridge
//! pack, cheats or the Beta APIs experiment.

use protocol::unstable::{
    NativeEntity, NativePlayer, PropertySnapshot, PropertyTarget, PropertyValue, Request,
};
use serde_json::{Value, json};

use super::Session;
use crate::{
    bridge::{Actor, labelled},
    error::{Error, Result, bail},
};

const CAPABILITY: &str = "world_state";

/// What the native read leaves out of `entity_inspect`, which the Bridge pack answers.
const INSPECT_NOTE: &str = "Read natively from the game's local server (the bedrock-mcp Bridge \
     pack is not active): eye, velocity, viewDirection, families and flags need the pack.";
const LIST_NOTE: &str = "Read natively from the game's local server (the bedrock-mcp Bridge \
     pack is not active): tags need the pack. Pages come from the list read on the first call, \
     so later pages do not see entities move.";
/// What the native player read leaves out of `player_get`, which the Bridge pack answers.
const PLAYER_NOTE: &str = "Read natively from the game's local server (the bedrock-mcp \
     Bridge pack is not active): velocity, flags, spawnPoint and level need the pack.";

/// Effect names by the game's numeric id (`MobEffectIds` in LeviLamina's
/// `src/mc/world/effect/MobEffectIds.h`, from 1), spelled as the vanilla effect list in
/// Mojang's bedrock-samples (`metadata/vanilladata_modules/mojang-effects.json`) names them.
const EFFECTS: [&str; 38] = [
    "speed",
    "slowness",
    "haste",
    "mining_fatigue",
    "strength",
    "instant_health",
    "instant_damage",
    "jump_boost",
    "nausea",
    "regeneration",
    "resistance",
    "fire_resistance",
    "water_breathing",
    "invisibility",
    "blindness",
    "night_vision",
    "hunger",
    "weakness",
    "poison",
    "wither",
    "health_boost",
    "absorption",
    "saturation",
    "levitation",
    "fatal_poison",
    "conduit_power",
    "slow_falling",
    "bad_omen",
    "village_hero",
    "darkness",
    "trial_omen",
    "wind_charged",
    "weaving",
    "oozing",
    "infested",
    "raid_omen",
    "breath_of_the_nautilus",
    "freezing",
];

/// Which entities a native list wants.
pub struct EntityFilter<'a> {
    pub player: Option<&'a str>,
    pub radius: f64,
    pub entity_type: Option<&'a str>,
}

fn round(value: f32) -> f64 {
    (f64::from(value) * 1000.0).round() / 1000.0
}

fn location(position: [f32; 3]) -> Value {
    json!({ "x": round(position[0]), "y": round(position[1]), "z": round(position[2]) })
}

fn dimension(number: i32) -> String {
    match number {
        0 => "minecraft:overworld".into(),
        1 => "minecraft:nether".into(),
        2 => "minecraft:the_end".into(),
        other => format!("dimension {other}"),
    }
}

fn health(entity: &NativeEntity) -> Value {
    entity.health.map_or(
        Value::Null,
        |[current, max]| json!({ "current": round(current), "max": round(max) }),
    )
}

/// A list entry, shaped like the Bridge pack's.
fn entity_entry(entity: &NativeEntity) -> Value {
    json!({
        "id": entity.id.to_string(),
        "type": entity.type_id,
        "name": entity.name_tag.as_deref().filter(|name| !name.is_empty()),
        "location": location(entity.position),
        "distance": entity.distance.map(round),
        "health": health(entity),
    })
}

/// `entity_inspect`'s fields that the native read knows.
fn entity_detail(entity: &NativeEntity) -> Value {
    json!({
        "id": entity.id.to_string(),
        "type": entity.type_id,
        "name": entity.name_tag.as_deref().filter(|name| !name.is_empty()),
        "nameTag": entity.name_tag.clone().unwrap_or_default(),
        "player": entity.player,
        "dimension": entity.dimension.map(dimension),
        "location": location(entity.position),
        "yaw": entity.rotation.map(|[_, yaw]| round(yaw)),
        "pitch": entity.rotation.map(|[pitch, _]| round(pitch)),
        "health": health(entity),
    })
}

/// An effect id as `minecraft:<name>`, or `effect <id>` for one the table does not know.
fn effect_name(id: u32) -> String {
    match id
        .checked_sub(1)
        .and_then(|index| EFFECTS.get(index as usize))
    {
        Some(name) => format!("minecraft:{name}"),
        None => format!("effect {id}"),
    }
}

/// `player_get`'s fields that the native player read knows, named as the Bridge pack names
/// them; `saturation` is extra.
pub fn player_detail(player: &NativePlayer) -> Value {
    let entity = &player.entity;
    json!({
        "name": entity.name_tag.as_deref().filter(|name| !name.is_empty()),
        "id": entity.id.to_string(),
        "dimension": entity.dimension.map(dimension),
        "location": location(entity.position),
        "yaw": entity.rotation.map(|[_, yaw]| round(yaw)),
        "pitch": entity.rotation.map(|[pitch, _]| round(pitch)),
        "health": health(entity),
        "hunger": player.hunger.map(|[current, _]| round(current)),
        "saturation": player.saturation.map(|[current, _]| round(current)),
        "selectedSlot": player.selected_slot,
        "effects": player.effects.as_ref().map(|effects| {
            effects
                .iter()
                .map(|effect| {
                    json!({
                        "id": effect_name(effect.id),
                        "amplifier": effect.amplifier,
                        "ticks": effect.duration,
                    })
                })
                .collect::<Vec<_>>()
        }),
    })
}

fn property_value(value: &PropertyValue) -> Value {
    match value {
        PropertyValue::Double(number) => json!({ "type": "double", "value": number }),
        PropertyValue::Float(number) => json!({ "type": "float", "value": number }),
        PropertyValue::Bool(flag) => json!({ "type": "boolean", "value": flag }),
        PropertyValue::String(text) => json!({ "type": "string", "value": text }),
        PropertyValue::Vector([x, y, z]) => {
            json!({ "type": "vector", "value": { "x": x, "y": y, "z": z } })
        }
    }
}

fn properties_json(snapshot: &PropertySnapshot) -> Value {
    let collections: serde_json::Map<String, Value> = snapshot
        .collections
        .iter()
        .map(|collection| {
            let properties: serde_json::Map<String, Value> = collection
                .properties
                .iter()
                .map(|property| (property.key.clone(), property_value(&property.value)))
                .collect();
            (
                collection.name.clone(),
                json!({ "byteCount": collection.byte_count, "properties": properties }),
            )
        })
        .collect();
    json!({
        "entity": snapshot.entity.map(|id| id.to_string()),
        "collections": collections,
        "truncated": snapshot.truncated,
        "tick": snapshot.tick,
        "readMicros": snapshot.read_us,
    })
}

/// A decimal entity id, as `Entity.id` spells it.
pub fn parse_entity_id(id: &str) -> Result<i64> {
    match id.parse() {
        Ok(id) => Ok(id),
        Err(_) => bail!("Entity id {id:?} is not an entity id; use an id from entities_list."),
    }
}

impl Session {
    /// Whether the native world reads can answer now; otherwise the reason goes to `notes`.
    pub(crate) fn native_world(&mut self, notes: &mut Vec<String>) -> bool {
        self.connect_running_agent();
        self.unstable_capability(CAPABILITY, notes)
    }

    /// The player `actor` names (the only player for `Actor::Me`) on the game's local
    /// server; fails with why when the world_state capability cannot answer.
    pub(crate) fn native_player(&mut self, actor: &Actor) -> Result<NativePlayer> {
        let mut notes = Vec::new();
        if !self.native_world(&mut notes) {
            return Err(Error::msg(notes.join("; ")));
        }
        let name = match actor {
            Actor::Me => None,
            Actor::Named(name) => Some(name.clone()),
        };
        self.unstable.world_player(name)
    }

    /// The native player read as `player_get` fields, with its notes; None, with the
    /// reason in `notes`, when it cannot answer.
    pub(crate) fn native_player_state(
        &mut self,
        actor: &Actor,
        notes: &mut Vec<String>,
    ) -> Option<Value> {
        match self.native_player(actor) {
            Ok(player) => {
                notes.push(PLAYER_NOTE.to_owned());
                notes.extend(
                    player
                        .notes
                        .iter()
                        .map(|note| format!("Native read: {note}.")),
                );
                Some(player_detail(&player))
            }
            Err(error) => {
                notes.push(format!("The native player read could not answer: {error}."));
                None
            }
        }
    }

    /// `game_status`'s world block with the native player read added as `player` when the
    /// Bridge pack did not answer it: `{...the command snapshot, player}`, or `{player}` without
    /// the WebSocket. Left as it is when the native read cannot answer.
    pub(crate) fn with_native_player(&mut self, world: Option<Value>) -> Option<Value> {
        if world.as_ref().is_some_and(|world| world["bridge"] == true) {
            return world;
        }
        let mut notes = Vec::new();
        let Some(player) = self.native_player_state(&Actor::Me, &mut notes) else {
            return world;
        };
        let mut world = world.unwrap_or_else(|| json!({}));
        world["player"] = labelled("native", notes, player);
        Some(world)
    }

    /// Entities around a player, nearest first, without that player: (entries, notes).
    pub(crate) fn native_entities(
        &mut self,
        filter: &EntityFilter<'_>,
    ) -> Result<(Vec<Value>, Vec<String>)> {
        let snapshot = self.unstable.world_entities(Request::WorldEntities {
            id: None,
            player: filter.player.map(str::to_owned),
            radius: Some(filter.radius),
            entity_type: filter.entity_type.map(str::to_owned),
            limit: Some(4096),
        })?;
        let mut notes = vec![LIST_NOTE.to_owned()];
        match (&snapshot.centre, filter.player) {
            (None, Some(player)) => {
                bail!("No player named {player} is on the game's local server.")
            }
            (None, None) => bail!("No player is on the game's local server to measure from."),
            _ => {}
        }
        if snapshot.total as usize > snapshot.entities.len() {
            notes.push(format!(
                "The native read keeps the nearest {} of {} entities; farther ones are left out.",
                snapshot.entities.len(),
                snapshot.total
            ));
        }
        if snapshot.skipped > 0 {
            notes.push(format!(
                "{} entities failed the native read's layout checks and are left out.",
                snapshot.skipped
            ));
        }
        Ok((snapshot.entities.iter().map(entity_entry).collect(), notes))
    }

    /// One entity by id, labelled `native`.
    pub(crate) fn native_entity(&mut self, id: &str) -> Result<Value> {
        let id = parse_entity_id(id)?;
        let snapshot = self.unstable.world_entities(Request::WorldEntities {
            id: Some(id),
            player: None,
            radius: None,
            entity_type: None,
            limit: Some(1),
        })?;
        let Some(entity) = snapshot.entities.first() else {
            bail!(
                "No entity with id {id} is on the game's local server: it died, despawned, or \
                 the id is wrong. entities_list shows the current ids."
            );
        };
        Ok(labelled(
            "native",
            vec![INSPECT_NOTE.to_owned()],
            entity_detail(entity),
        ))
    }

    /// `dynamic_properties_get`: every pack's collections on the world, a player or an entity.
    pub(crate) fn dynamic_properties_get(
        &mut self,
        target: &str,
        player: Option<&str>,
        collection: Option<String>,
    ) -> Result<Value> {
        let mut notes = Vec::new();
        if !self.native_world(&mut notes) {
            bail!(
                "dynamic_properties_get needs the unstable layer's world_state capability: {}",
                notes.join("; ")
            );
        }
        let target = match target {
            "world" => PropertyTarget::World,
            "player" => PropertyTarget::Player {
                name: player.map(str::to_owned),
            },
            id => PropertyTarget::Entity {
                id: parse_entity_id(id)?,
            },
        };
        let snapshot = self.unstable.dynamic_properties(target, collection)?;
        if snapshot.truncated {
            notes.push(
                "Some properties are left out: a size bound cut the read, or a number was not \
                 finite."
                    .to_owned(),
            );
        }
        Ok(labelled("native", notes, properties_json(&snapshot)))
    }
}

#[cfg(test)]
mod tests {
    use protocol::unstable::{Property, PropertyCollection};

    use super::*;

    #[test]
    fn properties_are_keyed_by_collection_and_typed() {
        let snapshot = PropertySnapshot {
            tick: 3,
            entity: Some(-5),
            collections: vec![PropertyCollection {
                name: "pack".into(),
                byte_count: 12,
                properties: vec![
                    Property {
                        key: "form".into(),
                        value: PropertyValue::String("wolf".into()),
                    },
                    Property {
                        key: "spot".into(),
                        value: PropertyValue::Vector([1.0, 2.0, 3.0]),
                    },
                ],
            }],
            truncated: false,
            read_us: 9,
        };
        let value = properties_json(&snapshot);
        assert_eq!(value["entity"], "-5");
        assert_eq!(
            value["collections"]["pack"]["properties"]["form"],
            json!({ "type": "string", "value": "wolf" })
        );
        assert_eq!(
            value["collections"]["pack"]["properties"]["spot"]["value"],
            json!({ "x": 1.0, "y": 2.0, "z": 3.0 })
        );
    }

    #[test]
    fn entity_entries_match_the_bridge_shape() {
        let entity = NativeEntity {
            id: -4_294_967_295,
            type_id: Some("minecraft:cow".into()),
            name_tag: Some(String::new()),
            player: false,
            dimension: Some(1),
            position: [1.25, 64.0, -3.5],
            rotation: Some([10.0, 90.0]),
            health: Some([7.5, 10.0]),
            distance: Some(2.0),
        };
        let entry = entity_entry(&entity);
        assert_eq!(entry["id"], "-4294967295");
        assert_eq!(entry["name"], Value::Null);
        assert_eq!(
            entry["location"],
            json!({ "x": 1.25, "y": 64.0, "z": -3.5 })
        );
        assert_eq!(entry["health"], json!({ "current": 7.5, "max": 10.0 }));
        let detail = entity_detail(&entity);
        assert_eq!(detail["dimension"], "minecraft:nether");
        assert_eq!(
            (detail["yaw"].clone(), detail["pitch"].clone()),
            (json!(90.0), json!(10.0))
        );
    }

    #[test]
    fn native_players_match_the_bridge_shape() {
        let mut player = crate::unstable::fake::native_player();
        let detail = player_detail(&player);
        assert_eq!(
            detail["location"],
            json!({ "x": 1.5, "y": 64.0, "z": -2.5 })
        );
        assert_eq!(detail["selectedSlot"], 2);
        assert_eq!(detail["hunger"], 17.0);
        assert_eq!(
            detail["effects"],
            json!([{ "id": "minecraft:speed", "amplifier": 1, "ticks": 600 }])
        );
        player.effects = None;
        player.selected_slot = None;
        let detail = player_detail(&player);
        assert_eq!(detail["effects"], Value::Null);
        assert_eq!(detail["selectedSlot"], Value::Null);
        assert_eq!(effect_name(38), "minecraft:freezing");
        assert_eq!(effect_name(99), "effect 99");
    }
}
