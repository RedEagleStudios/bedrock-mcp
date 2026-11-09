//! The bedrock-mcp Bridge behavior pack (`mc/bridge/`): `re_mcp:*` custom commands that answer with
//! JSON in the WebSocket `statusMessage`. Detected once per WebSocket connection.

use std::{
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    error::{Error, Result, bail},
    world::{World, number, shortest_turn},
};

/// The shell command that exports the Bridge pack; the one place naming its folder.
macro_rules! export_command {
    () => {
        "cd mc/bridge && bun install && rgl run"
    };
}

pub const INSTALL_HINT: &str = concat!(
    "The bedrock-mcp Bridge behavior pack is not active in this world. Ask the user before adding \
     it: once they agree, install it with packs_install (or export it with `",
    export_command!(),
    "` in the bedrock-mcp repository), add the pack to the world, then run /reload all (it reopens \
     the world, which registers new custom commands; plain /reload only reloads scripts behind \
     existing commands)."
);

/// Why a known bridge rejects one of its commands as unknown.
const STALE_HINT: &str = concat!(
    "The Bridge pack in this world answers re_mcp:ping but not this command, so it is older than \
     this server. Ask the user, then update it with packs_install (or export it again with `",
    export_command!(),
    "`), then run /reload all."
);

/// How long a queued bridge write (look, hotbar, player_set) gets to run.
const WRITE_TIMEOUT: Duration = Duration::from_secs(3);
const WRITE_POLL: Duration = Duration::from_millis(20);

/// Who a player-scoped tool acts on: the player who ran /connect, or a player by name (a
/// GameTest simulated player or another player in the world).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub enum Actor {
    #[default]
    Me,
    Named(String),
}

impl TryFrom<String> for Actor {
    type Error = String;

    fn try_from(name: String) -> std::result::Result<Self, String> {
        let name = name.trim();
        if name.is_empty() || name == "self" {
            return Ok(Self::Me);
        }
        if name.contains(['"', '\\', '\n', '\r']) {
            return Err(format!(
                "Player name {name:?} cannot contain quotes, backslashes or line breaks"
            ));
        }
        Ok(Self::Named(name.to_owned()))
    }
}

impl Actor {
    /// The trailing ` "name"` argument of a bridge command; empty for the caller.
    pub fn argument(&self) -> String {
        match self {
            Self::Me => String::new(),
            Self::Named(name) => format!(" \"{name}\""),
        }
    }

    /// The target selector for vanilla commands.
    pub fn selector(&self) -> String {
        match self {
            Self::Me => "@s".to_owned(),
            Self::Named(name) => format!("@a[name=\"{name}\"]"),
        }
    }
}

/// A quoted String argument for a bridge command.
pub fn quoted(text: &str) -> Result<String> {
    if text.contains(['"', '\\', '\n', '\r']) {
        bail!("{text:?} cannot contain quotes, backslashes or line breaks.");
    }
    Ok(format!("\"{text}\""))
}

/// A read result: `fields` with the contract's `source` and `notes`.
pub fn labelled(source: &str, notes: Vec<String>, fields: Value) -> Value {
    let mut result = match fields {
        Value::Object(map) => Value::Object(map),
        other => json!({ "result": other }),
    };
    result["source"] = source.into();
    result["notes"] = json!(notes);
    result
}

/// What a `re_mcp:ping` reply says about the bridge.
#[derive(Debug, PartialEq)]
pub enum Detection {
    Present,
    /// The command does not exist: the pack is missing.
    Absent,
    /// Another failure; try again later.
    Unknown(String),
}

fn unknown_command(message: &str) -> bool {
    ["Unexpected", "Syntax error", "Unknown command"]
        .iter()
        .any(|marker| message.contains(marker))
}

pub fn classify_ping(body: &Value) -> Detection {
    let message = body["statusMessage"].as_str().unwrap_or_default();
    if body["statusCode"].as_i64().unwrap_or(0) == 0 {
        return match serde_json::from_str::<Value>(message) {
            Ok(reply) if reply.get("version").is_some() => Detection::Present,
            _ => Detection::Unknown(format!("unexpected re_mcp:ping reply {message:?}")),
        };
    }
    if unknown_command(message) {
        Detection::Absent
    } else {
        Detection::Unknown(message.to_owned())
    }
}

/// The quoted name after `prefix` in `text`, quotes included.
fn quoted_after<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = text.strip_prefix(prefix)?;
    let end = rest.get(1..)?.find('"')? + 2;
    rest.starts_with('"').then(|| &rest[..end])
}

/// A failed pack command as a sentence for the agent: the game's reason without the internal
/// command line, with the next step for the failures agents run into. `command` is the
/// command line; only its first word (the command name) is kept, for failures without a
/// known shape.
pub fn friendly(command: &str, message: &str) -> String {
    let name = command.split(' ').next().unwrap_or(command);
    let mut text = message.trim();
    if let Some((head, rest)) = text.split_once(" failed: ")
        && !head.contains(' ')
    {
        text = rest;
    }
    while let Some(rest) = text.strip_prefix("Error: ") {
        text = rest;
    }
    let text = text.trim_end_matches('.');
    if let Some(player) = quoted_after(text, "No simulated player named ") {
        return format!(
            "No simulated player named {player} is in the world; spawn one with gametest_spawn \
             (gametest_list shows the ones there)."
        );
    }
    if let Some(player) = quoted_after(text, "No player named ") {
        return format!(
            "No player named {player} is in the world. For a simulated player, spawn one with \
             gametest_spawn; entities_list with type minecraft:player shows the players there."
        );
    }
    if let Some(player) = quoted_after(text, "Simulated player ")
        && text.ends_with(" is dead")
    {
        return format!(
            "Simulated player {player} is dead (health 0), so it cannot act. Remove it with \
             gametest_remove and spawn it again with gametest_spawn."
        );
    }
    if text.starts_with("No loaded entity with id ") || text.starts_with("No entity with id ") {
        return format!(
            "{text}: it died, despawned, or is in an unloaded chunk. entities_list shows the \
             current ids."
        );
    }
    if text.ends_with("already exists") {
        return format!("{text}; pick another name, or remove it with gametest_remove first.");
    }
    format!("{name} failed: {text}.")
}

/// The JSON a bridge command answered with, or its failure as an error.
pub fn parse_reply(command: &str, body: &Value) -> Result<Value> {
    let message = body["statusMessage"].as_str().unwrap_or_default();
    if body["statusCode"].as_i64().unwrap_or(0) != 0 {
        if unknown_command(message) {
            bail!("/{command}: {message}. {STALE_HINT}");
        }
        bail!("{}", friendly(command, message));
    }
    serde_json::from_str(message).map_err(|error| {
        Error::msg(format!(
            "/{command} answered with invalid JSON ({error}): {message}"
        ))
    })
}

/// One `re_mcp:look` per frame from `start`, turning by `delta` (yaw, pitch) in total.
pub fn look_steps(actor: &Actor, start: (f64, f64), delta: (f64, f64), frames: u32) -> Vec<String> {
    let steps = frames.max(1);
    let player = actor.argument();
    (1..=steps)
        .map(|step| {
            let fraction = f64::from(step) / f64::from(steps);
            let yaw = start.0 + delta.0 * fraction;
            let pitch = start.1 + delta.1 * fraction;
            format!("re_mcp:look {} {}{player}", number(yaw), number(pitch))
        })
        .collect()
}

impl World {
    /// Whether the bridge answers on this connection. A cached absence is checked again
    /// when `recheck` is set, since the pack can be added and the world reloaded meanwhile.
    pub fn bridge(&self, recheck: bool) -> Result<bool> {
        let connection = self.socket().connections();
        if let Some(present) = self.cached_bridge(connection)
            && (present || !recheck)
        {
            return Ok(present);
        }
        match classify_ping(&self.raw("re_mcp:ping")?) {
            Detection::Present => {
                self.cache_bridge(connection, true);
                Ok(true)
            }
            Detection::Absent => {
                self.cache_bridge(connection, false);
                Ok(false)
            }
            Detection::Unknown(_) => Ok(false),
        }
    }

    /// Fail with the install hint unless the bridge answers.
    pub fn require_bridge(&self) -> Result<()> {
        if !self.bridge(true)? {
            bail!("{INSTALL_HINT}");
        }
        Ok(())
    }

    /// Run a bridge command. A command the game does not know, while the cache says the
    /// bridge answers, may mean the world changed under the same connection: ask again, and
    /// give the install hint when the pack is gone.
    pub fn bridge_command(&self, command: &str) -> Result<Value> {
        let body = self.raw(command)?;
        let rejected = body["statusCode"].as_i64().unwrap_or(0) != 0
            && unknown_command(body["statusMessage"].as_str().unwrap_or_default());
        if rejected {
            self.forget_bridge();
            if !self.bridge(true)? {
                bail!("{INSTALL_HINT}");
            }
        }
        parse_reply(command, &body)
    }

    /// Run a writing bridge command and wait until its queued job ran on a game tick; the
    /// job's outcome, or its error.
    pub fn bridge_write(&self, command: &str) -> Result<Value> {
        let queued = self.bridge_command(command)?;
        let Some(id) = queued["write"].as_u64() else {
            bail!("/{command} did not queue a write: {queued}");
        };
        let deadline = Instant::now() + WRITE_TIMEOUT;
        loop {
            let mut outcome = self.bridge_command(&format!("re_mcp:result {id}"))?;
            if outcome["pending"] != true {
                if let Some(error) = outcome["error"].as_str() {
                    bail!("{}", friendly(command, error));
                }
                if let Some(fields) = outcome.as_object_mut() {
                    fields.remove("pending");
                }
                return Ok(outcome);
            }
            if Instant::now() >= deadline {
                bail!(
                    "/{command} was queued but did not run within {} s; the world may be paused.",
                    WRITE_TIMEOUT.as_secs()
                );
            }
            thread::sleep(WRITE_POLL);
        }
    }

    /// Turn a player exactly with `re_mcp:look`, one step per frame; returns once the last step
    /// ran on a game tick.
    pub fn bridge_look(
        &self,
        actor: &Actor,
        yaw: f64,
        pitch: f64,
        relative: bool,
        frames: u32,
        wait_frame: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        let steps = if !relative && frames <= 1 {
            look_steps(actor, (yaw, pitch), (0.0, 0.0), 1)
        } else {
            let command = format!("re_mcp:state{}", actor.argument());
            let state = self.bridge_command(&command)?;
            let angle = |key: &str| {
                state[key]
                    .as_f64()
                    .ok_or_else(|| Error::msg(format!("{command} has no {key}")))
            };
            let start = (angle("yaw")?, angle("pitch")?);
            let delta = if relative {
                (yaw, pitch)
            } else {
                (shortest_turn(start.0, yaw), pitch - start.1)
            };
            look_steps(actor, start, delta, frames)
        };
        let Some((last, before)) = steps.split_last() else {
            return Ok(());
        };
        for step in before {
            self.bridge_command(step)?;
            wait_frame()?;
        }
        self.bridge_write(last)?;
        Ok(())
    }

    /// Player state, world info and nearby entities from the bridge.
    pub fn bridge_snapshot(&self) -> Result<Value> {
        Ok(json!({
            "bridge": true,
            "player": self.bridge_command("re_mcp:state")?,
            "world": self.bridge_command("re_mcp:world")?,
            "nearby": self.bridge_command("re_mcp:entities")?["entities"],
        }))
    }
}

/// A game on a real WebSocket for tests.
#[cfg(test)]
pub mod fake {
    use std::sync::{Arc, Mutex};

    use serde_json::{Value, json};
    use tungstenite::Message;

    use crate::world::World;

    pub type Seen = Arc<Mutex<Vec<String>>>;

    /// A game that answers every command with `answer(commandLine)` (a response body) and
    /// records the command lines it saw.
    pub fn game(answer: impl Fn(&str) -> Value + Send + 'static) -> (World, Seen) {
        let socket = Arc::new(crate::ws::GameSocket::bind().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (mut client, _) =
            tungstenite::connect(format!("ws://127.0.0.1:{}", socket.port())).unwrap();
        assert!(socket.wait_connected(std::time::Duration::from_secs(5)));
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            while let Ok(message) = client.read() {
                let Message::Text(text) = message else {
                    continue;
                };
                let request: Value = serde_json::from_str(text.as_str()).unwrap();
                if request["header"]["messagePurpose"] != "commandRequest" {
                    continue;
                }
                let line = request["body"]["commandLine"].as_str().unwrap().to_owned();
                let body = answer(&line);
                log.lock().unwrap().push(line);
                let reply = json!({
                    "header": {"messagePurpose": "commandResponse", "requestId": request["header"]["requestId"]},
                    "body": body,
                });
                if client.send(Message::text(reply.to_string())).is_err() {
                    return;
                }
            }
        });
        (World::new(socket), seen)
    }

    /// A successful bridge reply carrying `value` as JSON.
    pub fn json_reply(value: Value) -> Value {
        json!({"statusCode": 0, "statusMessage": value.to_string()})
    }

    pub fn failure(message: &str) -> Value {
        json!({"statusCode": 1, "statusMessage": message})
    }

    /// How the game answers a command it does not know.
    pub fn unknown(line: &str) -> Value {
        json!({"statusCode": -2147483648, "statusMessage": format!("Syntax error: Unexpected \"{line}\": at \">>{line}<<\"")})
    }

    /// The answers of a bridge whose writes ran: ping, and `re_mcp:result` done at once.
    pub fn bridge_basics(line: &str) -> Option<Value> {
        match line {
            "re_mcp:ping" => Some(json_reply(json!({"version": 2, "tick": 5}))),
            _ if line.starts_with("re_mcp:result ") => Some(json_reply(
                json!({"pending": false, "yaw": 0.0, "pitch": 0.0}),
            )),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{fake::*, *};

    #[test]
    fn pings_tell_present_absent_and_unknown_apart() {
        let present = json!({"statusCode": 0, "statusMessage": "{\"version\":1,\"tick\":42}"});
        assert_eq!(classify_ping(&present), Detection::Present);
        let missing = json!({"statusCode": -2147483648, "statusMessage": "Syntax error: Unexpected \"re_mcp:ping\": at \">>re_mcp:ping<<\""});
        assert_eq!(classify_ping(&missing), Detection::Absent);
        let odd = json!({"statusCode": 0, "statusMessage": "pong"});
        assert!(matches!(classify_ping(&odd), Detection::Unknown(_)));
        let busy = json!({"statusCode": 1, "statusMessage": "Command timed out"});
        assert!(matches!(classify_ping(&busy), Detection::Unknown(_)));
    }

    #[test]
    fn replies_parse_as_json_or_fail_with_the_game_message() {
        let body = json!({"statusCode": 0, "statusMessage": "{\"health\":{\"current\":20}}"});
        assert_eq!(
            parse_reply("re_mcp:state", &body).unwrap()["health"]["current"],
            20
        );
        let failed = json!({"statusCode": 1, "statusMessage": "re_mcp:block failed: unloaded"});
        assert!(
            parse_reply("re_mcp:block 0 0 0", &failed)
                .unwrap_err()
                .to_string()
                .contains("unloaded")
        );
        let garbage = json!({"statusCode": 0, "statusMessage": "not json"});
        assert!(parse_reply("re_mcp:state", &garbage).is_err());
        let stale = parse_reply("re_mcp:raycast \"dx=1\"", &unknown("re_mcp:raycast"))
            .unwrap_err()
            .to_string();
        assert!(stale.contains("older than this server"), "{stale}");
    }

    #[test]
    fn failures_read_without_the_command_line() {
        let state = parse_reply(
            "re_mcp:state \"Nobody\"",
            &failure("re_mcp:state failed: Error: No player named \"Nobody\""),
        )
        .unwrap_err()
        .to_string();
        assert!(
            state.starts_with("No player named \"Nobody\" is in the world")
                && state.contains("gametest_spawn"),
            "{state}"
        );
        let entity = friendly(
            "re_mcp:gametest_act \"Bot\" \"eyJhIjoxfQ\"",
            "Error: No entity with id \"-9\"",
        );
        assert!(
            entity.starts_with("No entity with id \"-9\": it died") && !entity.contains("eyJ"),
            "{entity}"
        );
        assert_eq!(
            friendly("re_mcp:block 0 0 0", "re_mcp:block failed: unloaded"),
            "re_mcp:block failed: unloaded."
        );
    }

    #[test]
    fn actors_parse_from_names_and_reject_quotes() {
        let actor = |name: &str| Actor::deserialize(json!(name));
        assert_eq!(actor("self").unwrap(), Actor::Me);
        assert_eq!(actor("").unwrap(), Actor::Me);
        let bot = actor("Bot 1").unwrap();
        assert_eq!(bot.argument(), " \"Bot 1\"");
        assert_eq!(bot.selector(), "@a[name=\"Bot 1\"]");
        assert!(actor("a\"b").is_err());
    }

    #[test]
    fn bridge_looks_interpolate_absolute_angles_per_frame() {
        let me = Actor::Me;
        assert_eq!(
            look_steps(&me, (10.0, 5.0), (0.0, 0.0), 0),
            ["re_mcp:look 10 5"]
        );
        assert_eq!(
            look_steps(&me, (170.0, 0.0), (shortest_turn(170.0, -170.0), 30.0), 2),
            ["re_mcp:look 180 15", "re_mcp:look 190 30"]
        );
        assert_eq!(
            look_steps(&Actor::Named("Bot".into()), (0.0, 0.0), (-90.0, 10.0), 3)[2],
            "re_mcp:look -90 10 \"Bot\""
        );
    }

    fn with_bridge(line: &str) -> Value {
        if let Some(body) = bridge_basics(line) {
            return body;
        }
        let reply = match line {
            "re_mcp:state" => {
                json!({"yaw": 170.0, "pitch": 0.0, "health": {"current": 20, "max": 20}})
            }
            "re_mcp:world" => json!({"timeOfDay": 6000}),
            "re_mcp:entities" => json!({"snapshot": 1, "total": 0, "entities": [], "next": null}),
            _ if line.starts_with("re_mcp:look ") => json!({"write": 3}),
            _ => return failure(&format!("unexpected {line}")),
        };
        json_reply(reply)
    }

    #[test]
    fn the_bridge_is_detected_once_and_drives_look_and_state() {
        let (world, seen) = game(with_bridge);
        let mut frames = 0;
        world
            .look(-170.0, 30.0, false, 2, &mut || {
                frames += 1;
                Ok(())
            })
            .unwrap();
        world.look(5.0, 6.0, false, 0, &mut || Ok(())).unwrap();
        let snapshot = world.snapshot().unwrap();
        assert_eq!(snapshot["player"]["health"]["current"], 20);
        assert_eq!(snapshot["nearby"], json!([]));
        assert_eq!(frames, 1);
        assert_eq!(
            *seen.lock().unwrap(),
            [
                "re_mcp:ping",
                "re_mcp:state",
                "re_mcp:look 180 15",
                "re_mcp:look 190 30",
                "re_mcp:result 3",
                "re_mcp:look 5 6",
                "re_mcp:result 3",
                "re_mcp:state",
                "re_mcp:world",
                "re_mcp:entities",
            ]
        );
    }

    #[test]
    fn writes_wait_for_their_job_and_report_its_error() {
        let (world, _) = game(|line| match line {
            "re_mcp:hotbar 9" => json_reply(json!({"write": 4})),
            "re_mcp:result 4" => {
                json_reply(json!({"pending": false, "error": "Error: slot out of range"}))
            }
            _ => failure("unexpected"),
        });
        let error = world
            .bridge_write("re_mcp:hotbar 9")
            .unwrap_err()
            .to_string();
        assert!(error.contains("slot out of range"), "{error}");
    }

    #[test]
    fn without_the_bridge_look_uses_tp_and_reads_explain_the_install() {
        let (world, seen) = game(unknown);
        let error = world.require_bridge().unwrap_err().to_string();
        assert!(
            error.contains("rgl run") && error.contains("/reload all"),
            "{error}"
        );
        // The cached absence is reused by look, which falls back to /tp (here rejected).
        assert!(world.look(1.0, 2.0, true, 0, &mut || Ok(())).is_err());
        assert_eq!(*seen.lock().unwrap(), ["re_mcp:ping", "tp @s ~ ~ ~ ~1 ~2"]);
    }

    #[test]
    fn a_world_without_the_pack_on_the_same_connection_is_noticed() {
        // The bridge answered in the previous world; this one rejects every re_mcp command.
        let (world, seen) = game(unknown);
        world.cache_bridge(world.socket().connections(), true);
        let error = world
            .bridge_command("re_mcp:state")
            .unwrap_err()
            .to_string();
        assert!(error.contains("not active in this world"), "{error}");
        assert!(!world.bridge(false).unwrap());
        assert_eq!(*seen.lock().unwrap(), ["re_mcp:state", "re_mcp:ping"]);
    }
}
