//! The warm-up before a play-test: after a world loads, the held item's attachable can play no
//! animations until the selected slot changes and the player moves. The warm-up selects another
//! hotbar slot and back, walks about a block forward and back, puts the player back where it
//! stood facing the same way, and checks the attachable with the unstable layer's
//! player_render when it is active; once more when it still does not animate.
//!
//! With the Bridge pack it reads the player and inventory there and puts the player back with
//! a Script API teleport. Without it, it reads the position and selected slot natively from
//! the game's local server (single player), takes the next slot without reading the
//! inventory, confirms each slot change with a native read and puts the player back with `/tp`
//! (cheats).

use serde_json::{Value, json};

use super::{CONFIRM_TIMEOUT, Game, Request, perform, point_of};
use crate::{
    bridge::quoted,
    error::{Result, bail},
    world::{World, distance, number, shortest_turn},
};

/// How long each walk of the warm-up holds a movement key.
const WALK_MS: u64 = 250;
/// How far from the start the player may end before it is put back.
const DRIFT: f64 = 0.2;
/// How many warm-ups run at most while the attachable does not animate.
const ATTEMPTS: usize = 2;
/// Native reads (one server tick each) a slot change gets to show.
const SLOT_READS: usize = 5;

/// Where the warm-up reads the player and how it puts the player back.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Bridge,
    Native,
}

impl Source {
    fn name(self) -> &'static str {
        match self {
            Self::Bridge => "bridge",
            Self::Native => "native",
        }
    }
}

/// What one read of the player gives the warm-up.
struct Spot {
    location: Option<[f64; 3]>,
    yaw: Option<f64>,
    pitch: Option<f64>,
    slot: Option<u64>,
}

fn read_spot(game: &mut dyn Game, world: &World, source: Source) -> Result<Spot> {
    Ok(match source {
        Source::Bridge => {
            let state = world.bridge_command("re_mcp:state")?;
            Spot {
                location: point_of(&state["location"]),
                yaw: state["yaw"].as_f64(),
                pitch: state["pitch"].as_f64(),
                slot: state["selectedSlot"].as_u64(),
            }
        }
        Source::Native => {
            let player = game.native_player()?;
            let [x, y, z] = player.entity.position.map(f64::from);
            Spot {
                location: Some([x, y, z]),
                yaw: player.entity.rotation.map(|[_, yaw]| f64::from(yaw)),
                pitch: player.entity.rotation.map(|[pitch, _]| f64::from(pitch)),
                slot: player.selected_slot.map(u64::from),
            }
        }
    })
}

/// When an action_sequence warms up: always, never, or `auto` (when it records, or when the
/// connected player has not warmed up since the world loaded).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WarmUp {
    On,
    Off,
    #[default]
    Auto,
}

impl WarmUp {
    /// `true`, `false` or `"auto"`; missing is auto.
    pub fn parse(value: Option<&Value>) -> Result<Self> {
        Ok(match value {
            None | Some(Value::Null) => Self::Auto,
            Some(Value::Bool(true)) => Self::On,
            Some(Value::Bool(false)) => Self::Off,
            Some(Value::String(text)) if text == "auto" => Self::Auto,
            Some(other) => bail!("warm_up must be true, false or \"auto\", not {other}."),
        })
    }

    /// Whether a run warms up; `record` says whether it records.
    pub fn wanted(self, game: &mut dyn Game, record: bool) -> bool {
        match self {
            Self::On => true,
            Self::Off => false,
            Self::Auto => {
                record || {
                    let now = game.world_key();
                    now.is_some() && game.warm_key() != now
                }
            }
        }
    }
}

/// Run the warm-up for the connected player; its report `{done, swapped_slot, walked,
/// attachable, attempts, detail}`. Remembers the world it warmed up in.
pub fn warm_up(game: &mut dyn Game) -> Value {
    match run(game) {
        Ok(report) => {
            if report["done"] == true {
                let key = game.world_key();
                game.set_warm_key(key);
            }
            report
        }
        Err(error) => json!({
            "done": false,
            "swapped_slot": null,
            "walked": false,
            "attachable": null,
            "detail": format!("The warm-up failed: {error}"),
        }),
    }
}

fn act(game: &mut dyn Game, action: &str, args: Value) -> Result<bool> {
    Ok(perform(game, &Request::parse(action, args)?, CONFIRM_TIMEOUT)?.confirmed)
}

fn not_done(detail: String) -> Value {
    json!({
        "done": false,
        "source": null,
        "swapped_slot": null,
        "walked": false,
        "attachable": null,
        "detail": detail,
    })
}

/// Select `slot`; with the native source, confirmed only once a native read shows it.
fn select(game: &mut dyn Game, source: Source, slot: u64) -> Result<bool> {
    let confirmed = act(game, "hotbar_select", json!({ "slot": slot }))?;
    if source == Source::Bridge {
        return Ok(confirmed);
    }
    for _ in 0..SLOT_READS {
        if game.native_player()?.selected_slot.map(u64::from) == Some(slot) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn run(game: &mut dyn Game) -> Result<Value> {
    let world = game.world()?;
    let mut notes = Vec::new();
    let source = if world.bridge(true)? {
        Source::Bridge
    } else {
        match game.native_player() {
            Ok(_) => {
                notes.push(
                    "The Bridge pack is not active, so the warm-up read the player natively from \
                     the game's local server and puts it back with /tp (cheats)."
                        .to_owned(),
                );
                Source::Native
            }
            Err(error) => {
                return Ok(not_done(format!(
                    "The warm-up reads the slot and position from the Bridge pack, which is not \
                     active, or natively from the unstable layer, which could not answer: {error}"
                )));
            }
        }
    };
    let start = read_spot(game, &world, source)?;
    // Another slot: with the pack, the next one holding a different item (or nothing).
    let selected = start.slot.map(|slot| slot.min(8));
    let other = match (source, selected) {
        (Source::Bridge, selected) => {
            let selected = selected.unwrap_or(0);
            let inventory = world.bridge_command("re_mcp:inventory")?;
            let item_in = |slot: u64| {
                inventory["slots"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|entry| entry["slot"].as_u64() == Some(slot))
                    .map(|entry| entry["id"].clone())
                    .unwrap_or(Value::Null)
            };
            let held = item_in(selected);
            Some(
                (1..9)
                    .map(|step| (selected + step) % 9)
                    .find(|slot| item_in(*slot) != held)
                    .unwrap_or((selected + 1) % 9),
            )
        }
        (Source::Native, Some(selected)) => {
            notes.push(format!(
                "Without the pack the inventory is not read, so the warm-up took the next slot \
                 ({}), which may hold the same item.",
                (selected + 1) % 9
            ));
            Some((selected + 1) % 9)
        }
        (Source::Native, None) => {
            notes.push(
                "The native read gave no selected slot, so the warm-up did not change slots: it \
                 could not select the player's slot again afterwards."
                    .to_owned(),
            );
            None
        }
    };
    let mut attachable = Value::Null;
    let mut attempts = 0;
    let mut walked = false;
    let mut swapped = other.is_some();
    while attempts < ATTEMPTS {
        attempts += 1;
        if let (Some(other), Some(selected)) = (other, selected.or(Some(0))) {
            swapped &= select(game, source, other)?;
            swapped &= select(game, source, selected)?;
        }
        act(
            game,
            "movement_set",
            json!({ "forward": 1, "duration_ms": WALK_MS }),
        )?;
        act(
            game,
            "movement_set",
            json!({ "forward": -1, "duration_ms": WALK_MS }),
        )?;
        walked = true;
        // Put the player back where it stood, facing the same way.
        let now = read_spot(game, &world, source)?;
        if let (Some(start), Some(at), Some(yaw), Some(pitch)) =
            (start.location, now.location, start.yaw, start.pitch)
        {
            let turned = now
                .yaw
                .is_none_or(|now| shortest_turn(now, yaw).abs() > 1.0);
            let drift = distance(start, at);
            if drift > DRIFT || turned {
                let [x, y, z] = start.map(number);
                let (yaw, pitch) = (number(yaw), number(pitch));
                let how = match source {
                    Source::Bridge => {
                        let spec = format!("x={x};y={y};z={z};yaw={yaw};pitch={pitch}");
                        world.bridge_write(&format!("re_mcp:player_set {}", quoted(&spec)?))?;
                        "a Script API teleport"
                    }
                    Source::Native => {
                        world.command(&format!("tp @s {x} {y} {z} {yaw} {pitch}"))?;
                        "/tp"
                    }
                };
                let note = format!(
                    "The walk ended {drift:.2} blocks from the start, so the player was put back \
                     with {how}."
                );
                if !notes.contains(&note) {
                    notes.push(note);
                }
            }
        } else {
            let note = "The player's start position or rotation was not read, so it was not put \
                        back."
                .to_owned();
            if !notes.contains(&note) {
                notes.push(note);
            }
        }
        match game.attachable() {
            Ok(Some(found)) => {
                let animating = found["animating"] == true;
                // With nothing in hand there is no attachable to warm, so a second round
                // would change nothing.
                let none = found["attachables"].as_u64() == Some(0);
                attachable = found;
                if animating || none {
                    break;
                }
            }
            Ok(None) => {
                notes.push(
                    "The unstable layer's player_render capability is not active, so the \
                     attachable was not checked."
                        .to_owned(),
                );
                break;
            }
            Err(error) => {
                notes.push(format!("player_render could not be read: {error}"));
                break;
            }
        }
    }
    if other.is_some() && !swapped && source == Source::Native {
        notes.push(
            "A native read did not show a slot change within 5 server ticks after its key press."
                .to_owned(),
        );
    }
    let animating = attachable["animating"].as_bool();
    if animating == Some(false) {
        notes.push(match attachable["attachables"].as_u64() {
            Some(0) => "The player shows no attachable: the held item may have none.".to_owned(),
            _ => "The attachable still plays no animation after the second warm-up.".to_owned(),
        });
    }
    Ok(json!({
        "done": swapped && walked,
        "source": source.name(),
        "swapped_slot": other,
        "walked": walked,
        "attachable": attachable,
        "attempts": attempts,
        "detail": notes.join(" "),
    }))
}

/// A warm-up report as the default replies give it: `{done, detail}`, with detail one line
/// on what it did, whether the held item's attachable animates, and why not.
pub fn warm_summary(report: &Value) -> Value {
    let mut line = Vec::new();
    if report["done"] == true {
        line.push(format!(
            "Selected slot {} and back and walked a block and back.",
            report["swapped_slot"]
        ));
    }
    if report["attachable"]["animating"] == true {
        line.push("The held item's attachable animates.".to_owned());
    }
    if let Some(detail) = report["detail"]
        .as_str()
        .filter(|detail| !detail.is_empty())
    {
        line.push(detail.to_owned());
    }
    json!({ "done": report["done"], "detail": line.join(" ") })
}
