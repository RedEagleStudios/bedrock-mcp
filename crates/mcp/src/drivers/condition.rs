//! Conditions an action sequence checks while it runs: `stop_when`, and a step's `wait_for`,
//! `if` and `repeat.until`. A check reads the game once and says whether the condition holds,
//! with what showed it.

use std::{collections::HashMap, time::Duration};

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Game, point_of, rounded};
use crate::{
    bridge::Actor,
    error::{Error, Result, bail},
    events::{EventCursor, HubEvent},
    gametest::Vec3,
    world::{World, distance, number},
};

/// A condition, written as an object with one key: `{"event": {...}}`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Check {
    /// An event from the merged stream, named `name` (ignoring case) and containing `contains`
    /// in its body's JSON text.
    Event {
        name: Option<String>,
        contains: Option<String>,
        /// What showed the event once it came; the events after a cursor only grow, so it
        /// keeps holding.
        #[serde(skip)]
        seen: Option<Value>,
    },
    /// An entity of type `matches` within `radius` blocks of `actor`.
    Entity {
        matches: String,
        radius: Option<f64>,
        actor: Option<Actor>,
    },
    /// A field of the Bridge pack's player state compared with a value.
    State {
        actor: Option<Actor>,
        path: String,
        below: Option<f64>,
        above: Option<f64>,
        equals: Option<Value>,
    },
    /// `actor` within `radius` blocks of `position`.
    Near {
        actor: Option<Actor>,
        position: Vec3,
        radius: f64,
    },
    Not(Box<Check>),
    All(Vec<Check>),
    Any(Vec<Check>),
}

const DEFAULT_ENTITY_RADIUS: f64 = 32.0;

impl Check {
    /// Reject conditions that can never be checked.
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Event { .. } => Ok(()),
            Self::Entity {
                matches, radius, ..
            } => {
                if matches.is_empty()
                    || matches.contains(|c: char| c.is_whitespace() || "[],\"=".contains(c))
                {
                    bail!("entity matches must be an entity type id such as minecraft:zombie.");
                }
                if radius.is_some_and(|radius| radius <= 0.0) {
                    bail!("entity radius must be above 0.");
                }
                Ok(())
            }
            Self::State {
                path,
                below,
                above,
                equals,
                ..
            } => {
                if path.split('.').any(str::is_empty) {
                    bail!("state path must name fields joined by dots, e.g. health.current.");
                }
                let comparisons = usize::from(below.is_some())
                    + usize::from(above.is_some())
                    + usize::from(equals.is_some());
                if comparisons != 1 {
                    bail!("state needs exactly one of below, above and equals.");
                }
                Ok(())
            }
            Self::Near { radius, .. } => {
                if *radius <= 0.0 {
                    bail!("near radius must be above 0.");
                }
                Ok(())
            }
            Self::Not(inner) => inner.validate(),
            Self::All(all) | Self::Any(all) => {
                if all.is_empty() {
                    bail!("all and any need at least one condition.");
                }
                all.iter().try_for_each(Self::validate)
            }
        }
    }

    /// Whether any part reads events.
    pub fn reads_events(&self) -> bool {
        self.any_part(&|check| matches!(check, Self::Event { .. }))
    }

    /// Whether any part reads the Bridge pack's player state.
    pub fn reads_state(&self) -> bool {
        self.any_part(&|check| matches!(check, Self::State { .. }))
    }

    fn any_part(&self, test: &dyn Fn(&Self) -> bool) -> bool {
        test(self)
            || match self {
                Self::Not(inner) => inner.any_part(test),
                Self::All(all) | Self::Any(all) => all.iter().any(|check| check.any_part(test)),
                _ => false,
            }
    }

    /// Visit the texts an event condition matches: its name and contains.
    pub fn visit(&self, visit: &mut dyn FnMut(&str)) {
        match self {
            Self::Event { name, contains, .. } => {
                name.iter().chain(contains).for_each(|text| visit(text));
            }
            Self::Not(inner) => inner.visit(visit),
            Self::All(all) | Self::Any(all) => all.iter().for_each(|check| check.visit(visit)),
            _ => {}
        }
    }

    /// [`Check::visit`], changing the texts.
    pub fn visit_mut(&mut self, visit: &mut dyn FnMut(&mut String)) {
        match self {
            Self::Event { name, contains, .. } => {
                name.iter_mut().chain(contains).for_each(&mut *visit);
            }
            Self::Not(inner) => inner.visit_mut(visit),
            Self::All(all) | Self::Any(all) => {
                all.iter_mut().for_each(|check| check.visit_mut(visit));
            }
            _ => {}
        }
    }
}

/// What a check reads the game through.
pub struct Probe<'a> {
    pub world: &'a World,
    /// Whether the Bridge pack answers.
    pub bridge: bool,
    /// The actor for conditions that do not name one.
    pub actor: &'a Actor,
}

/// A condition being watched: events count from `since`.
pub struct Watch {
    check: Check,
    since: EventCursor,
}

impl Watch {
    pub fn new(check: Check, since: EventCursor) -> Self {
        Self { check, since }
    }

    /// What shows the condition holding, or `None` when it does not hold now.
    pub fn holds(&mut self, game: &mut dyn Game, probe: &Probe) -> Result<Option<Value>> {
        let mut states = HashMap::new();
        eval(&mut self.check, game, probe, &self.since, &mut states)
    }

    /// The condition, to fill in references once their entities exist.
    pub fn check_mut(&mut self) -> &mut Check {
        &mut self.check
    }
}

fn eval(
    check: &mut Check,
    game: &mut dyn Game,
    probe: &Probe,
    since: &EventCursor,
    states: &mut HashMap<String, Value>,
) -> Result<Option<Value>> {
    let actor_of = |actor: &Option<Actor>| actor.clone().unwrap_or_else(|| probe.actor.clone());
    Ok(match check {
        Check::Event {
            name,
            contains,
            seen,
        } => {
            if seen.is_none() {
                let wanted = |event: &HubEvent| {
                    name.as_deref()
                        .is_none_or(|name| name.eq_ignore_ascii_case(&event.name))
                        && contains
                            .as_deref()
                            .is_none_or(|text| event.body_contains(text))
                };
                let (event, _) = game.wait_event(since, Duration::ZERO, &wanted)?;
                *seen = event.map(|event| json!({ "event": event.to_json() }));
            }
            seen.clone()
        }
        Check::Entity {
            matches,
            radius,
            actor,
        } => {
            let radius = radius.unwrap_or(DEFAULT_ENTITY_RADIUS);
            let found = match actor_of(actor) {
                // Commands over the WebSocket run at the connected player.
                Actor::Me => probe.world.entity_near(matches, radius)?,
                actor => {
                    let [x, y, z] = position(probe, &actor, states)?;
                    probe.world.test(&format!(
                        "testfor @e[type={matches},x={},y={},z={},r={}]",
                        number(x),
                        number(y),
                        number(z),
                        number(radius)
                    ))?
                }
            };
            found.then(|| json!({ "entity": matches, "radius": radius }))
        }
        Check::State {
            actor,
            path,
            below,
            above,
            equals,
        } => {
            let state = state(probe, &actor_of(actor), states)?;
            let value = field(&state, path)?;
            let numeric = || {
                value
                    .as_f64()
                    .ok_or_else(|| Error::msg(format!("state {path} is {value}, not a number.")))
            };
            let held = match (below, above, equals) {
                (Some(limit), _, _) => numeric()? < *limit,
                (_, Some(limit), _) => numeric()? > *limit,
                (_, _, Some(expected)) => values_equal(value, expected),
                _ => unreachable!("validated: one comparison"),
            };
            held.then(|| json!({ "state": path, "value": value }))
        }
        Check::Near {
            actor,
            position: target,
            radius,
        } => {
            let at = position(probe, &actor_of(actor), states)?;
            let gap = distance(at, [target.x, target.y, target.z]);
            (gap <= *radius).then(|| json!({ "near": rounded(gap) }))
        }
        Check::Not(inner) => match eval(inner, game, probe, since, states)? {
            Some(_) => None,
            None => Some(json!({ "not": true })),
        },
        Check::All(all) => {
            let mut details = Vec::with_capacity(all.len());
            for check in all {
                match eval(check, game, probe, since, states)? {
                    Some(detail) => details.push(detail),
                    None => return Ok(None),
                }
            }
            Some(json!({ "all": details }))
        }
        Check::Any(any) => {
            for check in any {
                if let Some(detail) = eval(check, game, probe, since, states)? {
                    return Ok(Some(json!({ "any": detail })));
                }
            }
            None
        }
    })
}

/// Numbers compare by value, so `equals: 20` matches a state of `20.0`.
fn values_equal(value: &Value, expected: &Value) -> bool {
    match (value.as_f64(), expected.as_f64()) {
        (Some(a), Some(b)) => a == b,
        _ => value == expected,
    }
}

/// The Bridge pack's `re_mcp:state` for `actor`, read once per check.
fn state(probe: &Probe, actor: &Actor, states: &mut HashMap<String, Value>) -> Result<Value> {
    let command = format!("re_mcp:state{}", actor.argument());
    if let Some(state) = states.get(&command) {
        return Ok(state.clone());
    }
    let state = probe.world.bridge_command(&command)?;
    states.insert(command, state.clone());
    Ok(state)
}

/// Where `actor` stands: the Bridge pack's state, or `querytarget` without the pack.
fn position(probe: &Probe, actor: &Actor, states: &mut HashMap<String, Value>) -> Result<[f64; 3]> {
    if !probe.bridge {
        return Ok(probe.world.actor_target(actor)?.position);
    }
    let state = state(probe, actor, states)?;
    point_of(&state["location"])
        .ok_or_else(|| Error::msg(format!("re_mcp:state{} has no location.", actor.argument())))
}

/// The value at a dotted path such as `health.current` or `effects.0.id`.
fn field<'a>(state: &'a Value, path: &str) -> Result<&'a Value> {
    let mut value = state;
    for key in path.split('.') {
        let next = match value {
            Value::Array(items) => key.parse::<usize>().ok().and_then(|index| items.get(index)),
            Value::Object(fields) => fields.get(key),
            _ => None,
        };
        value = next.ok_or_else(|| {
            let known = state
                .as_object()
                .map(|fields| fields.keys().cloned().collect::<Vec<_>>().join(", "))
                .unwrap_or_default();
            Error::msg(format!(
                "The player state has no {path}; its fields are {known}."
            ))
        })?;
    }
    Ok(value)
}
