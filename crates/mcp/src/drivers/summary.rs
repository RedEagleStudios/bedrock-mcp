//! The summary block of a finished sequence: per target entity, the actions aimed at it, how
//! many hit, which run killed it and how it ended, with why unconfirmed actions failed. Deaths
//! and hurts come from the Bridge pack's events, or without the pack from the `native` events
//! of the sequence's [`Tracker`].

use std::{collections::BTreeMap, time::Duration};

use serde_json::{Map, Value, json};

use super::{Game, native::Tracker};
use crate::{
    bridge::quoted,
    events::{EventCursor, HubEvent, Source},
    world::World,
};

/// The most events the summary reads.
const MAX_EVENTS: usize = 2000;
/// Actions that hit or miss their target.
const STRIKES: [&str; 3] = ["attack", "entity_interact", "item_use"];
/// What the summary counts as a hit, for its readers.
const HITS_MEAN: &str = "An attack or entity_interact hits when the game confirmed it on the \
    target. An item_use (a throw or shot) hits when its projectile hit the target or the \
    target was hurt after the use and before the next use aimed at it; a confirmed throw that \
    hurt nothing is a miss. An action that never went out (out of reach, blocked, no item, on \
    cooldown, its projectile still out) is neither a hit nor a miss; reasons counts it. Actions \
    that went out after the target died count as after_death, not as misses.";
/// Reasons that mean nothing was pressed or thrown.
const HELD_BACK: [&str; 6] = [
    "out_of_reach",
    "blocked",
    "no_item",
    "cooldown",
    "projectile_in_flight",
    "target_dead",
];

/// An entity a spawn step summoned.
pub struct Spawned {
    pub id: String,
    pub label: Option<String>,
    pub step: usize,
    pub kind: Value,
}

/// Where the summary reads how each target ended.
pub enum Ends<'a> {
    /// The Bridge pack's entity read.
    Bridge,
    /// The native tracker's last read of each target.
    Native(&'a Tracker),
}

/// One action of a step: the step itself, or one run of a repeated step.
struct Action<'a> {
    step: u64,
    run: Option<u64>,
    action: &'a str,
    target: Option<&'a str>,
    value: &'a Value,
}

impl Action<'_> {
    fn confirmed(&self) -> bool {
        self.value["outcome"]["confirmed"] == true
    }

    /// The game tick the action's input went out on, or its confirming event's.
    fn tick(&self) -> Option<u64> {
        let outcome = &self.value["outcome"];
        outcome["evidence"]["acted_tick"]
            .as_u64()
            .or(outcome["tick"].as_u64())
    }

    /// Why it was not confirmed; `None` when it was.
    fn reason(&self) -> Option<&str> {
        reason_of(self.value)
    }

    fn place(&self, tick: Option<u64>) -> Value {
        let mut place = json!({ "step": self.step });
        if let Some(run) = self.run {
            place["run"] = run.into();
        }
        if let Some(tick) = tick {
            place["tick"] = tick.into();
        }
        place
    }
}

/// Why an action (a step entry, or one run of a repeated step) was not confirmed: `error`,
/// `interrupted`, its outcome's reason class or `no_event`; `None` when it was confirmed or
/// never acted.
fn reason_of(action: &Value) -> Option<&str> {
    if action.get("error").is_some() {
        return Some("error");
    }
    if action.get("outcome").is_none() {
        return action.get("interrupted").map(|_| "interrupted");
    }
    if action["outcome"]["confirmed"] == true {
        return None;
    }
    Some(
        action["outcome"]["reason"]["class"]
            .as_str()
            .unwrap_or("no_event"),
    )
}

/// What went wrong with an action that acted: its [`reason_of`], else `expect_unmet` when it
/// missed its expect.
fn problem(action: &Value) -> Option<&str> {
    reason_of(action).or_else(|| (action["expect"]["met"] == false).then_some("expect_unmet"))
}

/// Every action the steps ran, in step order.
fn actions(steps: &Value) -> Vec<Action<'_>> {
    let mut found = Vec::new();
    for entry in steps.as_array().into_iter().flatten() {
        let step = entry["index"].as_u64().unwrap_or(0);
        let action = entry["action"].as_str().unwrap_or_default();
        let target = entry["target"].as_str();
        match entry["runs"].as_array() {
            // A run skipped because its target was already dead did not act.
            Some(runs) => found.extend(runs.iter().filter(|run| run.get("skipped").is_none()).map(
                |run| Action {
                    step,
                    run: run["run"].as_u64(),
                    action,
                    target,
                    value: run,
                },
            )),
            None if entry.get("skipped").is_none() => found.push(Action {
                step,
                run: None,
                action,
                target,
                value: entry,
            }),
            None => {}
        }
    }
    found
}

/// The ids an event names as `field.id`.
fn id_of<'a>(event: &'a HubEvent, field: &str) -> Option<&'a str> {
    event.body[field]["id"].as_str()
}

/// The deaths, hits and hurts of `targets` since `since`, from the Bridge pack or the native
/// tracker.
fn events(game: &mut dyn Game, since: &EventCursor, targets: &[&str]) -> Vec<HubEvent> {
    let wanted = |event: &HubEvent| {
        matches!(event.source, Source::Bridge | Source::Native)
            && match event.name.as_str() {
                "entity_die" | "entity_hurt" => {
                    id_of(event, "entity").is_some_and(|id| targets.contains(&id))
                }
                "projectile_hit_entity" => {
                    id_of(event, "target").is_some_and(|id| targets.contains(&id))
                }
                _ => false,
            }
    };
    let mut cursor = since.clone();
    let mut found = Vec::new();
    while found.len() < MAX_EVENTS {
        match game.wait_event(&cursor, Duration::ZERO, &wanted) {
            Ok((Some(event), _)) => {
                cursor = event.cursor.clone();
                found.push(event);
            }
            _ => break,
        }
    }
    found
}

/// How a target ended: dead, alive with its health, or gone from the world.
fn final_state(world: &World, id: &str, died: bool, ends: &Ends) -> Value {
    if died {
        return json!({ "state": "dead" });
    }
    let tracker = match ends {
        Ends::Native(tracker) => tracker,
        Ends::Bridge => {
            let read =
                quoted(id).and_then(|id| world.bridge_command(&format!("re_mcp:entity {id}")));
            return match read {
                Ok(entity) => json!({ "state": "alive", "health": entity["health"] }),
                Err(error) if error.to_string().contains("No loaded entity") => {
                    json!({ "state": "gone" })
                }
                Err(error) => json!({ "state": "unknown", "error": error.to_string() }),
            };
        }
    };
    tracker.last(id).unwrap_or_else(|| {
        json!({
            "state": "unknown",
            "error": "No native read showed this entity, and the Bridge pack is not active.",
        })
    })
}

fn count(counts: &mut Map<String, Value>, class: &str) {
    let entry = counts.entry(class).or_insert(json!(0));
    *entry = (entry.as_u64().unwrap_or(0) + 1).into();
}

/// The summary of `steps`, a finished sequence's step entries; `since` is where its events
/// start.
pub fn summarize(
    game: &mut dyn Game,
    world: &World,
    steps: &Value,
    spawned: &[Spawned],
    since: &EventCursor,
    ends: Ends,
) -> Value {
    let actions = actions(steps);
    let mut ids: Vec<&str> = spawned.iter().map(|spawn| spawn.id.as_str()).collect();
    for action in &actions {
        if let Some(target) = action.target
            && !ids.contains(&target)
        {
            ids.push(target);
        }
    }
    // A target whose every action was skipped still gets its line.
    for entry in steps.as_array().into_iter().flatten() {
        if let Some(target) = entry["target"].as_str()
            && !ids.contains(&target)
        {
            ids.push(target);
        }
    }
    let events = if ids.is_empty() {
        Vec::new()
    } else {
        events(game, since, &ids)
    };
    let mut totals = json!({
        "actions": actions.len(), "confirmed": 0, "unconfirmed": 0,
        "hits": 0, "misses": 0, "kills": 0, "reasons": {},
    });
    let mut reasons = Map::new();
    for action in &actions {
        match action.reason() {
            None => totals["confirmed"] = (totals["confirmed"].as_u64().unwrap_or(0) + 1).into(),
            Some(class) => {
                totals["unconfirmed"] = (totals["unconfirmed"].as_u64().unwrap_or(0) + 1).into();
                count(&mut reasons, class);
            }
        }
    }
    totals["reasons"] = reasons.into();
    let mut targets = BTreeMap::new();
    for id in &ids {
        let aimed: Vec<&Action> = actions
            .iter()
            .filter(|action| action.target == Some(*id))
            .collect();
        let death = events
            .iter()
            .find(|event| event.name == "entity_die" && id_of(event, "entity") == Some(*id));
        let died = death.and_then(|event| event.tick);
        // An action that went out after the target died had nothing to hit: not a miss.
        let (strikes, after_death): (Vec<&&Action>, Vec<&&Action>) = aimed
            .iter()
            .filter(|action| STRIKES.contains(&action.action))
            .filter(|action| {
                action
                    .reason()
                    .is_none_or(|class| !HELD_BACK.contains(&class))
            })
            .partition(|action| {
                died.is_none_or(|died| action.tick().is_none_or(|acted| acted <= died))
            });
        let mut hits = 0;
        let mut reasons = Map::new();
        for (at, action) in strikes.iter().enumerate() {
            let hit = match action.action {
                "item_use" => {
                    // A use hits when its projectile or swing hurts the target before the next
                    // use aimed at it.
                    let from = action.tick();
                    let until = strikes[at + 1..].iter().find_map(|next| next.tick());
                    action.confirmed()
                        && from.is_some_and(|from| {
                            events.iter().any(|event| {
                                let on_target = match event.name.as_str() {
                                    "projectile_hit_entity" => id_of(event, "target"),
                                    "entity_hurt" => id_of(event, "entity"),
                                    _ => None,
                                } == Some(*id);
                                on_target
                                    && event.tick.is_some_and(|tick| {
                                        tick >= from && until.is_none_or(|until| tick < until)
                                    })
                            })
                        })
                }
                _ => action.confirmed(),
            };
            if hit {
                hits += 1;
            }
        }
        for action in &aimed {
            let late = died.is_some_and(|died| action.tick().is_some_and(|acted| acted > died));
            if let Some(class) = action.reason().filter(|_| !late) {
                count(&mut reasons, class);
            }
        }
        let killed_by = death.map(|event| {
            let killer = event.tick.and_then(|tick| {
                strikes
                    .iter()
                    .rev()
                    .find(|action| action.tick().is_some_and(|acted| acted <= tick))
            });
            let mut killed = match killer {
                Some(action) => action.place(event.tick),
                None => json!({ "tick": event.tick }),
            };
            killed["cause"] = event.body["cause"].clone();
            killed["killer"] = event.body["killer"]["name"].clone();
            killed["source"] = event.source.as_str().into();
            killed
        });
        let spawn = spawned.iter().find(|spawn| spawn.id == *id);
        let kind = spawn
            .map(|spawn| spawn.kind.clone())
            .filter(|kind| !kind.is_null())
            .or_else(|| death.map(|event| event.body["entity"]["type"].clone()))
            .unwrap_or(Value::Null);
        let mut steps: Vec<u64> = aimed.iter().map(|action| action.step).collect();
        steps.dedup();
        totals["hits"] = (totals["hits"].as_u64().unwrap_or(0) + hits).into();
        totals["misses"] =
            (totals["misses"].as_u64().unwrap_or(0) + strikes.len() as u64 - hits).into();
        if death.is_some() {
            totals["kills"] = (totals["kills"].as_u64().unwrap_or(0) + 1).into();
        }
        targets.insert(
            (*id).to_owned(),
            json!({
                "label": spawn.and_then(|spawn| spawn.label.clone()),
                "spawned_by": spawn.map(|spawn| spawn.step),
                "type": kind,
                "steps": steps,
                "hits": hits,
                "misses": strikes.len() as u64 - hits,
                "after_death": after_death.len(),
                "killed_by": killed_by,
                "final": final_state(world, id, death.is_some(), &ends),
                "reasons": reasons,
            }),
        );
    }
    let ticks = actions.iter().filter_map(Action::tick);
    json!({
        "targets": targets,
        "totals": totals,
        "first_tick": ticks.clone().min(),
        "last_tick": ticks.max(),
        "hits_mean": HITS_MEAN,
    })
}

/// One line per step entry that did not go as planned: skipped, failed, unconfirmed or missed
/// its expect. A line is `{index, action, target, ok: false}` with `confirmed` (whether the
/// game confirmed the action), `reason` (a reason class, `expect_unmet`, or `skipped` with the
/// skip's text) and `error` for an error; a repeated step's line counts its `runs` and `failed`
/// runs with `reasons {class: count}` instead.
pub fn failed_steps(steps: &Value) -> Vec<Value> {
    steps
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(failed_line)
        .collect()
}

fn failed_line(entry: &Value) -> Option<Value> {
    let mut line = json!({ "index": entry["index"], "action": entry["action"] });
    if let Some(target) = entry.get("target") {
        line["target"] = target.clone();
    }
    line["ok"] = false.into();
    if let Some(skipped) = entry.get("skipped") {
        line["reason"] = entry["reason"]["class"]
            .as_str()
            .unwrap_or("skipped")
            .into();
        line["skipped"] = skipped.clone();
        return Some(line);
    }
    let Some(runs) = entry["runs"].as_array() else {
        line["reason"] = problem(entry)?.into();
        line["confirmed"] = (entry["outcome"]["confirmed"] == true).into();
        if let Some(error) = entry.get("error") {
            line["error"] = error.clone();
        }
        return Some(line);
    };
    // A run skipped because its target was already dead did not act.
    let acted: Vec<&Value> = runs
        .iter()
        .filter(|run| run.get("skipped").is_none())
        .collect();
    let mut reasons = Map::new();
    for run in &acted {
        if let Some(class) = problem(run) {
            count(&mut reasons, class);
            if let (None, Some(error)) = (line.get("error"), run.get("error")) {
                line["error"] = error.clone();
            }
        }
    }
    if reasons.is_empty() {
        return None;
    }
    line["runs"] = acted.len().into();
    line["failed"] = reasons
        .values()
        .filter_map(Value::as_u64)
        .sum::<u64>()
        .into();
    line["reasons"] = reasons.into();
    Some(line)
}

/// The summary without what the default reply leaves out: its targets and totals.
pub fn brief(summary: &Value) -> Value {
    json!({ "targets": summary["targets"], "totals": summary["totals"] })
}
