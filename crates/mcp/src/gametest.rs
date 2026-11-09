//! The bedrock-mcp GameTest behavior pack (`mc/gametest/`) and the built-in `/gametest`
//! command. The pack's `re_mcp:gametest_*` commands answer with JSON in the WebSocket
//! `statusMessage`, like the Bridge. Custom command callbacks run in restricted execution,
//! where simulated player methods throw, so spawn, remove and act answer with a ticket and
//! the pack does the work on the next tick; `re_mcp:gametest_result <ticket>` then answers
//! with the outcome. The functions here hide that: each returns once the work ran.
//!
//! Simulated players are found by name. Entities are addressed by their script entity id
//! (`Entity.id`, as the Bridge's entity reads report it), blocks by location and an
//! optional face.

use std::{
    thread,
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    bridge::friendly,
    error::{Error, Result, bail},
    world::{World, number},
};

/// How to set the pack up; the only place that names its folder in the repository.
macro_rules! setup {
    () => {
        "Ask the user before setting it up. Then turn on the Beta APIs experiment in the world's \
         settings (the pack uses the beta @minecraft/server-gametest module and does not load \
         without it), install the pack with packs_install (or export it with `cd mc/gametest && \
         bun install && rgl run` in the bedrock-mcp repository), add it to the world, allow \
         cheats, then run /reload all."
    };
}

pub const SETUP: &str = setup!();

pub const INSTALL_HINT: &str = concat!(
    "The bedrock-mcp GameTest behavior pack is not active in this world. ",
    setup!()
);

pub const EXPERIMENT_HINT: &str = "/gametest exists only in worlds with the Beta APIs \
     experiment turned on and cheats allowed.";

/// How long the pack gets to run scheduled work: it runs on the next game tick.
const RESULT_TIMEOUT: Duration = Duration::from_secs(5);
const RESULT_POLL: Duration = Duration::from_millis(25);
/// Chat messages `run` keeps.
const MESSAGE_LIMIT: usize = 200;
const NAME_LIMIT: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Vec3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

/// A block face, for block interactions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Face {
    Down,
    Up,
    North,
    South,
    East,
    West,
}

/// How long a simulated player keeps looking at a target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LookDuration {
    Instant,
    Continuous,
    UntilMove,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GameMode {
    Survival,
    Creative,
    Adventure,
    Spectator,
}

impl GameMode {
    fn name(self) -> &'static str {
        match self {
            Self::Survival => "survival",
            Self::Creative => "creative",
            Self::Adventure => "adventure",
            Self::Spectator => "spectator",
        }
    }
}

/// How far a player reaches an entity, in blocks, by game mode.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reach {
    pub survival: f64,
    pub creative: f64,
}

/// One `SimulatedPlayer` method call (@minecraft/server-gametest 1.0.0-beta). It
/// deserializes from `{"action": "<snake_case name>", ...fields}`; unset options take the
/// method's defaults. Speeds are 0 to 1. `entity` is an entity id; `block` a block location.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    LookAtLocation {
        location: Vec3,
        #[serde(default)]
        duration: Option<LookDuration>,
    },
    LookAtEntity {
        entity: String,
        #[serde(default)]
        duration: Option<LookDuration>,
    },
    LookAtBlock {
        block: Vec3,
        #[serde(default)]
        duration: Option<LookDuration>,
    },
    /// Walk in a straight line.
    MoveToLocation {
        location: Vec3,
        #[serde(default)]
        face_target: Option<bool>,
        #[serde(default)]
        speed: Option<f64>,
    },
    MoveToBlock {
        block: Vec3,
        #[serde(default)]
        face_target: Option<bool>,
        #[serde(default)]
        speed: Option<f64>,
    },
    /// Path-find; the result is `{isFullPath, path}`.
    NavigateToLocation {
        location: Vec3,
        #[serde(default)]
        speed: Option<f64>,
    },
    NavigateToBlock {
        block: Vec3,
        #[serde(default)]
        speed: Option<f64>,
    },
    NavigateToEntity {
        entity: String,
        #[serde(default)]
        speed: Option<f64>,
    },
    /// Walk in a world direction until stopped.
    Move {
        west_east: f64,
        north_south: f64,
        #[serde(default)]
        speed: Option<f64>,
    },
    /// Walk relative to the player's facing until stopped.
    MoveRelative {
        left_right: f64,
        backward_forward: f64,
        #[serde(default)]
        speed: Option<f64>,
    },
    StopMoving,
    Jump,
    /// Swing at whatever the player's view ray hits.
    Attack,
    /// Attack an entity. SimulatedPlayer.attackEntity works at any distance (Microsoft Learn's
    /// SimulatedPlayer reference), so with `reach` the pack measures from the eyes in the same
    /// tick and refuses farther targets.
    AttackEntity {
        entity: String,
        #[serde(default)]
        reach: Option<Reach>,
    },
    /// Use whatever the player's view ray hits, up to the 6 blocks Microsoft Learn's
    /// SimulatedPlayer reference gives for `interact`.
    Interact,
    InteractWithBlock {
        block: Vec3,
        #[serde(default)]
        face: Option<Face>,
    },
    InteractWithEntity {
        entity: String,
        #[serde(default)]
        reach: Option<Reach>,
    },
    UseItemInSlot {
        slot: u8,
    },
    UseItemInSlotOnBlock {
        slot: u8,
        block: Vec3,
        #[serde(default)]
        face: Option<Face>,
        /// Where on the face, relative to the block's bottom north-west corner.
        #[serde(default)]
        face_location: Option<Vec3>,
    },
    /// The result is the item that was in use, or null.
    StopUsingItem,
    /// Keep hitting the block until it breaks or `StopBreakingBlock`.
    BreakBlock {
        block: Vec3,
        #[serde(default)]
        face: Option<Face>,
    },
    StopBreakingBlock,
    /// Drop the whole selected stack.
    DropSelectedItem,
    /// Drop `amount` items from the selected stack (the pack splits the stack first).
    DropAmount {
        amount: u8,
    },
    /// Set `selectedSlotIndex` (0 to 8).
    SelectSlot {
        slot: u8,
    },
    SetItem {
        slot: u8,
        item: String,
        #[serde(default)]
        amount: Option<u8>,
        #[serde(default)]
        select: Option<bool>,
    },
    GiveItem {
        item: String,
        #[serde(default)]
        amount: Option<u8>,
        #[serde(default)]
        select: Option<bool>,
    },
    Chat {
        message: String,
    },
    Respawn,
}

/// A simulated player to spawn. Without `location` it spawns at the player who runs the
/// command (the one that ran /connect), in that player's dimension.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spawn {
    pub name: String,
    #[serde(default)]
    pub location: Option<Vec3>,
    #[serde(default)]
    pub game_mode: Option<GameMode>,
}

/// A `/gametest` run: one test, the tests with a tag, or every registered test (`suite:all`).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Run {
    /// `<class>:<name>` as registered.
    #[serde(default)]
    pub test: Option<String>,
    #[serde(default)]
    pub tag: Option<String>,
    /// Quarter turns clockwise for the test structure.
    #[serde(default)]
    pub rotation: Option<u8>,
    /// Runs of a single test.
    #[serde(default)]
    pub repeat: Option<u32>,
    #[serde(default)]
    pub stop_on_failure: Option<bool>,
    /// For tags: stop the set at the first failure (`runsetuntilfail`).
    #[serde(default)]
    pub until_fail: bool,
    /// How long to collect chat messages after the command.
    #[serde(default)]
    pub wait_ms: u64,
}

fn quoted_name(name: &str) -> Result<String> {
    if name.trim().is_empty() || name.chars().count() > NAME_LIMIT {
        bail!("A simulated player name needs 1 to {NAME_LIMIT} characters.");
    }
    if name.contains(['"', '\\']) || name.chars().any(char::is_control) {
        bail!("A simulated player name cannot contain quotes, backslashes or control characters.");
    }
    Ok(format!("\"{name}\""))
}

fn location(at: Vec3) -> String {
    format!("{} {} {}", number(at.x), number(at.y), number(at.z))
}

fn spawn_command(spawn: &Spawn) -> Result<String> {
    let mut command = format!("re_mcp:gametest_spawn {}", quoted_name(&spawn.name)?);
    if spawn.location.is_some() || spawn.game_mode.is_some() {
        let at = spawn.location.map_or_else(|| "~ ~ ~".to_owned(), location);
        command.push(' ');
        command.push_str(&at);
    }
    if let Some(mode) = spawn.game_mode {
        command.push(' ');
        command.push_str(mode.name());
    }
    Ok(command)
}

fn remove_command(name: &str) -> Result<String> {
    Ok(format!("re_mcp:gametest_remove {}", quoted_name(name)?))
}

/// The action travels as base64url JSON, which needs no escaping inside a quoted argument.
/// With `at` the pack runs it on that game tick instead of the next one.
fn act_command(name: &str, action: &Action, at: Option<u64>) -> Result<String> {
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(action)?);
    let mut command = format!("re_mcp:gametest_act {} \"{payload}\"", quoted_name(name)?);
    if let Some(at) = at {
        command.push_str(&format!(" {at}"));
    }
    Ok(command)
}

fn token(value: &str, what: &str) -> Result<()> {
    if value.is_empty() || value.contains(|c: char| c.is_whitespace() || c == '"') {
        bail!("The {what} {value:?} must be one word without quotes.");
    }
    Ok(())
}

/// The `/gametest` command line for `run`, without the slash. With neither test nor tag it
/// runs `suite:all`: a bare `runset` runs only `suite:default`, which skips tagged tests.
pub fn run_command(run: &Run) -> Result<String> {
    let mut command = match (&run.test, &run.tag) {
        (Some(_), Some(_)) => bail!("Give a test or a tag, not both."),
        (Some(test), None) => {
            token(test, "test name")?;
            if run.until_fail {
                bail!("until_fail applies to tags; use stop_on_failure with repeat for one test.");
            }
            let mut command = format!("gametest run {test}");
            if run.repeat.is_some() || run.stop_on_failure.is_some() {
                let repeat = run.repeat.unwrap_or(1);
                if repeat == 0 {
                    bail!("repeat must be at least 1.");
                }
                let stop = run.stop_on_failure.unwrap_or(false);
                command.push_str(&format!(" {stop} {repeat}"));
            }
            command
        }
        (None, tag) => {
            if run.repeat.is_some() || run.stop_on_failure.is_some() {
                bail!("repeat and stop_on_failure apply to a single test.");
            }
            let verb = if run.until_fail {
                "runsetuntilfail"
            } else {
                "runset"
            };
            let tag = match tag {
                Some(tag) => {
                    token(tag, "tag")?;
                    tag.as_str()
                }
                None => "suite:all",
            };
            format!("gametest {verb} {tag}")
        }
    };
    if let Some(rotation) = run.rotation {
        if rotation > 3 {
            bail!("rotation is 0 to 3 quarter turns.");
        }
        command.push_str(&format!(" {rotation}"));
    }
    Ok(command)
}

/// Whether a failed command's message says the command itself does not exist.
fn unknown_command(message: &str, name: &str) -> bool {
    message.contains("Unknown command")
        || message.contains(&format!("Unexpected \"{name}"))
        || message.contains(&format!(">>{name}"))
}

/// The JSON a pack command answered with. A missing command is the install hint.
fn parse_reply(command: &str, body: &Value) -> Result<Value> {
    let message = body["statusMessage"].as_str().unwrap_or_default();
    if body["statusCode"].as_i64().unwrap_or(0) != 0 {
        if unknown_command(message, "re_mcp") {
            bail!("{INSTALL_HINT}");
        }
        bail!("{}", friendly(command, message));
    }
    serde_json::from_str(message).map_err(|error| {
        Error::msg(format!(
            "/{command} answered with invalid JSON ({error}): {message}"
        ))
    })
}

fn call(world: &World, command: &str) -> Result<Value> {
    parse_reply(command, &world.raw(command)?)
}

/// The failure of scheduled work as a sentence; `what` names the work for the agent.
fn work_failed(what: &str, error: &str) -> String {
    if error.contains("Unknown action") {
        return format!(
            "The bedrock-mcp GameTest pack in this world is older than this server and cannot run \
             {what}. {SETUP}"
        );
    }
    friendly(what, error)
}

/// Scheduled work the pack has not answered yet.
pub struct Ticket {
    id: u64,
    what: String,
    deadline: Instant,
}

/// Run a ticketed command; `what` names the work in errors instead of the command line, and
/// `wait` is how long before the work is due to run.
fn ticket(world: &World, command: &str, what: &str, wait: Duration) -> Result<Ticket> {
    let reply = call(world, command)?;
    let Some(id) = reply["ticket"].as_u64() else {
        bail!("/{command} answered without a ticket: {reply}");
    };
    Ok(Ticket {
        id,
        what: what.to_owned(),
        deadline: Instant::now() + wait + RESULT_TIMEOUT,
    })
}

/// The work's value once the pack ran it; `None` while it has not.
fn poll(world: &World, ticket: &Ticket) -> Result<Option<Value>> {
    let outcome = call(world, &format!("re_mcp:gametest_result {}", ticket.id))?;
    if outcome["done"] == true {
        if outcome["ok"] == true {
            return Ok(Some(outcome["value"].clone()));
        }
        bail!(
            "{}",
            work_failed(&ticket.what, outcome["error"].as_str().unwrap_or("failed"))
        );
    }
    if Instant::now() >= ticket.deadline {
        bail!(
            "{}: the pack did not run the scheduled work within {} s of its time; the game \
             runs it on a tick, so check that the world is not paused.",
            ticket.what,
            RESULT_TIMEOUT.as_secs()
        );
    }
    Ok(None)
}

/// Run a ticketed command and wait for the pack to run its work.
fn scheduled(world: &World, command: &str, what: &str) -> Result<Value> {
    let ticket = ticket(world, command, what, Duration::ZERO)?;
    loop {
        if let Some(value) = poll(world, &ticket)? {
            return Ok(value);
        }
        thread::sleep(RESULT_POLL);
    }
}

/// Spawn a simulated player; returns its state (as `list` describes players) once it settled
/// for a few ticks.
pub fn spawn(world: &World, spawn: &Spawn) -> Result<Value> {
    scheduled(world, &spawn_command(spawn)?, "gametest_spawn").map(with_alive)
}

/// A player as `list` reports it; packs without the `alive` field get one from the health.
fn with_alive(mut player: Value) -> Value {
    if player.get("alive").is_none() && player.is_object() {
        player["alive"] = player["health"]["current"]
            .as_f64()
            .is_none_or(|health| health > 0.0)
            .into();
    }
    player
}

/// Every simulated player in the world: name, id, dimension, location, eye, yaw, pitch,
/// headYaw, headPitch, health {current, max}, alive, gameMode, selectedSlot, selectedItem
/// {id, amount}, sprinting and onGround.
pub fn list(world: &World) -> Result<Vec<Value>> {
    let reply = call(world, "re_mcp:gametest_list")?;
    match reply["players"].as_array() {
        Some(players) => Ok(players.iter().cloned().map(with_alive).collect()),
        None => bail!("/re_mcp:gametest_list answered without players: {reply}"),
    }
}

/// Disconnect a simulated player.
pub fn remove(world: &World, name: &str) -> Result<()> {
    scheduled(world, &remove_command(name)?, "gametest_remove").map(drop)
}

/// The gametest driver for player actions: call one `SimulatedPlayer` method on the
/// simulated player `name` and return `{action, result, player, target, tick}`, where
/// `result` is the method's return value (a boolean, `{isFullPath, path}` for navigation, the
/// stopped item for `StopUsingItem`, null for void methods), `player` its state right after
/// the call, `target` what the pack read of an attacked entity before the attack ({id, valid,
/// health, hurtTicksAgo}), and `tick` the game tick it ran on. An attack or interaction with
/// `reach` the pack refused adds `refused: "reach"`, `distance` and `reach`. Movement
/// continues over later ticks; poll `list` to follow it. Fails with [`INSTALL_HINT`] when the
/// pack is missing, and when the player is dead (except for respawn, chat, inventory and
/// stop actions).
pub fn act(world: &World, name: &str, action: &Action) -> Result<Value> {
    let ticket = send(world, name, action, None)?;
    loop {
        if let Some(reply) = collect(world, &ticket)? {
            return Ok(reply);
        }
        thread::sleep(RESULT_POLL);
    }
}

/// Send an action without waiting for it: the pack runs it on the next tick, or with `at` on
/// game tick `at.0`, which is about `at.1` away. [`collect`] its reply, as [`act`] returns it.
pub fn send(
    world: &World,
    name: &str,
    action: &Action,
    at: Option<(u64, Duration)>,
) -> Result<Ticket> {
    let method = serde_json::to_value(action)?["action"]
        .as_str()
        .unwrap_or("the action")
        .to_owned();
    let command = act_command(name, action, at.map(|(tick, _)| tick))?;
    ticket(
        world,
        &command,
        &method,
        at.map_or(Duration::ZERO, |(_, wait)| wait),
    )
}

/// The reply to a sent action once the pack ran it; `None` while it has not.
pub fn collect(world: &World, ticket: &Ticket) -> Result<Option<Value>> {
    Ok(poll(world, ticket)?.map(|mut reply| {
        reply["player"] = with_alive(reply["player"].take());
        reply
    }))
}

/// Start registered GameTests with `/gametest` and report what the game answered, plus the
/// chat messages (WebSocket PlayerMessage events) that arrived within `run.wait_ms`.
pub fn run(world: &World, run: &Run) -> Result<Value> {
    let command = run_command(run)?;
    let socket = world.socket();
    let mut next = socket.event_count();
    let body = world.raw(&command)?;
    let message = body["statusMessage"].as_str().unwrap_or_default();
    if body["statusCode"].as_i64().unwrap_or(0) != 0 {
        if unknown_command(message, "gametest") {
            bail!("/{command}: {message}. {EXPERIMENT_HINT}");
        }
        bail!("/{command}: {message}");
    }
    let deadline = Instant::now() + Duration::from_millis(run.wait_ms);
    let mut messages = Vec::new();
    while messages.len() < MESSAGE_LIMIT {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Some(event) = socket.wait_event(next, remaining, |event| event.name == "PlayerMessage")
        else {
            break;
        };
        next = event.seq + 1;
        messages.push(json!({
            "type": event.body["type"],
            "sender": event.body["sender"],
            "message": event.body["message"],
        }));
    }
    Ok(json!({
        "command": command,
        "message": message,
        "messages": messages,
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    /// A game on a real WebSocket: every command is answered with `answer(commandLine)`,
    /// after the game sends the chat lines `chat(commandLine)` returns as PlayerMessage events.
    /// Returns the command lines it saw.
    fn fake_game(
        answer: impl Fn(&str) -> Value + Send + 'static,
        chat: impl Fn(&str) -> Vec<&'static str> + Send + 'static,
    ) -> (World, Arc<Mutex<Vec<String>>>) {
        use tungstenite::Message;
        let socket = Arc::new(crate::ws::GameSocket::bind().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (mut client, _) =
            tungstenite::connect(format!("ws://127.0.0.1:{}", socket.port())).unwrap();
        assert!(socket.wait_connected(Duration::from_secs(5)));
        let log = Arc::clone(&seen);
        thread::spawn(move || {
            while let Ok(message) = client.read() {
                let Message::Text(text) = message else {
                    continue;
                };
                let request: Value = serde_json::from_str(text.as_str()).unwrap();
                if request["header"]["messagePurpose"] != "commandRequest" {
                    continue;
                }
                let line = request["body"]["commandLine"].as_str().unwrap().to_owned();
                log.lock().unwrap().push(line.clone());
                let reply = json!({
                    "header": {"messagePurpose": "commandResponse", "requestId": request["header"]["requestId"]},
                    "body": answer(&line),
                });
                let mut frames = vec![reply];
                frames.extend(chat(&line).into_iter().map(|text| {
                    json!({
                        "header": {"messagePurpose": "event", "eventName": "PlayerMessage"},
                        "body": {"type": "chat", "sender": "Script Engine", "message": text},
                    })
                }));
                for frame in frames {
                    if client.send(Message::text(frame.to_string())).is_err() {
                        return;
                    }
                }
            }
        });
        (World::new(socket), seen)
    }

    fn json_reply(value: Value) -> Value {
        json!({"statusCode": 0, "statusMessage": value.to_string()})
    }

    fn missing(line: &str) -> Value {
        let name = line.split(' ').next().unwrap();
        json!({"statusCode": -2147483648, "statusMessage": format!("Syntax error: Unexpected \"{name}\": at \">>{line}<<\"")})
    }

    fn bot(x: f64) -> Value {
        json!({"name": "Bot", "id": "-4294967295", "location": {"x": x, "y": 64.0, "z": 0.0}, "gameMode": "Survival"})
    }

    /// A pack whose spawn is done on the second result poll.
    fn with_pack() -> impl Fn(&str) -> Value + Send + 'static {
        let polls = AtomicUsize::new(0);
        move |line: &str| {
            let reply = match line.split(' ').next().unwrap() {
                "re_mcp:gametest_list" => json!({"players": [bot(0.5)]}),
                "re_mcp:gametest_spawn" => json!({"ticket": 2}),
                "re_mcp:gametest_act" => json!({"ticket": 3}),
                "re_mcp:gametest_remove" => json!({"ticket": 4}),
                "re_mcp:gametest_result" => match line.rsplit(' ').next().unwrap() {
                    "2" if polls.fetch_add(1, Ordering::SeqCst) == 0 => json!({"done": false}),
                    "2" => json!({"done": true, "ok": true, "value": bot(1.5)}),
                    "3" => {
                        json!({"done": true, "ok": true, "value": {"action": "jump", "result": true, "player": bot(2.5)}})
                    }
                    _ => {
                        json!({"done": true, "ok": false, "error": "Error: No simulated player named \"Ghost\""})
                    }
                },
                _ => return json!({"statusCode": 1, "statusMessage": format!("unexpected {line}")}),
            };
            json_reply(reply)
        }
    }

    #[test]
    fn spawn_list_act_and_remove_wait_for_the_scheduled_work() {
        let (world, seen) = fake_game(with_pack(), |_| Vec::new());
        let spawned = spawn(
            &world,
            &Spawn {
                name: "Bot".into(),
                location: Some(Vec3 {
                    x: 1.5,
                    y: 64.0,
                    z: -2.0,
                }),
                game_mode: Some(GameMode::Creative),
            },
        )
        .unwrap();
        assert_eq!(spawned["location"]["x"], 1.5);
        assert_eq!(list(&world).unwrap()[0]["name"], "Bot");
        let acted = act(&world, "Bot", &Action::Jump).unwrap();
        assert_eq!(acted["result"], true);
        assert_eq!(acted["player"]["location"]["x"], 2.5);
        let error = remove(&world, "Ghost").unwrap_err().to_string();
        assert!(error.contains("No simulated player named"), "{error}");
        let jump = URL_SAFE_NO_PAD.encode(r#"{"action":"jump"}"#);
        assert_eq!(
            *seen.lock().unwrap(),
            [
                "re_mcp:gametest_spawn \"Bot\" 1.5 64 -2 creative".to_owned(),
                "re_mcp:gametest_result 2".to_owned(),
                "re_mcp:gametest_result 2".to_owned(),
                "re_mcp:gametest_list".to_owned(),
                format!("re_mcp:gametest_act \"Bot\" \"{jump}\""),
                "re_mcp:gametest_result 3".to_owned(),
                "re_mcp:gametest_remove \"Ghost\"".to_owned(),
                "re_mcp:gametest_result 4".to_owned(),
            ]
        );
    }

    #[test]
    fn a_missing_pack_explains_the_beta_apis_experiment() {
        let (world, seen) = fake_game(missing, |_| Vec::new());
        let error = act(&world, "Bot", &Action::Attack).unwrap_err().to_string();
        assert!(
            error.contains("Beta APIs") && error.contains("rgl run"),
            "{error}"
        );
        assert!(list(&world).unwrap_err().to_string().contains("Beta APIs"));
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[test]
    fn other_command_failures_keep_the_game_message() {
        let (world, _) = fake_game(
            |line| json!({"statusCode": 1, "statusMessage": format!("{line} failed: Error: name taken")}),
            |_| Vec::new(),
        );
        let error = spawn(
            &world,
            &Spawn {
                name: "Bot".into(),
                location: None,
                game_mode: None,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("name taken") && !error.contains("Beta APIs"),
            "{error}"
        );
    }

    #[test]
    fn actions_travel_as_base64url_json_with_snake_case_names() {
        let action: Action = serde_json::from_value(json!({
            "action": "use_item_in_slot_on_block",
            "slot": 2,
            "block": {"x": 1, "y": 2, "z": 3},
            "face": "up",
        }))
        .unwrap();
        let command = act_command("Bot 1", &action, None).unwrap();
        let payload = command
            .strip_prefix("re_mcp:gametest_act \"Bot 1\" \"")
            .and_then(|rest| rest.strip_suffix('"'))
            .unwrap();
        let sent: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap();
        assert_eq!(sent["action"], "use_item_in_slot_on_block");
        assert_eq!(sent["block"], json!({"x": 1.0, "y": 2.0, "z": 3.0}));
        assert_eq!(sent["face"], "up");
        assert!(sent["face_location"].is_null());
        let typo =
            json!({"action": "move_relative", "left_right": 0, "backward_forward": 1, "sped": 1});
        assert!(serde_json::from_value::<Action>(typo).is_err());
        assert!(serde_json::from_value::<Action>(json!({"action": "fly"})).is_err());
        assert!(act_command("Bad\"Name", &Action::Jump, None).is_err());
        assert!(act_command("", &Action::Jump, None).is_err());
        assert!(
            act_command("Bot", &Action::Jump, Some(42))
                .unwrap()
                .ends_with("\" 42")
        );
    }

    #[test]
    fn spawn_fills_the_location_when_only_a_game_mode_is_given() {
        let spawn = |location, game_mode| Spawn {
            name: "Bot".into(),
            location,
            game_mode,
        };
        assert_eq!(
            spawn_command(&spawn(None, None)).unwrap(),
            "re_mcp:gametest_spawn \"Bot\""
        );
        assert_eq!(
            spawn_command(&spawn(None, Some(GameMode::Adventure))).unwrap(),
            "re_mcp:gametest_spawn \"Bot\" ~ ~ ~ adventure"
        );
    }

    #[test]
    fn run_commands_follow_the_gametest_syntax() {
        let run = |value: Value| run_command(&serde_json::from_value(value).unwrap());
        assert_eq!(run(json!({})).unwrap(), "gametest runset suite:all");
        assert_eq!(
            run(json!({"test": "demo:walk", "rotation": 1})).unwrap(),
            "gametest run demo:walk 1"
        );
        assert_eq!(
            run(json!({"test": "demo:walk", "repeat": 3})).unwrap(),
            "gametest run demo:walk false 3"
        );
        assert_eq!(
            run(json!({"tag": "suite", "until_fail": true, "rotation": 2})).unwrap(),
            "gametest runsetuntilfail suite 2"
        );
        assert!(run(json!({"test": "a:b", "tag": "c"})).is_err());
        assert_eq!(
            run(json!({"rotation": 1})).unwrap(),
            "gametest runset suite:all 1"
        );
        assert!(run(json!({"tag": "suite", "repeat": 2})).is_err());
        assert!(run(json!({"test": "a b"})).is_err());
        assert!(run(json!({"test": "a:b", "rotation": 4})).is_err());
    }

    #[test]
    fn runs_report_the_game_answer_and_chat_within_the_wait() {
        let (world, _) = fake_game(
            |line| match line {
                "gametest run demo:walk" => {
                    json!({"statusCode": 0, "statusMessage": "Running 1 tests"})
                }
                _ => missing(line),
            },
            |line| {
                if line.starts_with("gametest run ") {
                    vec!["demo:walk passed"]
                } else {
                    Vec::new()
                }
            },
        );
        let reply = run(
            &world,
            &Run {
                test: Some("demo:walk".into()),
                wait_ms: 300,
                ..Run::default()
            },
        )
        .unwrap();
        assert_eq!(reply["message"], "Running 1 tests");
        assert_eq!(reply["messages"][0]["message"], "demo:walk passed");
        let error = run(
            &world,
            &Run {
                tag: Some("suite".into()),
                ..Run::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("Beta APIs experiment"), "{error}");
    }
}
