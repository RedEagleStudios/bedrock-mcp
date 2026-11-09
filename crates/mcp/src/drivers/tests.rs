//! The drivers against a fake game on a real WebSocket: it answers commands, plays the Bridge
//! pack's event log and the GameTest pack's tickets, and records the input it was sent.

use std::sync::{Arc, Mutex};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};

use super::*;
use crate::{
    bridge::fake::{Seen, failure, game, json_reply, unknown},
    events::{EventHub, Filter},
    world::facing,
};

const SHORT: Duration = Duration::from_millis(400);

/// The game's side: the bridge event log, the tick, the GameTest actions sent, and the
/// connected player's distance to pig -5.
#[derive(Default)]
struct Script {
    events: Vec<Value>,
    seq: u64,
    tick: u64,
    /// Every GameTest action sent, by ticket (from 1), with the tick it runs at.
    tickets: Vec<(Value, u64)>,
    /// With a clock the tick is real time (50 ms each) since it started, instead of one
    /// tick per re_mcp:ping.
    clock: Option<Instant>,
    /// How long the game takes to answer each command.
    latency: Duration,
    /// What `re_mcp:result` reports for the last queued write.
    written: Value,
    /// How far pig -5 is along Steve's sight; each aim while key.forward is held brings it
    /// a block closer.
    gap: f64,
    walking: bool,
    /// The game tick pig -5 dies on, seen on the first re_mcp:ping from then on.
    dies_at: Option<u64>,
    /// Pig -5 is gone: aiming at it fails.
    gone: bool,
    /// Health by entity id as the fake unstable layer reads it; `None`: no unstable layer.
    /// Each native read is one server tick (`tick`).
    native: Option<HashMap<String, [f64; 2]>>,
    /// The connected player as the fake unstable layer reads it; `None`: no unstable layer.
    player: Option<protocol::unstable::NativePlayer>,
}

impl Script {
    fn push(&mut self, kind: &str, details: Value) {
        self.seq += 1;
        let tick = match self.clock {
            Some(_) => self.now(),
            None => 100 + self.seq,
        };
        let mut event = json!({"seq": self.seq, "tick": tick, "type": kind});
        event
            .as_object_mut()
            .unwrap()
            .extend(details.as_object().unwrap().clone());
        self.events.push(event);
    }

    fn now(&self) -> u64 {
        match self.clock {
            Some(start) => (start.elapsed().as_millis() / TICK.as_millis()) as u64,
            None => self.tick,
        }
    }
}

type Answer = dyn Fn(&str, &mut Script) -> Option<Value> + Send;
type React = dyn FnMut(&InputRequest, &mut Script);

struct Fake {
    world: World,
    hub: EventHub,
    script: Arc<Mutex<Script>>,
    inputs: Vec<InputRequest>,
    react: Box<React>,
    /// "start" and "stop" of recordings, each with how many inputs went out before it.
    recordings: Vec<(&'static str, usize)>,
    /// The loaded world's key, and the one the player last warmed up in.
    world_key: Option<String>,
    warm_key: Option<String>,
}

/// A game with or without the Bridge pack; `answer` handles the other commands.
fn fake(bridge: bool, answer: Box<Answer>) -> (Fake, Seen) {
    let script = Arc::new(Mutex::new(Script::default()));
    let shared = Arc::clone(&script);
    let (world, seen) = game(move |line| {
        let mut script = shared.lock().unwrap();
        thread::sleep(script.latency);
        if !bridge && line.starts_with("re_mcp:") && !line.starts_with("re_mcp:gametest") {
            return unknown(line);
        }
        if line == "re_mcp:ping" {
            script.tick += 1;
            let now = script.now();
            if script.dies_at.is_some_and(|at| now >= at) {
                script.dies_at = None;
                script.push(
                    "entity_die",
                    json!({"entity": {"id": "-5", "type": "minecraft:pig"}, "cause": "fire"}),
                );
            }
            return json_reply(json!({"version": 2, "tick": now}));
        }
        if script.gone && line.starts_with("re_mcp:aim ") {
            return failure("re_mcp:aim failed: No loaded entity with id -5");
        }
        if let Some(since) = line.strip_prefix("re_mcp:events ") {
            let since: u64 = since.parse().unwrap();
            let newer: Vec<Value> = script
                .events
                .iter()
                .filter(|event| event["seq"].as_u64().unwrap() > since)
                .cloned()
                .collect();
            let next = newer.last().map_or(since.max(script.seq), |event| {
                event["seq"].as_u64().unwrap()
            });
            return json_reply(
                json!({"epoch": "e", "latest": script.seq, "events": newer, "next": next, "dropped": 0}),
            );
        }
        if line.starts_with("re_mcp:result ") {
            let mut reply = json!({"pending": false});
            if let Some(written) = script.written.as_object() {
                reply.as_object_mut().unwrap().extend(written.clone());
            }
            return json_reply(reply);
        }
        answer(line, &mut script).unwrap_or_else(|| failure(&format!("unexpected {line}")))
    });
    let fake = Fake {
        world,
        hub: EventHub::default(),
        script,
        inputs: Vec::new(),
        react: Box::new(|_, _| {}),
        recordings: Vec::new(),
        world_key: None,
        warm_key: None,
    };
    (fake, seen)
}

impl Game for Fake {
    fn world(&mut self) -> Result<World> {
        Ok(self.world.clone())
    }

    fn cursor(&mut self) -> Result<EventCursor> {
        self.hub.now(&self.world)
    }

    fn wait_event(
        &mut self,
        after: &EventCursor,
        timeout: Duration,
        matches: &dyn Fn(&HubEvent) -> bool,
    ) -> Result<(Option<HubEvent>, Vec<String>)> {
        let batch = self
            .hub
            .read(&self.world, after, &Filter::default(), 1, timeout, matches)?;
        Ok((batch.events.into_iter().next(), batch.notes))
    }

    fn input(&mut self, request: InputRequest) -> Result<()> {
        (self.react)(&request, &mut self.script.lock().unwrap());
        self.inputs.push(request);
        Ok(())
    }

    fn record_start(&mut self, args: Value) -> Result<Value> {
        self.recordings.push(("start", self.inputs.len()));
        Ok(json!({ "recording": true, "fps": args["fps"], "notes": [] }))
    }

    fn record_stop(&mut self) -> Result<Value> {
        self.recordings.push(("stop", self.inputs.len()));
        Ok(json!({ "path": "/tmp/fight.mp4", "duration_seconds": 1.5 }))
    }

    fn world_key(&mut self) -> Option<String> {
        self.world_key.clone()
    }

    fn warm_key(&mut self) -> Option<String> {
        self.warm_key.clone()
    }

    fn set_warm_key(&mut self, key: Option<String>) {
        self.warm_key = key;
    }

    fn native_entity(&mut self, id: &str) -> NativeRead {
        let mut script = self.script.lock().unwrap();
        script.tick += 1;
        let tick = script.tick;
        match &script.native {
            None => NativeRead::Unavailable("no unstable layer in this game".into()),
            Some(health) => NativeRead::Read {
                tick,
                entity: health.get(id).map(|health| Vitals {
                    kind: Some("minecraft:zombie".into()),
                    health: Some(*health),
                }),
            },
        }
    }

    fn push_native(&mut self, name: &str, tick: u64, body: Value) {
        self.hub.push_native(name, tick, body);
    }

    fn attachable(&mut self) -> Result<Option<Value>> {
        Ok(Some(
            json!({ "animating": true, "players": ["controller.sword"], "attachables": 1 }),
        ))
    }

    fn native_player(&mut self) -> Result<protocol::unstable::NativePlayer> {
        let mut script = self.script.lock().unwrap();
        script.tick += 1;
        script
            .player
            .clone()
            .ok_or_else(|| Error::msg("no unstable layer in this game"))
    }

    /// Every entity with native health, as a zombie two blocks south of the origin.
    fn native_list(&mut self, _radius: f64, _entity_type: Option<&str>) -> Result<Vec<Value>> {
        let mut script = self.script.lock().unwrap();
        script.tick += 1;
        let Some(health) = &script.native else {
            bail!("no unstable layer in this game");
        };
        Ok(health
            .iter()
            .map(|(id, [current, max])| {
                json!({
                    "id": id, "type": "minecraft:zombie",
                    "location": {"x": 0.0, "y": 0.0, "z": 2.0},
                    "health": {"current": current, "max": max},
                })
            })
            .collect())
    }
}

/// Steve, the connected player, at the origin; pig -5 three blocks south, `script.gap`
/// blocks along Steve's sight once he aims at it, with `target` on his crosshair.
fn steve(target: &'static str) -> Box<Answer> {
    Box::new(move |line, script| {
        Some(json_reply(match line {
            "re_mcp:state" => json!({
                "name": "Steve", "location": {"x": 0.0, "y": 0.0, "z": 0.0},
                "eye": {"x": 0.0, "y": 1.62, "z": 0.0}, "yaw": 0.0, "pitch": 0.0, "selectedSlot": 0
            }),
            "re_mcp:entity \"-5\"" => json!({
                "id": "-5", "location": {"x": 0.0, "y": 0.0, "z": 3.0}, "eye": {"x": 0.0, "y": 1.62, "z": 3.0}
            }),
            "re_mcp:target" => {
                json!({"entity": {"id": target, "type": "minecraft:pig"}, "block": null})
            }
            _ if line.starts_with("re_mcp:aim ") => {
                if script.walking {
                    script.gap -= 1.0;
                }
                let (yaw, pitch) = facing([0.0, 1.62, 0.0], [0.0, 0.81, 3.0]);
                script.written = json!({
                    "tick": script.now(), "name": "Steve", "gameMode": "Survival",
                    "requested": {"yaw": yaw, "pitch": pitch}, "yaw": yaw, "pitch": pitch,
                    "entity": {"id": target, "type": "minecraft:pig", "distance": script.gap},
                    "block": null,
                });
                json!({"write": 1})
            }
            _ => return None,
        }))
    })
}

fn hit(attacker: &str) -> Value {
    json!({"attacker": {"name": attacker}, "target": {"id": "-5"}})
}

fn attack_pig() -> Request {
    Request::parse("attack", json!({"entity_id": "-5"})).unwrap()
}

#[test]
fn input_attack_aims_from_the_eye_and_confirms_with_the_event_after_the_cursor() {
    let (mut game, seen) = fake(true, steve("-5"));
    // A hit from before the action must not count.
    game.script
        .lock()
        .unwrap()
        .push("entity_hit_entity", hit("Steve"));
    game.react = Box::new(|request, script| {
        if request.press == ["key.attack"] {
            script.push("entity_hit_entity", hit("Steve"));
        }
    });
    let outcome = perform(&mut game, &attack_pig(), SHORT).unwrap();
    assert!(outcome.confirmed, "{:?}", outcome.notes);
    assert_eq!(outcome.driver, "input");
    assert_eq!(outcome.actor, "self");
    assert_eq!(outcome.source, "event");
    assert_eq!(outcome.tick, Some(102));
    assert_eq!(outcome.evidence["event"]["name"], "entity_hit_entity");
    assert_eq!(game.inputs.len(), 1);

    // One re_mcp:aim turns toward the pig's middle and reads the crosshair; nothing else is
    // read before the click.
    let seen = seen.lock().unwrap();
    assert!(
        seen.contains(&"re_mcp:aim \"entity=-5\"".to_owned()),
        "{seen:?}"
    );
    assert!(
        !seen
            .iter()
            .any(|line| line == "re_mcp:target" || line == "re_mcp:state")
    );
    assert_eq!(outcome.evidence["crosshair"]["entity"]["id"], "-5");
    let (yaw, pitch) = facing([0.0, 1.62, 0.0], [0.0, 0.81, 3.0]);
    assert_eq!(outcome.evidence["aim"]["pitch"], rounded(pitch));
    assert_eq!(outcome.evidence["aim"]["yaw"], rounded(yaw));
}

#[test]
fn input_attack_is_unconfirmed_when_someone_else_hit_or_nothing_came() {
    let (mut game, _) = fake(true, steve("-5"));
    game.react = Box::new(|_, script| script.push("entity_hit_entity", hit("Alex")));
    let outcome = perform(&mut game, &attack_pig(), SHORT).unwrap();
    assert!(!outcome.confirmed);
    let notes = outcome.notes.join(" ");
    assert!(
        notes.contains("No entity_hit_entity event from Steve"),
        "{notes}"
    );
    assert!(notes.contains("entity_hit_entity by Alex"), "{notes}");

    let (mut game, _) = fake(true, steve("-5"));
    let outcome = perform(&mut game, &attack_pig(), SHORT).unwrap();
    assert!(!outcome.confirmed);
    assert!(
        outcome.notes[0].contains("within 400 ms"),
        "{:?}",
        outcome.notes
    );
    assert_eq!(outcome.to_json()["confirmed"], false);
}

#[test]
fn a_timed_hold_releases_and_waits_for_the_use_to_end() {
    let (mut game, _) = fake(true, steve("-5"));
    game.react = Box::new(|request, script| {
        if request.hold == ["key.use"] {
            let by = json!({"player": {"name": "Steve"}, "item": {"id": "minecraft:bread"}});
            script.push("item_start_use", by.clone());
            script.push("item_complete_use", by);
        }
    });
    let request = Request::parse("item_use", json!({"mode": "hold", "ticks": 4})).unwrap();
    let outcome = perform(&mut game, &request, SHORT).unwrap();
    assert_eq!(game.inputs[0].hold, ["key.use"]);
    assert_eq!(game.inputs[0].hold_ms, None);
    assert_eq!(game.inputs[1].release, ["key.use"]);
    assert!(outcome.confirmed);
    assert_eq!(outcome.evidence["event"]["name"], "item_start_use");
    assert_eq!(outcome.evidence["events"][1]["name"], "item_complete_use");
}

#[test]
fn breaking_holds_attack_until_the_timeout_then_lets_go_and_reads_the_block() {
    let answer: Box<Answer> = Box::new(|line, script| {
        let obsidian = json!({
            "id": "minecraft:obsidian", "location": {"x": 0, "y": 64, "z": 2}, "face": "North"
        });
        Some(json_reply(match line {
            "re_mcp:block 0 64 2" => json!({"id": "minecraft:obsidian"}),
            "re_mcp:aim \"x=0.5;y=64.5;z=2.5\"" => {
                script.written = json!({
                    "tick": 1, "name": "Steve", "requested": {"yaw": 0, "pitch": 0},
                    "entity": null, "block": obsidian,
                });
                json!({"write": 1})
            }
            _ => return None,
        }))
    });
    let (mut game, _) = fake(true, answer);
    let request = Request::parse(
        "block_break",
        json!({"location": {"x": 0, "y": 64, "z": 2}, "timeout_ms": 200}),
    )
    .unwrap();
    let outcome = perform(&mut game, &request, SHORT).unwrap();
    assert!(!outcome.confirmed);
    assert_eq!(game.inputs.len(), 2);
    assert_eq!(game.inputs[0].hold, ["key.attack"]);
    assert_eq!(game.inputs[1].release, ["key.attack"]);
    assert_eq!(outcome.evidence["block"]["id"], "minecraft:obsidian");
    assert!(
        outcome.notes.iter().any(|note| note.contains("still")),
        "{:?}",
        outcome.notes
    );
}

#[test]
fn walking_stops_before_water_ahead_unless_told_to_walk_in() {
    let shore: Box<Answer> = Box::new(|line, _| {
        Some(json_reply(match line {
            "re_mcp:state" => json!({
                "name": "Steve", "location": {"x": 0.5, "y": 64.0, "z": 0.5},
                "eye": {"x": 0.5, "y": 65.62, "z": 0.5}, "flags": {"inWater": false}
            }),
            "re_mcp:block 0 64 1" => json!({"id": "minecraft:air", "air": true}),
            "re_mcp:block 0 63 1" => json!({"id": "minecraft:water", "air": false}),
            _ if line.starts_with("re_mcp:look ") => json!({"write": 1}),
            _ => return None,
        }))
    });
    let (mut game, _) = fake(true, shore);
    let south = json!({"to": {"x": 0.5, "y": 64, "z": 10.5}, "timeout_ms": 300});
    let request = Request::parse("movement_set", south.clone()).unwrap();
    let outcome = perform(&mut game, &request, SHORT).unwrap();
    assert!(!outcome.confirmed);
    assert!(game.inputs.is_empty(), "{:?}", game.inputs);
    assert_eq!(outcome.evidence["liquid"]["id"], "minecraft:water");
    assert_eq!(outcome.evidence["liquid"]["block"], json!([0, 63, 1]));

    let mut wade = south;
    wade["avoid_water"] = json!(false);
    let request = Request::parse("movement_set", wade).unwrap();
    let outcome = perform(&mut game, &request, SHORT).unwrap();
    assert!(outcome.evidence.get("liquid").is_none());
    assert_eq!(game.inputs[0].r#move.as_ref().unwrap().forward, Some(1.0));
}

#[test]
fn input_attack_does_not_click_when_the_crosshair_shows_something_else() {
    let (mut game, _) = fake(true, steve("-9"));
    let outcome = perform(&mut game, &attack_pig(), SHORT).unwrap();
    assert!(!outcome.confirmed);
    assert!(game.inputs.is_empty());
    assert!(
        outcome.notes[0].contains("not on entity -5") && outcome.notes[0].contains("-9"),
        "{:?}",
        outcome.notes
    );
}

#[test]
fn without_a_way_to_aim_only_crosshair_actions_are_pressed() {
    let no_cheats: Box<Answer> = Box::new(|line, _| {
        line.starts_with("querytarget")
            .then(|| failure("You do not have permission to use this command"))
    });
    let (mut game, _) = fake(false, no_cheats);
    let outcome = perform(&mut game, &attack_pig(), SHORT).unwrap();
    assert!(!outcome.confirmed);
    assert!(game.inputs.is_empty());
    assert!(
        outcome.notes[0].contains("cannot be aimed"),
        "{:?}",
        outcome.notes
    );

    let crosshair = Request::parse("attack", json!({})).unwrap();
    let outcome = perform(&mut game, &crosshair, SHORT).unwrap();
    assert_eq!(game.inputs.len(), 1);
    assert_eq!(game.inputs[0].press, ["key.attack"]);
    assert!(!outcome.confirmed);
    assert!(
        outcome.notes[0].contains("No WebSocket event"),
        "{:?}",
        outcome.notes
    );
}

/// The action a `re_mcp:gametest_act` line carries.
fn payload(line: &str) -> Value {
    let encoded = line.rsplit('"').nth(1).unwrap();
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded).unwrap()).unwrap()
}

/// The GameTest pack with simulated player Bot at the origin, holding 14 cobblestone and
/// looking 45 degrees down to the south. Pigs -5 and -6 stand two blocks south, pig -9 twelve.
/// Bot's actions on -5 cause the matching bridge events; attacking -6 fails while it is still
/// hurt; navigation to x 99 finds no path.
fn bot() -> Box<Answer> {
    bot_with(true)
}

fn bot_with(alive: bool) -> Box<Answer> {
    Box::new(move |line, script| {
        let mut player = json!({
            "name": "Bot", "selectedSlot": 2, "location": {"x": 0.0, "y": 0.0, "z": 0.0},
            "eye": {"x": 0.0, "y": 1.62, "z": 0.0}, "yaw": 0.0, "pitch": 45.0, "alive": alive,
            "gameMode": "Survival", "selectedItem": {"id": "minecraft:cobblestone", "amount": 14}
        });
        let pig = |z: f64| {
            json_reply(json!({
                "location": {"x": 0.0, "y": 0.0, "z": z}, "eye": {"x": 0.0, "y": 0.9, "z": z}
            }))
        };
        Some(match line {
            "re_mcp:entity \"-5\"" | "re_mcp:entity \"-6\"" => pig(2.0),
            "re_mcp:entity \"-9\"" => pig(12.0),
            _ if line.starts_with("re_mcp:entity ") => failure(&format!(
                "re_mcp:entity failed: Error: No loaded entity with id {}",
                line.rsplit(' ').next().unwrap().trim_matches('"')
            )),
            "re_mcp:gametest_list" => json_reply(json!({"players": [player]})),
            _ if line.starts_with("re_mcp:gametest_act ") => {
                // The action runs on the next tick, or at the tick the optional last argument
                // names.
                let at = line.rsplit(' ').next().unwrap().parse::<u64>().ok();
                let next = script.now() + 1;
                let action = payload(line);
                script
                    .tickets
                    .push((action, at.map_or(next, |at| at.max(next))));
                json_reply(json!({"ticket": script.tickets.len()}))
            }
            _ if line.starts_with("re_mcp:gametest_result ") => {
                let ticket: usize = line.rsplit(' ').next().unwrap().parse().unwrap();
                let (acted, acted_at) = script.tickets[ticket - 1].clone();
                if script.now() < acted_at {
                    if script.clock.is_none() {
                        script.tick += 1;
                    }
                    return Some(json_reply(json!({"done": false})));
                }
                if !alive {
                    return Some(json_reply(json!({
                        "done": true, "ok": false, "error": "Error: Simulated player \"Bot\" is dead"
                    })));
                }
                if acted["entity"]
                    .as_str()
                    .is_some_and(|id| !["-5", "-6", "-9"].contains(&id))
                {
                    return Some(json_reply(json!({
                        "done": true, "ok": false,
                        "error": format!("Error: No entity with id {}", acted["entity"])
                    })));
                }
                let by = json!({"name": "Bot"});
                let far = acted["entity"] == "-9" && !acted["reach"].is_null();
                match acted["action"].as_str().unwrap() {
                    "attack_entity" if acted["entity"] == "-5" => script.push(
                        "entity_hit_entity",
                        json!({"attacker": by, "target": {"id": acted["entity"]}}),
                    ),
                    "interact_with_entity" if acted["entity"] == "-5" => script.push(
                        "player_interact_entity",
                        json!({"player": by, "target": {"id": acted["entity"]}}),
                    ),
                    "use_item_in_slot_on_block" => script.push(
                        "block_place",
                        json!({"player": by, "block": {"location": {"x": 1, "y": 65, "z": 2}}}),
                    ),
                    _ => {}
                }
                let result = match acted["action"].as_str().unwrap() {
                    "navigate_to_location" if acted["location"]["x"] == 99.0 => {
                        json!({"isFullPath": false, "path": []})
                    }
                    "navigate_to_location" => {
                        json!({"isFullPath": true, "path": [acted["location"]]})
                    }
                    "attack_entity" => json!(acted["entity"] == "-5"),
                    _ => json!(true),
                };
                if let Some(amount) = acted["amount"].as_u64() {
                    player["selectedItem"]["amount"] = (14 - amount).into();
                }
                let target = (acted["entity"] == "-6")
                    .then(|| json!({"id": "-6", "valid": true, "health": 10, "hurtTicksAgo": 3}));
                let mut value = json!({
                    "action": acted["action"], "result": result, "player": player,
                    "target": target, "tick": acted_at
                });
                if far {
                    value["refused"] = "reach".into();
                    value["distance"] = 11.68.into();
                    value["reach"] = 3.0.into();
                } else if !acted["reach"].is_null() {
                    value["distance"] = 1.83.into();
                }
                json_reply(json!({"done": true, "ok": true, "value": value}))
            }
            _ => return None,
        })
    })
}

fn acts(seen: &Seen) -> Vec<Value> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|line| line.starts_with("re_mcp:gametest_act \"Bot\" "))
        .map(|line| payload(line))
        .collect()
}

#[test]
fn a_named_actor_routes_to_gametest_act_with_the_action_payload() {
    let (mut game, seen) = fake(true, bot());
    let request = Request::parse("attack", json!({"actor": "Bot", "entity_id": "-5"})).unwrap();
    let outcome = perform(&mut game, &request, SHORT).unwrap();
    assert_eq!(outcome.driver, "gametest");
    assert_eq!(outcome.actor, "Bot");
    assert!(outcome.confirmed, "{:?}", outcome.notes);
    assert_eq!(outcome.evidence["method"], "attack_entity");
    assert_eq!(outcome.evidence["event"]["name"], "entity_hit_entity");
    assert!(game.inputs.is_empty());

    let request = Request::parse(
        "block_place",
        json!({"actor": "Bot", "location": {"x": 1, "y": 64, "z": 2}, "face": "up"}),
    )
    .unwrap();
    let outcome = perform(&mut game, &request, SHORT).unwrap();
    assert!(outcome.confirmed, "{:?}", outcome.notes);
    assert_eq!(outcome.evidence["placed_at"], json!([1, 65, 2]));
    assert_eq!(
        acts(&seen),
        [
            json!({"action": "attack_entity", "entity": "-5", "reach": {"survival": 3.0, "creative": 5.0}}),
            json!({"action": "use_item_in_slot_on_block", "slot": 2, "block": {"x": 1.0, "y": 64.0, "z": 2.0}, "face": "up", "face_location": null}),
        ]
    );
}

#[test]
fn unsupported_combinations_fail_clearly() {
    let (mut game, seen) = fake(true, bot());
    let offhand = Request::parse(
        "item_use",
        json!({"actor": "Bot", "mode": "instant", "hand": "offhand"}),
    )
    .unwrap();
    let error = perform(&mut game, &offhand, SHORT).unwrap_err().to_string();
    assert!(error.contains("offhand"), "{error}");
    let sneak = Request::parse(
        "movement_set",
        json!({"actor": "Bot", "forward": 1, "sneak": true}),
    )
    .unwrap();
    let error = perform(&mut game, &sneak, SHORT).unwrap_err().to_string();
    assert!(error.contains("cannot sneak"), "{error}");
    assert!(acts(&seen).is_empty());

    let timed_instant = Request::parse("item_use", json!({"mode": "instant", "ms": 100})).unwrap();
    let error = perform(&mut game, &timed_instant, SHORT)
        .unwrap_err()
        .to_string();
    assert!(error.contains("only apply to mode hold"), "{error}");
    let error = Request::parse("item_drop", json!({"quantity": "half"}))
        .unwrap_err()
        .to_string();
    assert!(error.contains("item_drop"), "{error}");
    let both = Request::parse(
        "movement_set",
        json!({"to": {"x": 1, "y": 2, "z": 3}, "forward": 1}),
    )
    .unwrap();
    let error = perform(&mut game, &both, SHORT).unwrap_err().to_string();
    assert!(error.contains("not both"), "{error}");
}

#[test]
fn a_sequence_runs_steps_in_tick_order_and_reports_each_outcome() {
    let (mut game, seen) = fake(true, bot());
    let args: SequenceArgs = serde_json::from_value(json!({
        "actor": "Bot",
        "steps": [
            {"action": "attack", "args": {"entity_id": "-5"}, "at_tick": 3},
            {"action": "entity_interact", "args": {"entity_id": "-5"}, "at_tick": 1},
            {"action": "item_drop", "args": {"actor": "Ghost"}, "at_tick": 3}
        ]
    }))
    .unwrap();
    let result = sequence_with(&mut game, args, SHORT).unwrap();
    assert_eq!(result["clock"], "tick");
    let steps = result["steps"].as_array().unwrap();
    let order: Vec<_> = steps.iter().map(|step| step["index"].clone()).collect();
    assert_eq!(order, [json!(1), json!(0), json!(2)]);
    assert_eq!(steps[0]["action"], "entity_interact");
    assert!(steps[0]["started_tick"].as_u64().unwrap() >= 1);
    assert!(steps[1]["started_tick"].as_u64().unwrap() >= 3);
    assert_eq!(steps[0]["outcome"]["confirmed"], true);
    assert_eq!(
        steps[0]["outcome"]["evidence"]["event"]["name"],
        "player_interact_entity"
    );
    assert_eq!(steps[1]["outcome"]["confirmed"], true);
    // A step's own actor wins; Ghost is not in the world, so its step reports an error.
    assert!(
        steps[2]["error"].as_str().unwrap().contains("Ghost"),
        "{}",
        steps[2]
    );
    assert_eq!(result["confirmed"], 2);
    let methods: Vec<_> = acts(&seen)
        .iter()
        .map(|action| action["action"].clone())
        .collect();
    assert_eq!(
        methods,
        [json!("interact_with_entity"), json!("attack_entity")]
    );
}

#[test]
fn a_sequence_checks_every_step_before_acting() {
    let (mut game, seen) = fake(true, bot());
    let mixed: SequenceArgs = serde_json::from_value(json!({
        "steps": [
            {"action": "attack", "at_tick": 1},
            {"action": "attack", "after_ms": 5}
        ]
    }))
    .unwrap();
    let error = sequence_with(&mut game, mixed, SHORT)
        .unwrap_err()
        .to_string();
    assert!(error.contains("not both"), "{error}");
    let unknown_action: SequenceArgs = serde_json::from_value(json!({
        "steps": [{"action": "attack", "args": {"actor": "Bot"}}, {"action": "dance"}]
    }))
    .unwrap();
    let error = sequence_with(&mut game, unknown_action, SHORT)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("Step 1") && error.contains("item_use"),
        "{error}"
    );
    assert!(seen.lock().unwrap().is_empty());
}

#[test]
fn simulated_attacks_respect_reach_and_say_why_the_game_refused() {
    let (mut game, seen) = fake(true, bot());
    let attack = |id: &str| Request::parse("attack", json!({"actor": "Bot", "entity_id": id}));
    let outcome = perform(&mut game, &attack("-9").unwrap(), SHORT).unwrap();
    assert!(!outcome.confirmed);
    assert_eq!(outcome.evidence["reach"], 3.0);
    assert_eq!(outcome.evidence["distance"], 11.68);
    assert!(
        outcome.notes[0].contains("out of reach"),
        "{:?}",
        outcome.notes
    );
    // The pack measures the reach when the attack runs, so nothing is read before it.
    assert_eq!(
        acts(&seen)[0]["reach"],
        json!({"survival": 3.0, "creative": 5.0})
    );
    assert!(
        !seen
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.starts_with("re_mcp:entity") || line == "re_mcp:gametest_list")
    );

    let outcome = perform(&mut game, &attack("-6").unwrap(), SHORT).unwrap();
    assert!(!outcome.confirmed);
    assert!(
        outcome
            .notes
            .iter()
            .any(|note| note.contains("hurt 3 ticks")),
        "{:?}",
        outcome.notes
    );

    let outcome = perform(&mut game, &attack("-5").unwrap(), SHORT).unwrap();
    assert!(outcome.confirmed, "{:?}", outcome.notes);
    assert_eq!(outcome.evidence["distance"], 1.83);
}

#[test]
fn simulated_player_failures_read_as_next_steps() {
    let (mut game, _) = fake(true, bot());
    let gone = Request::parse("attack", json!({"actor": "Bot", "entity_id": "-7"})).unwrap();
    let error = perform(&mut game, &gone, SHORT).unwrap_err().to_string();
    assert!(
        error.starts_with("No entity with id \"-7\": it died"),
        "{error}"
    );

    let (mut game, seen) = fake(true, bot_with(false));
    let walk = Request::parse("movement_set", json!({"actor": "Bot", "forward": 1})).unwrap();
    let error = perform(&mut game, &walk, SHORT).unwrap_err().to_string();
    assert!(
        error.contains("is dead") && error.contains("gametest_remove"),
        "{error}"
    );
    // The error names no internal command.
    let stop = Request::parse("stop", json!({"actor": "Bot"})).unwrap();
    let error = perform(&mut game, &stop, SHORT).unwrap_err().to_string();
    assert!(
        error.starts_with("Simulated player \"Bot\" is dead (health 0)")
            && !error.contains("re_mcp:"),
        "{error}"
    );
    assert_eq!(acts(&seen).len(), 2);
}

#[test]
fn simulated_travel_reports_the_height_gap_and_gives_up_without_a_path_or_progress() {
    let (mut game, _) = fake(true, bot());
    let travel = |to: Value| Request::parse("movement_set", json!({"actor": "Bot", "to": to}));
    let started = Instant::now();
    let outcome = perform(
        &mut game,
        &travel(json!({"x": 99, "y": 0, "z": 0})).unwrap(),
        SHORT,
    )
    .unwrap();
    assert!(!outcome.confirmed);
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(outcome.notes[1].contains("no path"), "{:?}", outcome.notes);

    // Straight up: horizontally there, but 26 blocks short, and the player never moves.
    let outcome = perform(
        &mut game,
        &travel(json!({"x": 0, "y": 26, "z": 0})).unwrap(),
        SHORT,
    )
    .unwrap();
    assert!(!outcome.confirmed);
    assert_eq!(outcome.evidence["distance"], 0.0);
    assert_eq!(outcome.evidence["vertical_gap"], 26.0);
    assert!(
        outcome.notes[0].contains("not moved for 2 s")
            && outcome.notes[0].contains("26 blocks below"),
        "{:?}",
        outcome.notes
    );
}

#[test]
fn a_simulated_look_keeps_looking_and_confirms_the_pitch_it_reads_back() {
    let (mut game, seen) = fake(true, bot());
    let look =
        |position: Value| Request::parse("look", json!({"actor": "Bot", "position": position}));
    // Bot reads yaw 0, pitch 45: 5 blocks south and 5 down from its eyes.
    let outcome = perform(
        &mut game,
        &look(json!({"x": 0, "y": -3.38, "z": 5})).unwrap(),
        SHORT,
    )
    .unwrap();
    assert!(outcome.confirmed, "{:?}", outcome.notes);
    assert_eq!(
        outcome.evidence["requested"],
        json!({"yaw": 0.0, "pitch": 45.0})
    );
    assert_eq!(acts(&seen)[0]["duration"], "continuous");
    let outcome = perform(
        &mut game,
        &look(json!({"x": 5, "y": 1.62, "z": 0})).unwrap(),
        SHORT,
    )
    .unwrap();
    assert!(!outcome.confirmed);
    assert!(
        outcome.notes[0].contains("not within"),
        "{:?}",
        outcome.notes
    );
}

#[test]
fn simulated_drops_split_the_stack_to_the_quantity() {
    let (mut game, seen) = fake(true, bot());
    let drop = Request::parse("item_drop", json!({"actor": "Bot", "quantity": 3})).unwrap();
    let outcome = perform(&mut game, &drop, SHORT).unwrap();
    assert!(outcome.confirmed, "{:?}", outcome.notes);
    assert_eq!(outcome.evidence["dropped"], 3);
    assert_eq!(acts(&seen), [json!({"action": "drop_amount", "amount": 3})]);
}

#[test]
fn sequence_steps_start_on_time_while_earlier_ones_wait_for_their_event() {
    let (mut game, _) = fake(true, bot());
    // Nothing reports Bot's item use, so the first step waits the whole 400 ms for it.
    let args: SequenceArgs = serde_json::from_value(json!({
        "actor": "Bot",
        "steps": [
            {"action": "item_use", "after_ms": 0},
            {"action": "attack", "args": {"entity_id": "-5"}, "after_ms": 100}
        ]
    }))
    .unwrap();
    let result = sequence_with(&mut game, args, SHORT).unwrap();
    let steps = result["steps"].as_array().unwrap();
    let late = steps[1]["started_ms"].as_u64().unwrap();
    assert!((100..300).contains(&late), "{late}");
    assert_eq!(steps[0]["outcome"]["confirmed"], false);
    assert!(
        steps[0]["outcome"]["notes"][0]
            .as_str()
            .unwrap()
            .contains("within 400 ms"),
        "{}",
        steps[0]
    );
    assert_eq!(steps[1]["outcome"]["confirmed"], true);
    assert_eq!(result["confirmed"], 1);
}

/// The GameTest pack with Bot in a game whose tick follows the clock and which takes
/// `latency` to answer each command.
fn slow_bot(latency: Duration) -> (Fake, Seen) {
    let (game, seen) = fake(true, bot());
    {
        let mut script = game.script.lock().unwrap();
        script.clock = Some(Instant::now());
        script.latency = latency;
    }
    (game, seen)
}

#[test]
fn sequence_steps_act_on_their_tick_when_the_game_answers_slowly() {
    // Each command takes two ticks to answer, like a busy game.
    let (mut game, _) = slow_bot(Duration::from_millis(100));
    let args: SequenceArgs = serde_json::from_value(json!({
        "actor": "Bot",
        "steps": [
            {"action": "look", "args": {"entity_id": "-5"}, "at_tick": 0},
            {"action": "attack", "args": {"entity_id": "-5"}, "at_tick": 2},
            {"action": "attack", "args": {"entity_id": "-5"}, "at_tick": 14},
            {"action": "attack", "args": {"entity_id": "-5"}, "at_tick": 26},
            {"action": "attack", "args": {"entity_id": "-5"}, "at_tick": 38}
        ]
    }))
    .unwrap();
    let result = sequence_with(&mut game, args, SHORT).unwrap();
    let started: Vec<u64> = result["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| step["started_tick"].as_u64().unwrap())
        .collect();
    assert_eq!(started, [0, 2, 14, 26, 38], "{result}");
    assert_eq!(result["confirmed"], 4, "{result}");
}

fn run_sequence(game: &mut Fake, args: Value) -> Result<Value> {
    sequence_with(game, serde_json::from_value(args).unwrap(), SHORT)
}

/// Steve in a game whose tick follows the clock and which takes `latency` to answer each
/// command. Every attack but the first hits pig -5.
fn slow_steve(latency: Duration) -> Fake {
    let (mut game, _) = fake(true, steve("-5"));
    {
        let mut script = game.script.lock().unwrap();
        script.clock = Some(Instant::now());
        script.latency = latency;
    }
    let mut clicks = 0;
    game.react = Box::new(move |request, script| {
        if request.press == ["key.attack"] {
            clicks += 1;
            if clicks > 1 {
                script.push("entity_hit_entity", hit("Steve"));
            }
        }
    });
    game
}

/// Whether `action` started within a tick of its at_tick.
fn on_tick(action: &Value) -> bool {
    let (at, started) = (
        action["at_tick"].as_u64().unwrap(),
        action["started_tick"].as_u64().unwrap(),
    );
    at.abs_diff(started) <= 1
}

#[test]
fn self_steps_start_on_their_tick_while_earlier_clicks_wait_for_their_event() {
    // Each command takes a tick to answer. The first attack never hits, so it waits the
    // whole 400 ms (8 ticks) for its event while the next ones go out every 4 ticks.
    let mut game = slow_steve(TICK);
    let attack = |at: u64| json!({"action": "attack", "args": {"entity_id": "-5"}, "at_tick": at});
    let result = run_sequence(
        &mut game,
        json!({"steps": [
            {"action": "look", "args": {"entity_id": "-5"}, "at_tick": 0},
            attack(6), attack(10), attack(14), attack(18)
        ]}),
    )
    .unwrap();
    let steps = result["steps"].as_array().unwrap();
    assert!(steps.iter().all(on_tick), "{result}");
    assert_eq!(result["confirmed"], 4, "{result}");
    // A later click's hit does not count for the first one.
    assert_eq!(steps[1]["outcome"]["confirmed"], false, "{result}");
    let notes = steps[1]["outcome"]["notes"].to_string();
    assert!(notes.contains("within 400 ms"), "{notes}");
    assert_eq!(result["notes"], json!([]), "{result}");
}

#[test]
fn repeated_self_attacks_keep_their_schedule() {
    let mut game = slow_steve(TICK);
    let result = run_sequence(
        &mut game,
        json!({"steps": [{
            "action": "attack", "args": {"entity_id": "-5"}, "at_tick": 2,
            "repeat": {"every_ticks": 4, "times": 5}
        }]}),
    )
    .unwrap();
    let runs = result["steps"][0]["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 5, "{result}");
    let planned: Vec<u64> = runs
        .iter()
        .map(|run| run["at_tick"].as_u64().unwrap())
        .collect();
    assert_eq!(planned, [2, 6, 10, 14, 18]);
    assert!(runs.iter().all(on_tick), "{result}");
    assert_eq!(result["confirmed"], 4, "{result}");
}

#[test]
fn holds_of_the_same_key_do_not_overlap() {
    let (mut game, _) = fake(true, steve("-5"));
    let hold = |at: u64| json!({"action": "item_use", "args": {"mode": "hold", "ms": 200}, "after_ms": at});
    let result = run_sequence(&mut game, json!({"steps": [hold(0), hold(50)]})).unwrap();
    let uses: Vec<(bool, bool)> = game
        .inputs
        .iter()
        .map(|input| (input.hold == ["key.use"], input.release == ["key.use"]))
        .collect();
    assert_eq!(
        uses,
        [(true, false), (false, true), (true, false), (false, true)],
        "{result}"
    );
    let second = result["steps"][1]["started_ms"].as_u64().unwrap();
    assert!(second < 100, "{result}");
}

#[test]
fn an_entity_out_of_reach_is_not_clicked_and_the_action_returns_at_once() {
    let (mut game, _) = fake(true, steve("-5"));
    game.script.lock().unwrap().gap = 4.2;
    let started = Instant::now();
    let outcome = perform(&mut game, &attack_pig(), SHORT).unwrap();
    assert!(started.elapsed() < SHORT / 2, "{:?}", started.elapsed());
    assert!(game.inputs.is_empty());
    assert!(!outcome.confirmed);
    let notes = outcome.notes.join(" ");
    assert!(
        notes.contains("4.2 blocks away") && notes.contains("approach: true"),
        "{notes}"
    );
}

#[test]
fn approach_walks_within_reach_then_clicks() {
    let walker = |game: &mut Fake| {
        game.script.lock().unwrap().gap = 5.5;
        game.react = Box::new(|request, script| {
            if let Some(axes) = &request.r#move {
                script.walking = axes.forward == Some(1.0);
            }
            if request.press == ["key.attack"] {
                script.push("entity_hit_entity", hit("Steve"));
            }
        });
    };
    let (mut game, _) = fake(true, steve("-5"));
    walker(&mut game);
    let request = Request::parse(
        "attack",
        json!({"entity_id": "-5", "approach": true, "approach_ms": 3000}),
    )
    .unwrap();
    let outcome = perform(&mut game, &request, SHORT).unwrap();
    assert!(outcome.confirmed, "{:?}", outcome.notes);
    let forward = |input: &InputRequest| input.r#move.as_ref().and_then(|axes| axes.forward);
    assert_eq!(game.inputs.len(), 3, "{:?}", game.inputs);
    assert_eq!(forward(&game.inputs[0]), Some(1.0));
    assert_eq!(forward(&game.inputs[1]), Some(0.0));
    assert_eq!(game.inputs[2].press, ["key.attack"]);
    assert_eq!(outcome.evidence["approach"]["reached"], true);
    assert_eq!(outcome.evidence["approach"]["distance"], 2.5);

    // Out of time before it gets close: it stops walking and does not click.
    let (mut game, _) = fake(true, steve("-5"));
    walker(&mut game);
    let request = Request::parse(
        "attack",
        json!({"entity_id": "-5", "approach": true, "approach_ms": 200}),
    )
    .unwrap();
    let outcome = perform(&mut game, &request, SHORT).unwrap();
    assert!(!outcome.confirmed);
    assert!(game.inputs.iter().all(|input| input.press.is_empty()));
    assert_eq!(forward(game.inputs.last().unwrap()), Some(0.0));
    assert!(
        outcome.notes.join(" ").contains("still"),
        "{:?}",
        outcome.notes
    );

    assert!(Request::parse("attack", json!({"entity_id": "-5", "approach_ms": 10})).is_ok());
    let (mut game, _) = fake(true, steve("-5"));
    let stray = Request::parse("attack", json!({"entity_id": "-5", "approach_ms": 10})).unwrap();
    assert!(perform(&mut game, &stray, SHORT).is_err());
    let bot = Request::parse(
        "attack",
        json!({"actor": "Bot", "entity_id": "-5", "approach": true}),
    )
    .unwrap();
    let error = perform(&mut game, &bot, SHORT).unwrap_err().to_string();
    assert!(error.contains("actor self"), "{error}");
}

#[test]
fn a_repeated_step_runs_until_its_event_comes() {
    let (mut game, _) = fake(true, steve("-5"));
    let mut hits = 0;
    game.react = Box::new(move |request, script| {
        if request.press == ["key.attack"] {
            script.push("entity_hit_entity", hit("Steve"));
            hits += 1;
            if hits == 3 {
                script.push(
                    "entity_die",
                    json!({"entity": {"id": "-5", "type": "minecraft:pig"}}),
                );
            }
        }
    });
    let result = run_sequence(
        &mut game,
        json!({"steps": [{
            "action": "attack", "args": {"entity_id": "-5"},
            "repeat": {"until": {"event": {"name": "entity_die", "contains": "minecraft:pig"}}, "every_ms": 10}
        }]}),
    )
    .unwrap();
    let step = &result["steps"][0];
    assert_eq!(step["ended"], "until", "{result}");
    assert_eq!(step["runs"].as_array().unwrap().len(), 3, "{result}");
    assert_eq!(step["until"]["event"]["name"], "entity_die");
    assert_eq!(result["confirmed"], 3);
}

#[test]
fn a_repeated_step_stops_after_its_times() {
    let (mut game, _) = fake(true, steve("-5"));
    game.react = Box::new(|request, script| {
        if request.press == ["key.attack"] {
            script.push("entity_hit_entity", hit("Steve"));
        }
    });
    let result = run_sequence(
        &mut game,
        json!({"steps": [{
            "action": "attack", "args": {"entity_id": "-5"},
            "repeat": {"times": 4, "until": {"event": {"name": "entity_die"}}, "every_ms": 10}
        }]}),
    )
    .unwrap();
    assert_eq!(result["steps"][0]["ended"], "times", "{result}");
    assert_eq!(game.inputs.len(), 4);
    assert_eq!(result["confirmed"], 4);
}

/// Answers `testfor` for a zombie within 4 blocks from the `after`th check on; no creeper
/// is ever near.
fn zombie_after(after: u32) -> Box<Answer> {
    let checks = Arc::new(std::sync::atomic::AtomicU32::new(0));
    Box::new(move |line, _| match line {
        "testfor @e[type=minecraft:zombie,r=4]" => {
            let check = checks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some(if check >= after {
                json_reply(json!({}))
            } else {
                failure("No targets matched selector")
            })
        }
        "testfor @e[type=minecraft:creeper,r=4]" => Some(failure("No targets matched selector")),
        _ => None,
    })
}

#[test]
fn steps_wait_for_their_condition_and_skip_when_it_does_not_hold() {
    let (mut game, _) = fake(true, zombie_after(2));
    let zombie = json!({"entity": {"matches": "minecraft:zombie", "radius": 4}});
    let creeper = json!({"entity": {"matches": "minecraft:creeper", "radius": 4}});
    let result = run_sequence(
        &mut game,
        json!({"steps": [
            {"action": "stop", "wait_for": zombie},
            {"action": "stop", "wait_for": creeper, "wait_ms": 100},
            {"action": "stop", "if": creeper},
            {"action": "stop", "if": {"not": creeper}}
        ]}),
    )
    .unwrap();
    let steps = result["steps"].as_array().unwrap();
    // Two checks a tick apart failed before the zombie came.
    let waited = steps[0]["wait_for"]["waited_ms"].as_u64().unwrap();
    assert!(waited >= 90, "{result}");
    assert_eq!(steps[0]["outcome"]["confirmed"], true);
    assert!(
        steps[1]["skipped"]
            .as_str()
            .unwrap()
            .starts_with("wait_for did not hold within"),
        "{result}"
    );
    assert_eq!(steps[2]["skipped"], "Its if condition did not hold.");
    assert_eq!(steps[3]["outcome"]["confirmed"], true);
    assert_eq!(game.inputs.len(), 2);
}

#[test]
fn stop_when_skips_the_steps_left_and_stops_the_actors_that_acted() {
    let reads = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let (mut game, _) = fake(
        true,
        Box::new(move |line, _| {
            (line == "re_mcp:state").then(|| {
                // Health falls from 20 to 4 after a few reads.
                let read = reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let health = if read < 3 { 20 } else { 4 };
                json_reply(json!({"name": "Steve", "health": {"current": health, "max": 20}}))
            })
        }),
    );
    let started = Instant::now();
    let result = run_sequence(
        &mut game,
        json!({
            "stop_when": {"state": {"path": "health.current", "below": 6}},
            "timeout_ms": 10000,
            "steps": [
                {"action": "stop", "after_ms": 0},
                {"action": "stop", "after_ms": 5000}
            ]
        }),
    )
    .unwrap();
    assert!(started.elapsed() < Duration::from_secs(2), "{result}");
    assert_eq!(
        result["steps"][1]["skipped"],
        "stop_when held before this step started."
    );
    let stopped = &result["stopped"];
    assert_eq!(
        stopped["detail"],
        json!({"state": "health.current", "value": 4}),
        "{result}"
    );
    assert_eq!(stopped["actors_stopped"][0]["actor"], "self");
    // The step's stop and the sequence's.
    assert_eq!(game.inputs.len(), 2);
}

#[test]
fn state_conditions_need_the_bridge_before_anything_acts() {
    let (mut game, _) = fake(false, Box::new(|_, _| None));
    let error = run_sequence(
        &mut game,
        json!({
            "stop_when": {"state": {"path": "health.current", "below": 6}},
            "steps": [{"action": "stop"}]
        }),
    )
    .unwrap_err();
    assert!(error.to_string().contains("Bridge"), "{error}");
    assert!(game.inputs.is_empty());
}

#[test]
fn faces_place_blocks_beside_and_aim_at_their_centres() {
    assert_eq!(beside([1, 64, 2], Face::Up), [1, 65, 2]);
    assert_eq!(beside([1, 64, 2], Face::West), [0, 64, 2]);
    assert_eq!(face_point([1, 64, 2], Some(Face::North)), [1.5, 64.5, 2.0]);
    assert_eq!(face_point([1, 64, 2], None), [1.5, 64.5, 2.5]);
    assert_eq!(face_named("East"), Some(Face::East));
    assert_eq!(
        block_of(&json!({"x": -0.5, "y": 64.9, "z": 2.0})),
        Some([-1, 64, 2])
    );
}

/// A fake game while a background run uses it: at each wait point it reads the run's status
/// as a client would, and can ask the run to stop or say the client went away.
struct Watching<'a> {
    game: &'a mut Fake,
    background: &'a Background,
    run_id: String,
    cursor: usize,
    /// Every status that brought news.
    statuses: Vec<Value>,
    /// Whether a status call with wait_ms waited while there was no news.
    waited: bool,
    /// Call sequence_stop once this many results came.
    stop_after: Option<usize>,
    /// The client goes away once this many results came.
    gone_after: Option<usize>,
    /// What background.conflict said to calls made while the run waited.
    refusals: Vec<String>,
}

impl<'a> Watching<'a> {
    fn new(game: &'a mut Fake, background: &'a Background, run_id: &str) -> Self {
        Self {
            game,
            background,
            run_id: run_id.to_owned(),
            cursor: 0,
            statuses: Vec::new(),
            waited: false,
            stop_after: None,
            gone_after: None,
            refusals: Vec::new(),
        }
    }

    fn status(&self, since: usize, wait_ms: u64) -> Result<Value> {
        self.background.status(
            serde_json::from_value(
                json!({"run_id": self.run_id, "since": since, "wait_ms": wait_ms, "verbose": true}),
            )
            .unwrap(),
        )
    }
}

impl Game for Watching<'_> {
    fn world(&mut self) -> Result<World> {
        self.game.world()
    }

    fn cursor(&mut self) -> Result<EventCursor> {
        self.game.cursor()
    }

    fn wait_event(
        &mut self,
        after: &EventCursor,
        timeout: Duration,
        matches: &dyn Fn(&HubEvent) -> bool,
    ) -> Result<(Option<HubEvent>, Vec<String>)> {
        self.game.wait_event(after, timeout, matches)
    }

    fn input(&mut self, request: InputRequest) -> Result<()> {
        self.game.input(request)
    }

    fn pause(&mut self, duration: Duration) {
        thread::sleep(duration);
        for (tool, arguments) in [
            ("attack", json!({})),
            ("attack", json!({"actor": "Bot"})),
            ("player_get", json!({})),
            ("action_sequence", json!({})),
        ] {
            if let Err(error) = self.background.conflict(tool, &arguments) {
                let refusal = format!("{tool}: {error}");
                if !self.refusals.contains(&refusal) {
                    self.refusals.push(refusal);
                }
            }
        }
        match self.status(self.cursor, 1000) {
            Err(Error::Waiting) => self.waited = true,
            Err(error) => panic!("{error}"),
            Ok(status) => {
                self.cursor = status["cursor"].as_u64().unwrap() as usize;
                self.statuses.push(status);
            }
        }
        if self.stop_after.is_some_and(|after| self.cursor >= after) {
            let stop = StopArgs::deserialize(json!({"run_id": self.run_id})).unwrap();
            assert!(matches!(self.background.stop(stop), Err(Error::Waiting)));
        }
    }

    fn abandoned(&mut self) -> bool {
        self.gone_after.is_some_and(|after| self.cursor >= after)
    }

    fn native_list(&mut self, radius: f64, entity_type: Option<&str>) -> Result<Vec<Value>> {
        self.game.native_list(radius, entity_type)
    }
}

/// Queue `args` as a background run; its id and the reply.
fn queue(game: &mut Fake, background: &mut Background, args: Value) -> (String, Value) {
    let prepared = prepare(game, serde_json::from_value(args).unwrap(), SHORT).unwrap();
    let reply = background.queue(prepared).unwrap();
    (reply["run_id"].as_str().unwrap().to_owned(), reply)
}

/// Steve, whose every attack hits pig -5.
fn hitting_steve() -> Fake {
    let (mut game, _) = fake(true, steve("-5"));
    game.react = Box::new(|request, script| {
        if request.press == ["key.attack"] {
            script.push("entity_hit_entity", hit("Steve"));
        }
    });
    game
}

#[test]
fn a_background_run_returns_its_plan_and_reports_steps_as_they_settle() {
    let mut game = hitting_steve();
    let mut background = Background::default();
    let (id, reply) = queue(
        &mut game,
        &mut background,
        json!({"background": true, "steps": [
            {"action": "attack", "args": {"entity_id": "-5"}, "after_ms": 0},
            {"action": "attack", "args": {"entity_id": "-5"}, "after_ms": 600},
            {"action": "stop", "after_ms": 1200}
        ]}),
    );
    assert_eq!(reply["clock"], "wall");
    assert_eq!(
        reply["planned"],
        json!([
            {"index": 0, "action": "attack", "after_ms": 0},
            {"index": 1, "action": "attack", "after_ms": 600},
            {"index": 2, "action": "stop", "after_ms": 1200}
        ])
    );
    // A second run is refused while this one has not ended.
    let second = prepare(
        &mut game,
        serde_json::from_value(json!({"steps": [{"action": "stop"}]})).unwrap(),
        SHORT,
    )
    .unwrap();
    let refused = background.queue(second).unwrap_err().to_string();
    assert!(refused.contains(&id), "{refused}");

    let queued = background.take_queued().unwrap();
    let mut watching = Watching::new(&mut game, &background, &id);
    run_queued(&mut watching, queued);
    let (statuses, waited, refusals) = (
        std::mem::take(&mut watching.statuses),
        watching.waited,
        std::mem::take(&mut watching.refusals),
    );

    // News came one step at a time while the run ran, and a status with wait_ms waited
    // while there was none.
    assert!(waited);
    let running: Vec<&Value> = statuses
        .iter()
        .filter(|s| s["state"] == "running")
        .collect();
    assert!(!running.is_empty(), "{statuses:?}");
    let seen: Vec<Value> = statuses
        .iter()
        .flat_map(|status| status["steps"].as_array().unwrap().clone())
        .map(|step| step["index"].clone())
        .collect();
    assert_eq!(seen, [json!(0), json!(1)], "{statuses:?}");
    assert_eq!(statuses[0]["steps"][0]["outcome"]["confirmed"], true);
    // Calls that would fight over the player's input are refused; others are not.
    assert_eq!(refusals.len(), 2, "{refusals:?}");
    assert!(refusals[0].starts_with("attack: ") && refusals[0].contains(&id));
    assert!(refusals[1].starts_with("action_sequence: "));

    let done = watching.status(0, 0).unwrap();
    assert_eq!(done["state"], "finished", "{done}");
    assert_eq!(done["cursor"], 3);
    assert_eq!(done["current"], Value::Null);
    assert_eq!(done["confirmed"], 3);
    let late = watching.status(2, 30_000).unwrap();
    assert_eq!(late["steps"].as_array().unwrap().len(), 1);
    assert_eq!(late["steps"][0]["index"], 2);
    // The finished run no longer holds the actor.
    assert!(background.conflict("attack", &json!({})).is_ok());
}

#[test]
fn sequence_stop_skips_the_rest_and_releases_the_player() {
    let mut game = hitting_steve();
    let mut background = Background::default();
    let (id, _) = queue(
        &mut game,
        &mut background,
        json!({"background": true, "steps": [
            {"action": "attack", "args": {"entity_id": "-5"}, "after_ms": 0},
            {"action": "movement_set", "args": {"forward": 1, "duration_ms": 3000}, "after_ms": 100},
            {"action": "attack", "args": {"entity_id": "-5"}, "after_ms": 4000}
        ]}),
    );
    let started = Instant::now();
    let queued = background.take_queued().unwrap();
    let mut watching = Watching::new(&mut game, &background, &id);
    watching.stop_after = Some(1);
    run_queued(&mut watching, queued);
    drop(watching);
    assert!(started.elapsed() < Duration::from_secs(2));

    let stop = StopArgs::deserialize(json!({"run_id": id, "verbose": true})).unwrap();
    let result = background.stop(stop).unwrap();
    assert_eq!(result["state"], "stopped", "{result}");
    assert_eq!(result["stopped"]["by"], "sequence_stop");
    assert_eq!(
        result["steps"][2]["skipped"],
        "sequence_stop ended the sequence before this step started."
    );
    let walk = &result["steps"][1];
    assert!(
        walk["interrupted"]
            .as_str()
            .unwrap()
            .starts_with("sequence_stop ended")
            || walk.get("outcome").is_some(),
        "{result}"
    );
    assert_eq!(result["stopped"]["actors_stopped"][0]["actor"], "self");
    // The last input lets go of every held control.
    assert!(game.inputs.last().unwrap().release_all);
}

#[test]
fn fail_fast_stops_after_an_unconfirmed_attack() {
    // Nothing confirms the attack.
    let (mut game, _) = fake(true, steve("-5"));
    let started = Instant::now();
    let result = run_sequence(
        &mut game,
        json!({"fail_fast": true, "steps": [
            {"action": "attack", "args": {"entity_id": "-5"}, "after_ms": 0},
            {"action": "attack", "args": {"entity_id": "-5"}, "after_ms": 2000}
        ]}),
    )
    .unwrap();
    assert!(started.elapsed() < Duration::from_millis(1500), "{result}");
    assert_eq!(result["steps"][0]["outcome"]["confirmed"], false);
    assert_eq!(
        result["steps"][1]["skipped"],
        "fail_fast ended the sequence before this step started."
    );
    assert_eq!(result["stopped"]["by"], "fail_fast");
    assert_eq!(result["stopped"]["step"], 0);
    assert_eq!(result["failures"], 1);
    assert_eq!(result["stopped"]["actors_stopped"][0]["actor"], "self");

    // max_failures lets that many pass.
    let (mut game, _) = fake(true, steve("-5"));
    let result = run_sequence(
        &mut game,
        json!({"max_failures": 2, "steps": [
            {"action": "attack", "args": {"entity_id": "-5"}, "after_ms": 0},
            {"action": "attack", "args": {"entity_id": "-5"}, "after_ms": 50},
            {"action": "attack", "args": {"entity_id": "-5"}, "after_ms": 3000}
        ]}),
    )
    .unwrap();
    assert_eq!(result["stopped"]["by"], "max_failures", "{result}");
    assert_eq!(result["failures"], 2);
    assert!(result["steps"][2]["skipped"].is_string());

    let (mut game, _) = fake(true, steve("-5"));
    let error = run_sequence(
        &mut game,
        json!({"fail_fast": true, "max_failures": 2, "steps": [{"action": "stop"}]}),
    )
    .unwrap_err();
    assert!(error.to_string().contains("not both"), "{error}");
}

#[test]
fn an_unmet_expect_is_a_failure_reported_with_its_evidence() {
    let mut game = hitting_steve();
    let result = run_sequence(
        &mut game,
        json!({"fail_fast": true, "steps": [
            {"action": "attack", "args": {"entity_id": "-5"}, "after_ms": 0,
             "expect": {"confirmed": true, "holds": {"event": {"name": "entity_hit_entity"}}}},
            {"action": "attack", "args": {"entity_id": "-5"}, "after_ms": 50,
             "expect": {"holds": {"event": {"name": "entity_die", "contains": "minecraft:pig"}}}},
            {"action": "attack", "args": {"entity_id": "-5"}, "after_ms": 3000}
        ]}),
    )
    .unwrap();
    let met = &result["steps"][0];
    assert_eq!(met["expect"]["met"], true, "{result}");
    assert_eq!(met["expect"]["confirmed"], true);
    assert_eq!(met["expect"]["holds"]["event"]["name"], "entity_hit_entity");
    let unmet = &result["steps"][1];
    assert_eq!(unmet["outcome"]["confirmed"], true);
    assert_eq!(unmet["expect"]["met"], false, "{result}");
    assert_eq!(unmet["expect"]["holds"], Value::Null);
    assert_eq!(result["stopped"]["by"], "fail_fast");
    assert_eq!(result["stopped"]["step"], 1);
    assert!(result["steps"][2]["skipped"].is_string());

    let (mut game, _) = fake(true, steve("-5"));
    let error = run_sequence(
        &mut game,
        json!({"steps": [{"action": "stop", "expect": {}}]}),
    )
    .unwrap_err();
    assert!(
        error.to_string().starts_with("Step 0: expect needs"),
        "{error}"
    );
}

#[test]
fn a_run_stops_and_releases_input_when_the_client_goes_away() {
    let mut game = hitting_steve();
    let mut background = Background::default();
    let (id, _) = queue(
        &mut game,
        &mut background,
        json!({"background": true, "steps": [
            {"action": "attack", "args": {"entity_id": "-5"}, "after_ms": 0},
            {"action": "attack", "args": {"entity_id": "-5"}, "after_ms": 3000}
        ]}),
    );
    let started = Instant::now();
    let queued = background.take_queued().unwrap();
    let mut watching = Watching::new(&mut game, &background, &id);
    watching.gone_after = Some(1);
    run_queued(&mut watching, queued);
    drop(watching);
    assert!(started.elapsed() < Duration::from_secs(2));
    let status = background
        .status(serde_json::from_value(json!({"run_id": id})).unwrap())
        .unwrap();
    assert_eq!(status["state"], "stopped", "{status}");
    assert_eq!(status["stopped"]["by"], "disconnect");
    assert!(game.inputs.last().unwrap().release_all);
}

/// Steve's world with room to spawn: air everywhere except stone at y 5, nothing loaded past
/// x 100, and every /summon spawning a zombie with id -5 (pig -5's answers stay). The
/// raycast meets glass once `script.walking` is set, which stands for a wall.
fn arena() -> Box<Answer> {
    let steve = steve("-5");
    Box::new(move |line, script| {
        if let Some(reply) = steve(line, script) {
            return Some(reply);
        }
        if let Some(at) = line.strip_prefix("re_mcp:block ") {
            let [x, y, _]: [i64; 3] = at
                .split(' ')
                .map(|axis| axis.parse().unwrap())
                .collect::<Vec<_>>()
                .try_into()
                .unwrap();
            return Some(match (x, y) {
                _ if x > 100 => failure("re_mcp:block failed: That block is not loaded"),
                (_, 5) => {
                    json_reply(json!({"id": "minecraft:stone", "air": false, "liquid": false}))
                }
                _ => json_reply(json!({"id": "minecraft:air", "air": true, "liquid": false})),
            });
        }
        if line.starts_with("re_mcp:raycast ") {
            let block = match script.walking {
                true => json!({"id": "minecraft:glass", "location": {"x": 0, "y": 1, "z": 1}}),
                false => Value::Null,
            };
            return Some(json_reply(json!({ "block": block, "entities": [] })));
        }
        if let Some(rest) = line.strip_prefix("summon ") {
            let parts: Vec<&str> = rest.split(' ').collect();
            let at = |index: usize| parts[parts.len() - 3 + index].parse::<f64>().unwrap();
            // An item drops nearby first; it is not the summoned entity.
            script.push(
                "entity_spawn",
                json!({"entity": {"id": "-7", "type": "minecraft:item", "location": {"x": 0.0, "y": 0.0, "z": 0.0}}}),
            );
            script.push(
                "entity_spawn",
                json!({"entity": {"id": "-5", "type": parts[0], "location": {"x": at(0), "y": at(1), "z": at(2)}}}),
            );
            return Some(json!({"statusCode": 0, "statusMessage": "Object successfully summoned"}));
        }
        if line.starts_with("effect ") {
            return Some(json!({"statusCode": 0, "statusMessage": "Gave effect"}));
        }
        None
    })
}

fn spawn_request(args: Value) -> Request {
    Request::parse("spawn", args).unwrap()
}

#[test]
fn spawn_summons_ahead_of_the_player_and_reads_the_new_id() {
    let (mut game, seen) = fake(true, arena());
    let outcome = perform(
        &mut game,
        &spawn_request(json!({
            "type": "zombie", "ahead": 3,
            "effects": [{"id": "slowness", "seconds": 60, "amplifier": 255, "hide": true}]
        })),
        SHORT,
    )
    .unwrap();
    assert!(outcome.confirmed, "{:?}", outcome.notes);
    assert_eq!(outcome.evidence["id"], "-5");
    assert_eq!(outcome.evidence["matched_by"], "entity_spawn");
    assert_eq!(outcome.evidence["line_of_sight"]["clear"], true);
    assert_eq!(outcome.evidence["effects"][0]["applied"], true);
    let seen = seen.lock().unwrap();
    // Steve faces +z (yaw 0), so 3 ahead is z 3.
    assert!(
        seen.contains(&"summon minecraft:zombie 0 0 3".to_owned()),
        "{seen:?}"
    );
    assert!(
        seen.contains(
            &"effect @e[type=minecraft:zombie,x=0,y=0,z=3,r=2,c=1] slowness 60 255 true".to_owned()
        ),
        "{seen:?}"
    );
    drop(seen);

    // A wall between the player and the spot shows in line_of_sight.
    game.script.lock().unwrap().walking = true;
    let walled = perform(
        &mut game,
        &spawn_request(json!({"type": "zombie", "ahead": 3})),
        SHORT,
    )
    .unwrap();
    assert_eq!(walled.evidence["line_of_sight"]["clear"], false);
    assert_eq!(
        walled.evidence["line_of_sight"]["blocked_by"]["id"],
        "minecraft:glass"
    );

    let solid = perform(
        &mut game,
        &spawn_request(json!({"type": "zombie", "up": 4})),
        SHORT,
    )
    .unwrap_err()
    .to_string();
    assert!(
        solid.contains("solid block (minecraft:stone at 0 5 0"),
        "{solid}"
    );
    let far = json!({"type": "zombie", "position": {"x": 500, "y": 0, "z": 0}});
    let unloaded = perform(&mut game, &spawn_request(far), SHORT)
        .unwrap_err()
        .to_string();
    assert!(unloaded.contains("outside the loaded chunks"), "{unloaded}");
}

/// Every attack on -5 hits it; the third kills it.
fn deadly_steve() -> Fake {
    let (mut game, _) = fake(true, arena());
    game.script.lock().unwrap().clock = Some(Instant::now());
    let mut hits = 0;
    game.react = Box::new(move |request, script| {
        if request.press == ["key.attack"] {
            script.push("entity_hit_entity", hit("Steve"));
            hits += 1;
            if hits == 3 {
                script.push(
                    "entity_die",
                    json!({"entity": {"id": "-5", "type": "minecraft:zombie"}, "cause": "entityAttack", "killer": {"name": "Steve"}}),
                );
            }
        }
    });
    game
}

#[test]
fn a_fight_names_its_spawned_entity_and_summarizes_hits_and_the_kill() {
    let mut game = deadly_steve();
    let result = run_sequence(
        &mut game,
        json!({"steps": [
            {"action": "spawn", "as": "A", "args": {"type": "zombie", "ahead": 3}},
            {"action": "attack", "args": {"entity_id": "$A"},
             "repeat": {"every_ticks": 2, "until": {"event": {"name": "entity_die", "contains": "$A"}}}}
        ]}),
    )
    .unwrap();
    let fight = &result["steps"][1];
    assert_eq!(fight["target"], "-5", "{result}");
    assert_eq!(fight["ended"], "until", "{result}");
    assert_eq!(fight["runs"].as_array().unwrap().len(), 3, "{result}");
    let target = &result["summary"]["targets"]["-5"];
    assert_eq!(target["label"], "A", "{result}");
    assert_eq!(target["type"], "minecraft:zombie");
    assert_eq!(target["hits"], 3, "{result}");
    assert_eq!(target["misses"], 0);
    assert_eq!(target["killed_by"]["step"], 1, "{result}");
    assert_eq!(target["killed_by"]["run"], 2, "{result}");
    assert_eq!(target["final"]["state"], "dead");
    assert_eq!(result["summary"]["totals"]["kills"], 1);
    assert!(result["timing"]["first_tick"].as_u64() <= result["timing"]["last_tick"].as_u64());
    // The attacks aimed at the spawned zombie by its id.
    assert!(
        game.inputs
            .iter()
            .filter(|input| input.press == ["key.attack"])
            .count()
            == 3
    );
}

/// A world with cheats and without the Bridge pack: Steve at the origin facing +z, and a
/// /summon that makes zombie -5 at z 3. With the fake unstable layer (`script.native`) the
/// zombie stands there once that layer reads it; without, it always does.
fn plain_arena() -> Box<Answer> {
    Box::new(|line, script| {
        let targets = |list: Value| json!({"statusCode": 0, "details": list.to_string()});
        let there = script
            .native
            .as_ref()
            .is_none_or(|health| health.contains_key("-5"));
        Some(match line {
            "querytarget @s" => targets(json!([{
                "uniqueId": "-1", "id": 1, "dimension": 0, "yRot": 0.0,
                "position": {"x": 0.0, "y": 1.62, "z": 0.0},
            }])),
            _ if line.starts_with("querytarget @e") && there => targets(json!([{
                "uniqueId": "-5", "id": 5, "dimension": 0, "yRot": 0.0,
                "position": {"x": 0.0, "y": 0.0, "z": 3.0},
            }])),
            _ if line.starts_with("querytarget @e") => failure("No targets matched selector"),
            _ if line.starts_with("summon minecraft:zombie ") => {
                if let Some(health) = script.native.as_mut() {
                    health.insert("-5".into(), [20.0, 20.0]);
                }
                json!({"statusCode": 0, "statusMessage": "Object successfully summoned"})
            }
            _ if line.starts_with("tp ") || line.starts_with("effect ") => {
                json!({"statusCode": 0, "statusMessage": "done"})
            }
            _ => return None,
        })
    })
}

/// [`plain_arena`] with the unstable layer, where every attack takes 8 of zombie -5's health
/// and the third leaves it at 0.
fn plain_fight() -> Fake {
    let (mut game, _) = fake(false, plain_arena());
    game.script.lock().unwrap().native = Some(HashMap::new());
    game.react = Box::new(|request, script| {
        if request.press == ["key.attack"]
            && let Some(health) = script.native.as_mut().and_then(|all| all.get_mut("-5"))
        {
            health[0] = (health[0] - 8.0).max(0.0);
        }
    });
    game
}

#[test]
fn without_the_bridge_an_attack_reads_the_hit_back_natively() {
    let mut game = plain_fight();
    game.script.lock().unwrap().native = Some(HashMap::from([("-5".into(), [20.0, 20.0])]));
    let outcome = perform(&mut game, &attack_pig(), SHORT).unwrap();
    assert!(outcome.confirmed, "{:?}", outcome.notes);
    assert_eq!(outcome.source, "native");
    assert_eq!(outcome.evidence["health"]["before"], 20.0);
    assert_eq!(outcome.evidence["health"]["after"], 12.0);
    assert_eq!(game.inputs.last().unwrap().press, ["key.attack"]);

    // A swing that takes no health is a miss, with the reason.
    game.react = Box::new(|_, _| {});
    let missed = perform(&mut game, &attack_pig(), SHORT).unwrap();
    assert!(!missed.confirmed);
    assert_eq!(missed.reason.as_ref().unwrap().0, "no_damage");
    assert_eq!(missed.evidence["health"]["after"], 12.0);

    // Without the unstable layer nothing confirms it, and the notes say why.
    let mut blind = fake(false, plain_arena()).0;
    let unconfirmed = perform(&mut blind, &attack_pig(), SHORT).unwrap();
    assert!(!unconfirmed.confirmed);
    assert_eq!(blind.inputs.len(), 1);
    assert!(
        unconfirmed.notes[0].contains("could not be read back natively"),
        "{:?}",
        unconfirmed.notes
    );
}

#[test]
fn without_the_bridge_a_fight_summarizes_hits_and_the_kill_from_native_reads() {
    let mut game = plain_fight();
    let result = run_sequence(
        &mut game,
        json!({"steps": [
            {"action": "spawn", "as": "A", "args": {"type": "zombie", "ahead": 3}},
            {"action": "attack", "args": {"entity_id": "$A"},
             "repeat": {"every_ticks": 2, "until": {"event": {"name": "entity_die", "contains": "$A"}}}}
        ]}),
    )
    .unwrap();
    assert_eq!(result["clock"], "wall");
    let spawn = &result["steps"][0]["outcome"];
    assert_eq!(spawn["evidence"]["id"], "-5", "{result}");
    let fight = &result["steps"][1];
    assert_eq!(fight["target"], "-5", "{result}");
    assert_eq!(fight["ended"], "until", "{result}");
    assert_eq!(fight["until"]["event"]["source"], "native", "{result}");
    let runs = fight["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 3, "{result}");
    for run in runs {
        assert_eq!(run["outcome"]["source"], "native", "{result}");
        assert_eq!(run["outcome"]["confirmed"], true, "{result}");
    }
    assert_eq!(runs[2]["outcome"]["evidence"]["killed"], true, "{result}");
    let target = &result["summary"]["targets"]["-5"];
    assert_eq!(target["label"], "A", "{result}");
    assert_eq!(target["hits"], 3, "{result}");
    assert_eq!(target["misses"], 0, "{result}");
    assert_eq!(target["after_death"], 0, "{result}");
    assert_eq!(target["killed_by"]["step"], 1, "{result}");
    assert_eq!(target["killed_by"]["run"], 2, "{result}");
    assert_eq!(target["killed_by"]["source"], "native", "{result}");
    assert_eq!(target["final"]["state"], "dead", "{result}");
    assert_eq!(result["summary"]["totals"]["kills"], 1);
    // The tracker reported each drop as a native entity_hurt.
    let hurts = game
        .hub
        .read(
            &game.world,
            &EventCursor::default(),
            &Filter {
                names: vec!["entity_hurt".into()],
                ..Filter::default()
            },
            10,
            Duration::ZERO,
            &|_| true,
        )
        .unwrap();
    assert_eq!(hurts.events.len(), 3, "{:?}", hurts.events);
    assert_eq!(hurts.events[0].body["damage"], 8.0);
}

#[test]
fn without_the_bridge_spawn_finds_the_new_id_with_querytarget() {
    let (mut game, seen) = fake(false, plain_arena());
    game.script.lock().unwrap().native = Some(HashMap::new());
    let outcome = perform(
        &mut game,
        &spawn_request(json!({
            "type": "zombie", "ahead": 3,
            "effects": [{"id": "slowness", "seconds": 60, "amplifier": 255, "hide": true}]
        })),
        SHORT,
    )
    .unwrap();
    assert!(outcome.confirmed, "{:?}", outcome.notes);
    assert_eq!(outcome.source, "command");
    assert_eq!(outcome.evidence["id"], "-5");
    assert_eq!(outcome.evidence["matched_by"], "querytarget");
    assert_eq!(outcome.evidence["line_of_sight"]["clear"], Value::Null);
    assert_eq!(outcome.evidence["effects"][0]["applied"], true);
    assert!(
        outcome
            .notes
            .iter()
            .any(|note| note.contains("line_of_sight is unknown")),
        "{:?}",
        outcome.notes
    );
    let seen = seen.lock().unwrap();
    assert!(
        seen.contains(&"summon minecraft:zombie 0 0 3".to_owned()),
        "{seen:?}"
    );
    drop(seen);

    // A zombie that stood there before the /summon is only the nearest one, and says so.
    let again = perform(
        &mut game,
        &spawn_request(json!({"type": "zombie", "ahead": 3})),
        SHORT,
    )
    .unwrap();
    assert_eq!(again.evidence["matched_by"], "nearest");
}

#[test]
fn references_must_name_an_earlier_spawn_step() {
    let (mut game, _) = fake(true, arena());
    let unknown = run_sequence(
        &mut game,
        json!({"steps": [{"action": "attack", "args": {"entity_id": "$B"}}]}),
    )
    .unwrap_err()
    .to_string();
    assert!(
        unknown.contains("$B names no earlier spawn step"),
        "{unknown}"
    );
    let label = run_sequence(&mut game, json!({"steps": [{"action": "stop", "as": "A"}]}))
        .unwrap_err()
        .to_string();
    assert!(label.contains("as names a spawn step"), "{label}");
    assert!(game.inputs.is_empty());
}

#[test]
fn a_blocking_call_waits_for_its_run_or_hands_it_over_at_the_limit() {
    let arguments = json!({"steps": [{"action": "attack", "args": {"entity_id": "-5"}}]});
    let mut game = hitting_steve();
    let mut background = Background::default();
    let (id, _) = queue(&mut game, &mut background, arguments.clone());
    background.claim(arguments.clone(), None, false);
    assert!(matches!(
        background.claimed(&arguments),
        Some(Err(Error::Waiting))
    ));
    // The call's own retries are not refused as a second sequence; another sequence is.
    assert!(background.conflict("action_sequence", &arguments).is_ok());
    assert!(background.conflict("action_sequence", &json!({})).is_err());
    assert!(background.claimed(&json!({"steps": []})).is_none());
    run_queued(&mut game, background.take_queued().unwrap());
    let done = background.claimed(&arguments).unwrap().unwrap();
    assert_eq!(done["run_id"], id.as_str());
    assert_eq!(done["state"], "finished", "{done}");
    assert_eq!(done["summary"]["totals"]["hits"], 1, "{done}");
    assert!(background.claimed(&arguments).is_none());

    // Past its limit the call answers with the run id and the run goes on.
    let (second, _) = queue(&mut game, &mut background, arguments.clone());
    background.claim(arguments.clone(), Some(Duration::ZERO), false);
    let handed = background.claimed(&arguments).unwrap().unwrap();
    assert_eq!(handed["run_id"], second.as_str());
    assert_eq!(handed["state"], "running");
    run_queued(&mut game, background.take_queued().unwrap());
    // sequence_status without run_id reads the latest run.
    let status = background
        .status(serde_json::from_value(json!({})).unwrap())
        .unwrap();
    assert_eq!(status["run_id"], second.as_str());
    assert_eq!(status["state"], "finished");
    assert_eq!(status["summary"]["totals"]["confirmed"], 1, "{status}");
}

#[test]
fn record_wraps_the_steps_in_one_recording() {
    let mut game = hitting_steve();
    let result = run_sequence(
        &mut game,
        json!({"record": {"fps": 30}, "steps": [
            {"action": "attack", "args": {"entity_id": "-5"}},
            {"action": "attack", "args": {"entity_id": "-5"}}
        ]}),
    )
    .unwrap();
    assert_eq!(game.recordings, [("start", 0), ("stop", 2)], "{result}");
    assert_eq!(result["recording"]["path"], "/tmp/fight.mp4");
    assert_eq!(result["confirmed"], 2);
}

#[test]
fn approach_gives_up_early_when_the_gap_stops_shrinking() {
    let (mut game, _) = fake(true, steve("-5"));
    // Forward is held, but the pig never gets closer: something is in the way.
    game.script.lock().unwrap().gap = 5.5;
    let request = Request::parse(
        "attack",
        json!({"entity_id": "-5", "approach": true, "approach_ms": 5000}),
    )
    .unwrap();
    let started = Instant::now();
    let outcome = perform(&mut game, &request, SHORT).unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(!outcome.confirmed);
    assert_eq!(
        outcome.reason.as_ref().map(|reason| reason.0),
        Some("blocked")
    );
    assert_eq!(outcome.evidence["approach"]["blocked"], true);
    assert!(game.inputs.iter().all(|input| input.press.is_empty()));
    assert!(
        outcome.notes.join(" ").contains("stopped closing the gap"),
        "{:?}",
        outcome.notes
    );
}

/// Steve holding `item` (null for an empty hand) with `around` near him.
fn holding(item: Value, around: Value) -> Box<Answer> {
    let steve = steve("-5");
    Box::new(move |line, script| match line {
        "re_mcp:item \"slot=0\"" => Some(json_reply(json!({"slot": 0, "item": item}))),
        "re_mcp:entities 48 * 64" => Some(json_reply(json!({"entities": around}))),
        "re_mcp:inventory" => Some(json_reply(
            json!({"slots": [{"slot": 0, "id": item["id"]}]}),
        )),
        _ => steve(line, script),
    })
}

#[test]
fn an_unconfirmed_use_says_why() {
    let reason = |item: Value, around: Value| {
        let (mut game, _) = fake(true, holding(item, around));
        let request = Request::parse("item_use", json!({})).unwrap();
        let outcome = perform(&mut game, &request, SHORT).unwrap();
        assert!(!outcome.confirmed);
        let reason = outcome.to_json()["reason"].clone();
        (
            reason["class"].as_str().unwrap().to_owned(),
            reason["detail"].to_string(),
        )
    };
    let boomerang = json!({"id": "demo:ruby_boomerang", "cooldown": null});
    assert_eq!(reason(Value::Null, json!([])).0, "no_item");
    let cooling = json!({"id": "demo:ruby_boomerang", "cooldown": {"category": "boomerang", "ticks": 20, "remaining": 12}});
    let (class, detail) = reason(cooling, json!([]));
    assert_eq!(class, "cooldown");
    assert!(detail.contains("12 of 20 ticks"), "{detail}");
    let thrown = json!([
        {"id": "-3", "type": "minecraft:zombie", "distance": 3.0},
        {"id": "-8", "type": "demo:boomerang", "distance": 6.5}
    ]);
    let (class, detail) = reason(boomerang.clone(), thrown);
    assert_eq!(class, "projectile_in_flight");
    assert!(detail.contains("demo:boomerang -8"), "{detail}");
    assert_eq!(reason(boomerang, json!([])).0, "no_event");
}

/// Steve with a sword in slot 0 and an apple in slot 1, whose hotbar keys select the slot.
fn armed_steve() -> Fake {
    let steve = steve("-5");
    let (mut game, _) = fake(
        true,
        Box::new(move |line, script| match line {
            "re_mcp:inventory" => Some(json_reply(json!({"selectedSlot": 0, "slots": [
                {"slot": 0, "id": "minecraft:iron_sword"},
                {"slot": 1, "id": "minecraft:apple"}
            ]}))),
            _ => steve(line, script),
        }),
    );
    game.react = Box::new(|request, script| {
        if let Some(key) = request.hotbar {
            script.push(
                "hotbar_select",
                json!({"player": {"name": "Steve"}, "to": key - 1}),
            );
        }
    });
    game.world_key = Some("world-1".to_owned());
    game
}

#[test]
fn auto_warm_up_runs_once_per_world_load_and_always_before_a_recording() {
    let mut game = armed_steve();
    let look = json!({"steps": [{"action": "look", "args": {"entity_id": "-5"}}]});
    let first = run_sequence(&mut game, look.clone()).unwrap();
    let warmed = &first["warm_up"];
    assert_eq!(warmed["done"], true, "{first}");
    assert_eq!(warmed["swapped_slot"], 1);
    assert_eq!(warmed["attachable"]["animating"], true);
    // Slot 1 and back (slot 0 is already selected, so no key), forward, then back.
    assert_eq!(game.inputs[0].hotbar, Some(2), "{:?}", game.inputs);
    assert!(
        game.inputs
            .iter()
            .any(|input| { input.r#move.as_ref().and_then(|axes| axes.forward) == Some(-1.0) })
    );

    // The same world: no second warm-up.
    let again = run_sequence(&mut game, look.clone()).unwrap();
    assert!(again.get("warm_up").is_none(), "{again}");
    // A new world load warms up again.
    game.world_key = Some("world-2".to_owned());
    let reloaded = run_sequence(&mut game, look.clone()).unwrap();
    assert_eq!(reloaded["warm_up"]["done"], true, "{reloaded}");

    // A recording always warms up first, and starts after the warm-up's input.
    let before = game.inputs.len();
    game.recordings.clear();
    let recorded = run_sequence(
        &mut game,
        json!({"record": true, "steps": [{"action": "look", "args": {"entity_id": "-5"}}]}),
    )
    .unwrap();
    assert_eq!(recorded["warm_up"]["done"], true, "{recorded}");
    let (event, at) = game.recordings[0];
    assert_eq!(event, "start");
    assert!(at > before, "{:?}", game.recordings);

    // warm_up: false skips it even with a recording.
    let skipped = run_sequence(
        &mut game,
        json!({"record": true, "warm_up": false, "steps": [{"action": "look", "args": {"entity_id": "-5"}}]}),
    )
    .unwrap();
    assert!(skipped.get("warm_up").is_none(), "{skipped}");
    // Runs that do not play the connected player do not warm it up on their own.
    game.world_key = Some("world-3".to_owned());
    let simulated = prepare(
        &mut game,
        serde_json::from_value(json!({"steps": [{"action": "stop", "args": {"actor": "Bot"}}]}))
            .unwrap(),
        SHORT,
    )
    .unwrap();
    assert!(!simulated.warm);
}

#[test]
fn an_empty_hand_right_after_a_throw_is_a_projectile_in_flight() {
    // The boomerang leaves the hand when thrown; its projectile's type shares nothing with it.
    let (mut game, _) = fake(
        true,
        holding(
            Value::Null,
            json!([{"id": "-8", "type": "minecraft:arrow"}]),
        ),
    );
    let mut uses = 0;
    game.react = Box::new(move |request, script| {
        if request.press == ["key.use"] {
            uses += 1;
            if uses == 1 {
                let by =
                    json!({"player": {"name": "Steve"}, "item": {"id": "demo:ruby_boomerang"}});
                script.push("item_use", by);
            }
        }
    });
    let result = run_sequence(
        &mut game,
        json!({"steps": [{"action": "item_use"}, {"action": "item_use"}]}),
    )
    .unwrap();
    assert_eq!(result["steps"][0]["outcome"]["confirmed"], true, "{result}");
    let reason = &result["steps"][1]["outcome"]["reason"];
    assert_eq!(reason["class"], "projectile_in_flight", "{result}");
    assert!(reason["detail"].to_string().contains("ruby_boomerang"));
}

#[test]
fn a_repeat_reads_until_right_before_each_run() {
    // The pig dies while the step waits for its second run, with no input in between.
    let (mut game, _) = fake(true, steve("-5"));
    {
        let mut script = game.script.lock().unwrap();
        script.clock = Some(Instant::now());
        script.dies_at = Some(12);
    }
    let result = run_sequence(
        &mut game,
        json!({"warm_up": false, "steps": [{
            "action": "attack", "args": {"entity_id": "-5"},
            "repeat": {"every_ticks": 20, "until": {"event": {"name": "entity_die", "contains": "-5"}}}
        }]}),
    )
    .unwrap();
    let fight = &result["steps"][0];
    assert_eq!(fight["ended"], "until", "{result}");
    assert_eq!(fight["runs"].as_array().unwrap().len(), 1, "{result}");
}

#[test]
fn a_run_whose_target_is_gone_is_skipped_and_ends_the_repeat() {
    let (mut game, _) = fake(true, steve("-5"));
    game.script.lock().unwrap().gone = true;
    let result = run_sequence(
        &mut game,
        json!({"fail_fast": true, "steps": [{
            "action": "attack", "args": {"entity_id": "-5"}, "repeat": {"times": 3}
        }]}),
    )
    .unwrap();
    let fight = &result["steps"][0];
    assert_eq!(fight["ended"], "target_dead", "{result}");
    let runs = fight["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1, "{result}");
    assert_eq!(runs[0]["reason"]["class"], "target_dead");
    assert!(runs[0].get("error").is_none());
    assert_eq!(result["stopped"], Value::Null, "{result}");
    assert_eq!(result["summary"]["targets"]["-5"]["misses"], 0, "{result}");
    assert!(game.inputs.is_empty());
}

/// Steve in the arena with a sword in slot 0 and an apple in slot 1; every attack on -5 hits
/// it and the third kills it.
fn fighting_steve() -> Fake {
    let arena = arena();
    let (mut game, _) = fake(
        true,
        Box::new(move |line, script| match line {
            "re_mcp:inventory" => Some(json_reply(json!({"selectedSlot": 0, "slots": [
                {"slot": 0, "id": "minecraft:iron_sword"},
                {"slot": 1, "id": "minecraft:apple"}
            ]}))),
            _ => arena(line, script),
        }),
    );
    game.script.lock().unwrap().clock = Some(Instant::now());
    let mut hits = 0;
    game.react = Box::new(move |request, script| {
        if let Some(key) = request.hotbar {
            script.push(
                "hotbar_select",
                json!({"player": {"name": "Steve"}, "to": key - 1}),
            );
        }
        if request.press == ["key.attack"] {
            script.push("entity_hit_entity", hit("Steve"));
            hits += 1;
            if hits == 3 {
                script.push(
                    "entity_die",
                    json!({"entity": {"id": "-5", "type": "minecraft:zombie"}, "cause": "entityAttack", "killer": {"name": "Steve"}}),
                );
            }
        }
    });
    game.world_key = Some("world-1".to_owned());
    game
}

/// A typical fight: spawn, select, look, attack until the zombie dies, look away, stop, all
/// recorded.
fn fight(extra: Value) -> Value {
    let mut arguments = json!({"record": true, "steps": [
        {"action": "spawn", "as": "A", "args": {"type": "zombie", "ahead": 3}},
        {"action": "hotbar_select", "args": {"slot": 0}},
        {"action": "look", "args": {"entity_id": "$A"}},
        {"action": "attack", "args": {"entity_id": "$A"},
         "repeat": {"every_ticks": 2, "until": {"event": {"name": "entity_die", "contains": "$A"}}}},
        {"action": "look", "args": {"position": {"x": 0, "y": 1.6, "z": -3}}},
        {"action": "stop"}
    ]});
    arguments
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    arguments
}

/// The reply a blocking call gets for `arguments`, `verbose` or not, on a fresh fight.
fn fight_reply(arguments: &Value, verbose: bool) -> Value {
    let mut game = fighting_steve();
    let mut background = Background::default();
    queue(&mut game, &mut background, arguments.clone());
    background.claim(arguments.clone(), None, verbose);
    run_queued(&mut game, background.take_queued().unwrap());
    background.claimed(arguments).unwrap().unwrap()
}

#[test]
fn a_run_replies_with_what_went_wrong_and_the_whole_log_only_when_verbose() {
    let arguments = fight(json!({}));
    let brief = fight_reply(&arguments, false);
    assert_eq!(brief["state"], "finished", "{brief}");
    for field in ["run_id", "timing", "recording", "notes"] {
        assert!(brief.get(field).is_some(), "{field}: {brief}");
    }
    for field in ["steps", "clock", "confirmed"] {
        assert!(brief.get(field).is_none(), "{field}: {brief}");
    }
    assert_eq!(brief["summary"]["totals"]["kills"], 1, "{brief}");
    assert_eq!(brief["summary"]["targets"]["-5"]["hits"], 3, "{brief}");
    assert!(brief["summary"].get("hits_mean").is_none(), "{brief}");
    let warm = brief["warm_up"].as_object().unwrap();
    assert_eq!(warm["done"], true, "{brief}");
    assert_eq!(warm.len(), 2, "{brief}");
    // Every step went as planned.
    assert_eq!(brief["failed_steps"], json!([]), "{brief}");

    let verbose = fight_reply(&fight(json!({"verbose": true})), true);
    assert_eq!(verbose["steps"].as_array().unwrap().len(), 6, "{verbose}");
    assert_eq!(verbose["steps"][3]["runs"].as_array().unwrap().len(), 3);
    assert_eq!(verbose["warm_up"]["swapped_slot"], 1, "{verbose}");
    assert!(verbose["summary"].get("hits_mean").is_some());
    assert_eq!(verbose["failed_steps"], json!([]));
}

#[test]
fn failed_steps_name_the_steps_that_went_wrong_one_line_each() {
    let steps = json!([
        {"index": 0, "action": "spawn", "outcome": {"confirmed": true}},
        {"index": 1, "action": "attack", "target": "-5",
         "outcome": {"confirmed": false, "reason": {"class": "out_of_reach", "detail": "far"}, "evidence": {"big": 1}}},
        {"index": 2, "action": "attack", "target": "-5", "runs": [
            {"run": 0, "outcome": {"confirmed": true}},
            {"run": 1, "outcome": {"confirmed": false, "reason": {"class": "cooldown"}}},
            {"run": 2, "error": "No item"},
            {"run": 3, "skipped": "dead", "reason": {"class": "target_dead"}}
        ]},
        {"index": 3, "action": "attack", "target": "-5", "runs": [
            {"run": 0, "outcome": {"confirmed": true}},
            {"run": 1, "skipped": "dead", "reason": {"class": "target_dead"}}
        ]},
        {"index": 4, "action": "look", "outcome": {"confirmed": true}, "expect": {"met": false}},
        {"index": 5, "action": "stop", "skipped": "Its if condition did not hold."}
    ]);
    let lines = summary::failed_steps(&steps);
    let indices: Vec<&Value> = lines.iter().map(|line| &line["index"]).collect();
    assert_eq!(
        indices,
        [&json!(1), &json!(2), &json!(4), &json!(5)],
        "{lines:?}"
    );
    assert_eq!(lines[0]["reason"], "out_of_reach");
    assert_eq!(lines[0]["confirmed"], false);
    assert_eq!(lines[0]["target"], "-5");
    assert!(lines[0].get("outcome").is_none());
    assert_eq!(lines[1]["runs"], 3, "{lines:?}");
    assert_eq!(lines[1]["failed"], 2);
    assert_eq!(lines[1]["reasons"], json!({"cooldown": 1, "error": 1}));
    assert_eq!(lines[1]["error"], "No item");
    assert_eq!(lines[2]["reason"], "expect_unmet");
    assert_eq!(lines[2]["confirmed"], true);
    assert_eq!(lines[3]["reason"], "skipped");
    assert!(lines.iter().all(|line| line["ok"] == false));
}

#[test]
fn status_and_stop_follow_the_same_rule() {
    let mut game = hitting_steve();
    let mut background = Background::default();
    let (id, _) = queue(
        &mut game,
        &mut background,
        json!({"steps": [
            {"action": "attack", "args": {"entity_id": "-5"}},
            {"action": "attack", "args": {"entity_id": "-5"}}
        ]}),
    );
    run_queued(&mut game, background.take_queued().unwrap());
    let status = |verbose: bool| {
        background
            .status(serde_json::from_value(json!({"run_id": id, "verbose": verbose})).unwrap())
            .unwrap()
    };
    let brief = status(false);
    assert!(brief.get("steps").is_none(), "{brief}");
    assert_eq!(brief["settled"], 2, "{brief}");
    assert_eq!(brief["cursor"], 2);
    assert_eq!(brief["failed_steps"], json!([]));
    assert_eq!(brief["summary"]["totals"]["hits"], 2, "{brief}");
    let full = status(true);
    assert_eq!(full["steps"].as_array().unwrap().len(), 2, "{full}");
    assert_eq!(full["confirmed"], 2);
    let stop = |verbose: bool| {
        background
            .stop(StopArgs::deserialize(json!({"run_id": id, "verbose": verbose})).unwrap())
            .unwrap()
    };
    assert!(stop(false).get("steps").is_none());
    assert_eq!(stop(true)["steps"].as_array().unwrap().len(), 2);
}

#[test]
fn warm_up_without_the_bridge_reads_natively_and_puts_the_player_back_with_tp() {
    let (mut game, seen) = fake(
        false,
        Box::new(|line, _| line.starts_with("tp @s ").then(|| json!({"statusCode": 0}))),
    );
    game.script.lock().unwrap().player = Some(crate::unstable::fake::native_player());
    // Slot keys change the native slot; walking leaves the player half a block off.
    game.react = Box::new(|request, script| {
        let player = script.player.as_mut().unwrap();
        match request.hotbar {
            Some(key) => player.selected_slot = Some(key - 1),
            None if request.r#move.is_some() => player.entity.position[2] += 0.5,
            None => {}
        }
    });
    let report = warm_up(&mut game);
    assert_eq!(report["source"], "native", "{report}");
    assert_eq!(report["done"], true, "{report}");
    assert_eq!(report["swapped_slot"], 3);
    let hotbar: Vec<u8> = game
        .inputs
        .iter()
        .filter_map(|input| input.hotbar)
        .collect();
    assert_eq!(hotbar, [4, 3], "{:?}", game.inputs);
    let detail = report["detail"].as_str().unwrap();
    assert!(detail.contains("inventory is not read"), "{detail}");
    assert!(detail.contains("with /tp"), "{detail}");
    assert!(
        seen.lock()
            .unwrap()
            .contains(&"tp @s 1.5 64 -2.5 90 10".to_owned()),
        "{:?}",
        seen.lock().unwrap()
    );

    // Neither the pack nor the native read: nothing runs, and the report says why.
    let (mut game, _) = fake(false, Box::new(|_, _| None));
    let report = warm_up(&mut game);
    assert_eq!(report["done"], false);
    let detail = report["detail"].as_str().unwrap();
    assert!(detail.contains("no unstable layer"), "{detail}");
    assert!(game.inputs.is_empty());
}

#[test]
fn fight_targets_parse_as_id_type_or_family() {
    use fight::Wanted;
    assert_eq!(
        Wanted::parse(None).unwrap(),
        Wanted::Family("monster".into())
    );
    assert_eq!(
        Wanted::parse(Some("-4294967295")).unwrap(),
        Wanted::Id("-4294967295".into())
    );
    assert_eq!(
        Wanted::parse(Some("minecraft:zombie")).unwrap(),
        Wanted::Type("minecraft:zombie".into())
    );
    assert_eq!(
        Wanted::parse(Some("undead")).unwrap(),
        Wanted::Family("undead".into())
    );
    assert!(Wanted::parse(Some("zombie,r=99]")).is_err());
    let one_id = Request::parse("fight", json!({"target": "-7", "all": true})).unwrap();
    assert!(perform(&mut fake(true, Box::new(|_, _| None)).0, &one_id, SHORT).is_err());
}

/// Steve at the origin facing south (+z); zombies -1 and -2 straight ahead, 2 and 2.5 blocks
/// away. Unless `tough`, each key.attack kills the nearest zombie still alive, with the Bridge
/// pack's entity_hurt by Steve and entity_die.
fn zombie_pair(tough: bool) -> Fake {
    let alive = Arc::new(Mutex::new(vec![("-1", 2.0), ("-2", 2.5)]));
    // Steve looks at the torso of the nearer zombie, where the fight aims.
    let (_, pitch) = facing([0.0, 1.62, 0.0], [0.0, 1.95 * 0.62, 2.0]);
    let living = Arc::clone(&alive);
    let answer: Box<Answer> = Box::new(move |line, _| {
        let alive = living.lock().unwrap();
        let entity = |id: &str| {
            alive
                .iter()
                .find(|(alive, _)| *alive == id)
                .map(|(_, z)| *z)
        };
        Some(match line {
            "re_mcp:state" => json_reply(json!({
                "name": "Steve", "location": {"x": 0.0, "y": 0.0, "z": 0.0},
                "yaw": 0.0, "pitch": pitch,
            })),
            "querytarget @e[r=24,type=minecraft:zombie]" if !alive.is_empty() => {
                let list: Vec<Value> = alive
                    .iter()
                    .map(|(id, z)| {
                        json!({
                            "uniqueId": id, "id": 1, "dimension": 0, "yRot": 0.0,
                            "position": {"x": 0.0, "y": 0.0, "z": z},
                        })
                    })
                    .collect();
                json!({"statusCode": 0, "details": Value::from(list).to_string()})
            }
            _ if line.starts_with("querytarget @e") => failure("No targets matched selector"),
            _ if line.starts_with("re_mcp:entity ") => {
                let id = line.trim_start_matches("re_mcp:entity ").trim_matches('"');
                match entity(id) {
                    Some(z) => json_reply(json!({
                        "id": id, "type": "minecraft:zombie",
                        "location": {"x": 0.0, "y": 0.0, "z": z},
                        "health": {"current": 20.0, "max": 20.0},
                    })),
                    None => failure(&format!(
                        "re_mcp:entity failed: Error: No loaded entity with id {id}"
                    )),
                }
            }
            _ if line.starts_with("re_mcp:aim ") || line.starts_with("re_mcp:look ") => {
                json_reply(json!({"write": 1}))
            }
            _ => return None,
        })
    });
    let (mut game, _) = fake(true, answer);
    game.react = Box::new(move |request, script| {
        if tough || request.press != ["key.attack"] {
            return;
        }
        let mut alive = alive.lock().unwrap();
        if alive.is_empty() {
            return;
        }
        let (id, _) = alive.remove(0);
        let entity = json!({"id": id, "type": "minecraft:zombie"});
        script.push(
            "entity_hurt",
            json!({"entity": entity, "attacker": {"name": "Steve"}, "damage": 20}),
        );
        script.push(
            "entity_die",
            json!({"entity": entity, "cause": "entity_attack"}),
        );
    });
    game
}

fn attacks(game: &Fake) -> usize {
    game.inputs
        .iter()
        .filter(|input| input.press == ["key.attack"])
        .count()
}

#[test]
fn a_fight_takes_the_nearest_mob_first_and_moves_on_until_none_is_left() {
    let mut game = zombie_pair(false);
    let request =
        Request::parse("fight", json!({"target": "minecraft:zombie", "all": true})).unwrap();
    let outcome = perform(&mut game, &request, SHORT).unwrap();
    let evidence = &outcome.evidence;
    assert!(outcome.confirmed, "{evidence} {:?}", outcome.notes);
    assert_eq!(outcome.source, "bridge");
    assert_eq!(evidence["ended"], "no_targets_left");
    assert_eq!(evidence["kills"], 2);
    let targets = evidence["targets"].as_array().unwrap();
    assert_eq!(targets[0]["id"], "-1", "{evidence}");
    assert_eq!(targets[1]["id"], "-2", "{evidence}");
    for target in targets {
        assert_eq!(target["hits"], 1, "{target}");
        assert_eq!(target["misses"], 0, "{target}");
        assert_eq!(target["kill"]["source"], "bridge", "{target}");
        assert_eq!(target["kill"]["confirmed"], true, "{target}");
        assert!(target["time_to_kill_ms"].is_u64(), "{target}");
    }
    // The second zombie stood 2.5 blocks away: the swing came with a sprint run-up.
    assert_eq!(targets[1]["sprint_swings"], 1, "{evidence}");
    assert_eq!(attacks(&game), 2);
    // Every key the fight held is let go at the end.
    let last = game.inputs.last().unwrap().r#move.unwrap();
    assert_eq!((last.forward, last.strafe), (Some(0.0), Some(0.0)));
}

#[test]
fn a_fight_ends_at_max_targets_the_timeout_or_without_mobs() {
    let mut game = zombie_pair(false);
    let one = Request::parse("fight", json!({"target": "minecraft:zombie"})).unwrap();
    let outcome = perform(&mut game, &one, SHORT).unwrap();
    assert!(outcome.confirmed);
    assert_eq!(outcome.evidence["ended"], "max_targets");
    assert_eq!(outcome.evidence["targets"].as_array().unwrap().len(), 1);

    let mut game = zombie_pair(true);
    let short = Request::parse(
        "fight",
        json!({"target": "minecraft:zombie", "all": true, "timeout_ms": 400}),
    )
    .unwrap();
    let outcome = perform(&mut game, &short, SHORT).unwrap();
    assert!(!outcome.confirmed);
    assert_eq!(outcome.evidence["ended"], "timeout");
    assert_eq!(outcome.to_json()["reason"]["class"], "timeout");
    let first = &outcome.evidence["targets"][0];
    assert_eq!(first["left"]["class"], "timeout", "{first}");
    assert_eq!(first["killed"], false);
    assert!(attacks(&game) >= 1);

    let mut game = zombie_pair(false);
    let none = Request::parse("fight", json!({"target": "minecraft:skeleton"})).unwrap();
    let outcome = perform(&mut game, &none, SHORT).unwrap();
    assert!(!outcome.confirmed);
    assert_eq!(outcome.to_json()["reason"]["class"], "no_target");
    assert_eq!(attacks(&game), 0);
}

/// A game without the Bridge pack whose unstable layer reads Steve at the origin facing
/// zombie -5 two blocks south; each key.attack takes 8 of its 20 health.
fn native_zombie() -> Fake {
    native_zombie_parts().0
}

/// [`native_zombie`] with the commands it saw.
fn native_zombie_parts() -> (Fake, Seen) {
    let (mut game, seen) = fake(
        false,
        Box::new(|line, _| line.starts_with("tp @s ").then(|| json!({"statusCode": 0}))),
    );
    let mut player = crate::unstable::fake::native_player();
    let (_, pitch) = facing([0.0, 1.62, 0.0], [0.0, 1.95 * 0.62, 2.0]);
    player.entity.position = [0.0, 0.0, 0.0];
    player.entity.rotation = Some([pitch as f32, 0.0]);
    {
        let mut script = game.script.lock().unwrap();
        script.player = Some(player);
        script.native = Some(HashMap::from([("-5".to_owned(), [20.0, 20.0])]));
    }
    game.react = Box::new(|request, script| {
        if request.press == ["key.attack"]
            && let Some(health) = script.native.as_mut().and_then(|all| all.get_mut("-5"))
        {
            health[0] = (health[0] - 8.0).max(0.0);
        }
    });
    (game, seen)
}

#[test]
fn without_the_bridge_a_fight_counts_hits_and_the_kill_from_native_reads() {
    let mut game = native_zombie();
    let request =
        Request::parse("fight", json!({"target": "minecraft:zombie", "all": true})).unwrap();
    let outcome = perform(&mut game, &request, SHORT).unwrap();
    let evidence = &outcome.evidence;
    assert!(outcome.confirmed, "{evidence} {:?}", outcome.notes);
    assert_eq!(outcome.source, "native");
    let target = &evidence["targets"][0];
    assert_eq!(target["swings"], 3, "{target}");
    assert_eq!(target["hits"], 3, "{target}");
    assert_eq!(target["kill"]["seen"], "health_zero", "{target}");
    assert_eq!(evidence["ended"], "no_targets_left");
}

#[test]
fn a_fight_step_is_confirmed_and_lists_each_mob_it_fought_in_the_summary() {
    let mut game = native_zombie();
    let reply = run_sequence(
        &mut game,
        json!({
            "warm_up": false,
            "steps": [{"action": "fight", "args": {"target": "minecraft:zombie", "all": true}}],
        }),
    )
    .unwrap();
    let summary = &reply["summary"];
    assert_eq!(summary["totals"]["confirmed"], 1, "{reply}");
    assert_eq!(summary["totals"]["kills"], 1, "{reply}");
    assert_eq!(summary["totals"]["hits"], 3, "{reply}");
    let target = &summary["targets"]["-5"];
    assert_eq!(target["hits"], 3, "{target}");
    assert_eq!(target["misses"], 0, "{target}");
    assert_eq!(target["steps"], json!([0]), "{target}");
    assert_eq!(target["killed_by"]["step"], 0, "{target}");
    assert_eq!(target["killed_by"]["source"], "native", "{target}");
    assert_eq!(target["final"]["state"], "dead", "{target}");
    assert!(
        reply["failed_steps"].as_array().is_none_or(Vec::is_empty),
        "{reply}"
    );
}

#[test]
fn an_exact_aim_is_one_mouse_flick_and_a_short_correction_without_a_look_command() {
    use view::{Camera, Exact};
    let (mut game, seen) = native_zombie_parts();
    let mut camera = Camera::measured(7.0);
    let sent = camera
        .exact_turn(&mut game, (0.0, 0.0), (30.0, -10.0))
        .unwrap();
    assert_eq!(sent, Exact::Sent);
    let flick = game.inputs.last().unwrap().mouse.unwrap();
    assert_eq!((flick.x, flick.y), (210.0, -70.0));
    // Right after the flick a read from before it shows the old view: the flick still counts,
    // so nothing more goes out for it.
    camera
        .exact_turn(&mut game, (0.0, 0.0), (30.0, -10.0))
        .unwrap();
    let early = game.inputs.last().unwrap().mouse.unwrap();
    assert!(early.x.abs() < 1e-9 && early.y.abs() < 1e-9, "{early:?}");
    // Once reads show it, the game turned only 80% (mouse acceleration, say): the correction
    // sends the rest from where the read shows the view.
    thread::sleep(Duration::from_millis(200));
    camera
        .exact_turn(&mut game, (24.0, -8.0), (30.0, -10.0))
        .unwrap();
    let correction = game.inputs.last().unwrap().mouse.unwrap();
    assert!((correction.x - 42.0).abs() < 1e-9 && (correction.y + 14.0).abs() < 1e-9);
    assert!(
        !seen
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.starts_with("tp "))
    );
}

#[test]
fn a_fight_aims_by_mouse_and_never_teleports_when_the_mouse_turns_the_view() {
    let (mut game, seen) = native_zombie_parts();
    // Steve starts facing 70 degrees away from the zombie; the mouse turns him 1 degree per
    // 7 counts, the way the probe measures it.
    game.script
        .lock()
        .unwrap()
        .player
        .as_mut()
        .unwrap()
        .entity
        .rotation = Some([0.0, 70.0]);
    game.react = Box::new(|request, script| {
        if let Some(motion) = request.mouse
            && let Some(player) = script.player.as_mut()
        {
            let [pitch, yaw] = player.entity.rotation.unwrap();
            player.entity.rotation = Some([
                (pitch + (motion.y / 7.0) as f32).clamp(-90.0, 90.0),
                yaw + (motion.x / 7.0) as f32,
            ]);
        }
        if request.press == ["key.attack"]
            && let Some(health) = script.native.as_mut().and_then(|all| all.get_mut("-5"))
        {
            health[0] = (health[0] - 8.0).max(0.0);
        }
    });
    let request = Request::parse(
        "fight",
        json!({"target": "minecraft:zombie", "all": true, "aim": "mouse"}),
    )
    .unwrap();
    let outcome = perform(&mut game, &request, SHORT).unwrap();
    assert!(
        outcome.confirmed,
        "{} {:?}",
        outcome.evidence, outcome.notes
    );
    assert_eq!(outcome.evidence["look_aims"], 0, "{}", outcome.evidence);
    assert!(game.inputs.iter().any(|input| input.mouse.is_some()));
    assert!(
        !seen
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.starts_with("tp ")),
        "{:?}",
        seen.lock().unwrap()
    );
}

#[test]
fn without_the_pack_a_fight_aims_with_tp_by_default_and_says_it_shows_in_chat() {
    let (mut game, seen) = native_zombie_parts();
    // Steve faces 70 degrees away: the first due swing turns him with /tp, then clicks.
    game.script
        .lock()
        .unwrap()
        .player
        .as_mut()
        .unwrap()
        .entity
        .rotation = Some([0.0, 70.0]);
    let request =
        Request::parse("fight", json!({"target": "minecraft:zombie", "all": true})).unwrap();
    let outcome = perform(&mut game, &request, SHORT).unwrap();
    assert!(outcome.confirmed, "{}", outcome.evidence);
    assert!(outcome.evidence["look_aims"].as_u64().unwrap() >= 1);
    assert_eq!(outcome.evidence["flicks"], 0);
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .any(|line| line.starts_with("tp @s ~ ~ ~ ")),
        "{:?}",
        seen.lock().unwrap()
    );
    assert!(
        outcome.notes.iter().any(|note| note.contains("chat")),
        "{:?}",
        outcome.notes
    );
}
