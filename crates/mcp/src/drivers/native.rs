//! Melee and kills without the Bridge pack, through the unstable layer's native entity read
//! (`world_state`, single player: it reads the game's local server). An attack reads its
//! target's health right before the swing and again for a short window after it: a drop
//! confirms a hit, a read of 0 or the entity gone confirms a kill. While a sequence runs, a
//! [`Tracker`] reads the health of the entities its steps target and turns each change into a
//! `native` event in the merged stream (`entity_hurt`, `entity_die`), so `until`, `stop_when`,
//! `wait_for` and the summary's `killed_by` work as they do with the Bridge pack's events.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{Game, Outcome, TICK};
use crate::error::{Result, bail};

/// One entity as a native read saw it.
#[derive(Clone, Debug, PartialEq)]
pub struct Vitals {
    /// The type identifier, e.g. `minecraft:zombie`.
    pub kind: Option<String>,
    /// Current and maximum health, for entities with a health attribute.
    pub health: Option<[f64; 2]>,
}

/// What a native read of one entity found.
#[derive(Clone, Debug, PartialEq)]
pub enum NativeRead {
    /// The unstable layer cannot read now; why.
    Unavailable(String),
    /// The server tick that answered, and the entity: `None` when no entity with that id is
    /// on the game's local server.
    Read { tick: u64, entity: Option<Vitals> },
}

fn round(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn health_json([current, max]: [f64; 2]) -> Value {
    json!({ "current": round(current), "max": round(max) })
}

/// An entity's id and type as an event's actor field.
fn entity_json(id: &str, kind: &Option<String>) -> Value {
    json!({ "id": id, "type": kind })
}

/// How a hit was read back.
const HIT_NOTE: &str = "Confirmed by reading the target's health natively before and after the \
     swing (the Bridge pack is not active). The read does not name the attacker, so other \
     damage in the same window counts too.";

/// An attack's health readback: the read before the swing, then reads after it.
pub struct Readback {
    id: String,
    kind: Option<String>,
    /// Health before the swing: current and maximum.
    before: [f64; 2],
    started: Instant,
    window: Duration,
    polled: Option<Instant>,
}

impl Readback {
    /// Read entity `id` before the swing. `Ok(Err(reason))` when the native read cannot
    /// confirm this attack, so the caller falls back; an error when the entity is gone.
    pub fn before(
        game: &mut dyn Game,
        id: &str,
        window: Duration,
        outcome: &mut Outcome,
    ) -> Result<std::result::Result<Self, String>> {
        let (tick, vitals) = match game.native_entity(id) {
            NativeRead::Unavailable(reason) => return Ok(Err(reason)),
            NativeRead::Read { tick, entity } => (tick, entity),
        };
        let Some(vitals) = vitals else {
            bail!(
                "No entity with id {id} is on the game's local server: it died, despawned, or the \
                 id is wrong."
            );
        };
        let Some(before) = vitals.health else {
            return Ok(Err(format!("entity {id} has no health to read natively")));
        };
        outcome.evidence["acted_tick"] = tick.into();
        Ok(Ok(Self {
            id: id.to_owned(),
            kind: vitals.kind,
            before,
            started: Instant::now(),
            window,
            polled: None,
        }))
    }

    /// Read again, at most once a tick; the finished outcome once the health dropped, the
    /// entity died or went away, or the window passed.
    pub fn poll(&mut self, game: &mut dyn Game, outcome: &mut Outcome) -> Option<()> {
        if self.polled.is_some_and(|at| at.elapsed() < TICK) {
            return None;
        }
        self.polled = Some(Instant::now());
        let over = self.started.elapsed() >= self.window;
        let [before, max] = self.before;
        outcome.source = "native";
        outcome.evidence["target"] = entity_json(&self.id, &self.kind);
        let (tick, entity) = match game.native_entity(&self.id) {
            NativeRead::Unavailable(reason) => {
                outcome.reason = Some(("no_event", format!("The native read stopped: {reason}")));
                outcome
                    .notes
                    .push(format!("The health readback stopped: {reason}."));
                return Some(());
            }
            NativeRead::Read { tick, entity } => (tick, entity),
        };
        let after = entity.as_ref().and_then(|entity| entity.health);
        let killed = match after {
            None => entity.is_none().then_some("gone"),
            Some([current, _]) if current <= 0.0 => Some("health_zero"),
            Some(_) => None,
        };
        let dropped = after.is_some_and(|[current, _]| current < before);
        if killed.is_none() && !dropped && !over {
            return None;
        }
        outcome.evidence["health"] = json!({
            "before": round(before),
            "after": after.map(|[current, _]| round(current)),
            "max": round(max),
        });
        outcome.evidence["read_tick"] = tick.into();
        if let Some(seen) = killed {
            outcome.evidence["killed"] = true.into();
            outcome.evidence["seen"] = seen.into();
        }
        if killed.is_some() || dropped {
            outcome.confirmed = true;
            outcome.tick = Some(tick);
            outcome.notes.push(HIT_NOTE.to_owned());
            if killed == Some("gone") {
                outcome.notes.push(
                    "The target was gone from the local server after the swing; a despawn in the \
                     same window would read the same."
                        .to_owned(),
                );
            }
        } else {
            let ms = self.window.as_millis();
            outcome.reason = Some((
                "no_damage",
                format!(
                    "Health stayed at {} of {} for {ms} ms after the swing",
                    round(before),
                    round(max)
                ),
            ));
            outcome.notes.push(format!(
                "The target's health, read natively, stayed at {} of {} for {ms} ms after the \
                 swing: a miss, out of reach, its short invulnerability after a recent hit, or \
                 armor or resistance took all the damage.",
                round(before),
                round(max)
            ));
        }
        Some(())
    }
}

/// One entity a sequence tracks.
struct Tracked {
    id: String,
    kind: Option<String>,
    health: Option<[f64; 2]>,
    /// A read showed it at least once.
    seen: bool,
    /// Its death was reported; it is read no more.
    dead: bool,
}

/// Reads the health of the entities a sequence targets, about once a tick, and reports each
/// drop as an `entity_hurt` event and each death (health 0, or gone after a read showed it)
/// as an `entity_die` event, both with source `native`.
#[derive(Default)]
pub struct Tracker {
    entities: Vec<Tracked>,
    polled: Option<Instant>,
    /// Why the native read stopped answering: no more reads after that.
    off: Option<String>,
}

impl Tracker {
    /// Follow entity `id` from now: read it at once, so a later death has a read before it.
    pub fn track(&mut self, game: &mut dyn Game, id: &str) {
        if self.off.is_some() || self.entities.iter().any(|entity| entity.id == id) {
            return;
        }
        self.entities.push(Tracked {
            id: id.to_owned(),
            kind: None,
            health: None,
            seen: false,
            dead: false,
        });
        let last = self.entities.len() - 1;
        self.read(game, last);
    }

    /// Read every live tracked entity, at most once a tick unless `force`.
    pub fn poll(&mut self, game: &mut dyn Game, force: bool) {
        if self.off.is_some() || (!force && self.polled.is_some_and(|at| at.elapsed() < TICK)) {
            return;
        }
        self.polled = Some(Instant::now());
        for index in 0..self.entities.len() {
            if self.off.is_some() {
                return;
            }
            if !self.entities[index].dead {
                self.read(game, index);
            }
        }
    }

    fn read(&mut self, game: &mut dyn Game, index: usize) {
        let entity = &mut self.entities[index];
        let (tick, read) = match game.native_entity(&entity.id) {
            NativeRead::Unavailable(reason) => {
                self.off = Some(reason);
                return;
            }
            NativeRead::Read { tick, entity } => (tick, entity),
        };
        let Some(read) = read else {
            if entity.seen {
                entity.dead = true;
                let body = json!({
                    "entity": entity_json(&entity.id, &entity.kind),
                    "cause": null, "killer": null, "seen": "gone",
                });
                game.push_native("entity_die", tick, body);
            }
            return;
        };
        entity.seen = true;
        entity.kind = read.kind.or(entity.kind.take());
        let Some(now) = read.health else {
            return;
        };
        if let Some([before, _]) = entity.health
            && now[0] < before
        {
            let body = json!({
                "entity": entity_json(&entity.id, &entity.kind),
                "damage": round(before - now[0]),
                "health": health_json(now),
                "cause": null, "attacker": null,
            });
            game.push_native("entity_hurt", tick, body);
        }
        entity.health = Some(now);
        if now[0] <= 0.0 {
            entity.dead = true;
            let body = json!({
                "entity": entity_json(&entity.id, &entity.kind),
                "cause": null, "killer": null, "seen": "health_zero",
            });
            game.push_native("entity_die", tick, body);
        }
    }

    /// Why native reads stopped, for the sequence's notes.
    pub fn stopped(&self) -> Option<&str> {
        self.off.as_deref()
    }

    /// How a tracked entity that did not die ended, as the last read saw it: alive with its
    /// health, or `None` when no read showed it.
    pub fn last(&self, id: &str) -> Option<Value> {
        let entity = self.entities.iter().find(|entity| entity.id == id)?;
        entity.seen.then(|| {
            json!({
                "state": "alive",
                "health": entity.health.map(health_json),
                "source": "native",
            })
        })
    }
}
