//! The `automation` toolset: Bedrock's WebSocket automation session, commands, chat, events
//! and waits.

use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    Handler, Hints, Tool, Toolset, arguments, player, reply,
    schema::{describe, object},
};
use crate::{
    error::{Result, bail},
    events,
    session::Session,
    world::World,
    ws,
};

pub fn tools() -> Vec<Tool> {
    let tool = |hints: Hints, title: &'static str, run: Handler, definition| {
        Tool::new(Toolset::Automation, hints, title, run, definition)
    };
    vec![
        tool(
            Hints::SET,
            "Connect the WebSocket",
            |s, _| reply(connect(s)?),
            json!({
                "name": "automation_connect",
                "description": concat!(
                    "Use this right after world_open, before tools that need commands, events or the Bridge pack.\n\n",
                    "It connects Minecraft's WebSocket automation session to this server now, instead of when a tool first needs it. ",
                    "It types /connect 127.0.0.1:<port> into chat while the player is in a world and waits for the game to connect.\n\n",
                    "- Does nothing when already connected.\n",
                    "- WebSocket events arrive only while connected, so connect before the events you want to observe.\n\n",
                    "Returns {connected, url, connections, bridge}. bridge is whether the bedrock-mcp Bridge pack answers in this world."
                ),
                "inputSchema": object(json!({}), &[])
            }),
        ),
        tool(
            Hints::READ,
            "WebSocket session status",
            |s, _| reply(status(s)),
            json!({
                "name": "automation_status",
                "description": concat!(
                    "Use this when commands or events stop arriving.\n\n",
                    "It reports the WebSocket automation session without connecting it.\n\n",
                    "- events_dropped counts events pushed out of the 512-event history; commands_lost counts commands whose connection closed before the game answered.\n",
                    "- When the server cannot listen it returns {listening: false, error}.\n\n",
                    "Returns {listening, url, connected, connections, subscriptions, observed_events, events_received, events_dropped, commands_lost, commands_in_flight, max_commands_in_flight, bridge}. ",
                    "connections counts connections accepted so far; bridge is whether the Bridge pack answered on this connection, null until checked."
                ),
                "inputSchema": object(json!({}), &[])
            }),
        ),
        tool(
            Hints::DESTROY,
            "Run a command",
            |s, a| reply(execute(s, arguments(a)?)?),
            json!({
                "name": "command_execute",
                "description": concat!(
                    "Use this for a one-off game command, such as /summon or /give.\n\n",
                    "It runs one command as the player over the WebSocket, without chat output. ",
                    "When the WebSocket is not connected it connects first by typing /connect.\n\n",
                    "- The leading / is optional; a line break fails, since it runs one command.\n",
                    "- Most commands need a world that allows cheats.\n",
                    "- `reload all` reloads every pack by reopening the world, which applies edits to behavior and resource packs and picks up newly exported development packs and new custom commands such as the Bridge's re_mcp:*. The WebSocket stops answering until the world is back, and notes say to wait_until in_world.\n",
                    "- Plain `reload` only reloads functions and scripts behind existing commands.\n\n",
                    "Returns {ok, status_code, status_message, body, notes}. body is the game's whole response, which some commands extend (/list adds players)."
                ),
                "inputSchema": object(json!({
                    "command": { "type": "string", "description": "One command line, e.g. \"time set day\" or \"/give @s diamond 3\"" }
                }), &["command"])
            }),
        ),
        tool(
            Hints::ACT,
            "Send chat",
            |s, a| reply(s.chat(arguments(a)?)?),
            json!({
                "name": "chat_send",
                "description": concat!(
                    "Use this to test add-on behaviour that reacts to chat.\n\n",
                    "It types a chat line as the player and confirms it by the game's PlayerMessage event. ",
                    "When the WebSocket is not connected it connects first by typing /connect.\n\n",
                    "- Rejects text starting with /; run commands with command_execute.\n",
                    "- Fails when the PlayerMessage event does not arrive within 5 s.\n\n",
                    "Returns {sent, event}. event is the PlayerMessage body."
                ),
                "inputSchema": object(json!({ "text": { "type": "string", "description": "The chat line to type; must not start with /" } }), &["text"])
            }),
        ),
        tool(
            Hints::READ,
            "List event names",
            |s, _| reply(s.events_list()?),
            json!({
                "name": "events_list",
                "description": concat!(
                    "Use this to find event names before event_subscribe or events_read.\n\n",
                    "It lists the event names events_read can return, per source, with the body fields each carries. ",
                    "ws holds Bedrock's legacy WebSocket events, which need event_subscribe; bridge holds script events the bedrock-mcp Bridge pack records, which need no subscription; native holds the events an action_sequence derives from native health reads when the pack is absent.\n\n",
                    "- Neither the ws nor the bridge list is exhaustive, and current Bedrock versions may send nothing for many ws names.\n",
                    "- PlayerMessage is always subscribed.\n\n",
                    "Returns {source, ws, bridge, native, always_subscribed, subscribed, observed: {ws, bridge}, notes}. ",
                    "ws, bridge and native are [{name, description}]; subscribed is the current subscriptions; observed lists the names seen in this session."
                ),
                "inputSchema": object(json!({}), &[])
            }),
        ),
        tool(
            Hints::SET,
            "Subscribe to WebSocket events",
            |s, a| reply(subscribe(s, arguments(a)?)?),
            json!({
                "name": "event_subscribe",
                "description": concat!(
                    "Use this before an action whose WebSocket events you want to read.\n\n",
                    "It subscribes to legacy WebSocket events by name (see events_list). ",
                    "The subscriptions are sent now if the game is connected, and again on every later connection.\n\n",
                    "- Names outside the known list are subscribed too and listed in unknown, since the game may never send them.\n",
                    "- A name must be a letter, then letters, digits or underscores.\n\n",
                    "Returns {subscribed, added, unknown, connected, notes}. subscribed is every subscription now; added holds the names that were new."
                ),
                "inputSchema": object(json!({
                    "names": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "Event names such as ItemUsed or PlayerTravelled" }
                }), &["names"])
            }),
        ),
        tool(
            Hints::SET,
            "Unsubscribe from WebSocket events",
            |s, a| reply(unsubscribe(s, arguments(a)?)?),
            json!({
                "name": "event_unsubscribe",
                "description": concat!(
                    "Use this to stop WebSocket events you no longer need.\n\n",
                    "It sends unsubscribe to the game and ignores any of those events still arriving.\n\n",
                    "- Fails when names include PlayerMessage, which stays subscribed because chat_send confirms chat with it.\n\n",
                    "Returns {subscribed, removed, not_subscribed}."
                ),
                "inputSchema": object(json!({
                    "names": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "Subscribed event names to stop; names not subscribed are listed in not_subscribed" }
                }), &["names"])
            }),
        ),
        tool(
            Hints::READ,
            "Read events",
            |s, a| reply(s.events_read(arguments(a)?)?),
            json!({
                "name": "events_read",
                "description": concat!(
                    "Use this to see what an action caused: take since \"now\" right before it, then read from that cursor after it.\n\n",
                    "It reads one stream merging WebSocket events (source ws, see event_subscribe), Bridge pack script events (source bridge, polled with re_mcp:events) and events an action_sequence derives from native health reads without the pack (source native: entity_hurt and entity_die of its targets). ",
                    "Pass the returned cursor as since to continue.\n\n",
                    "- since \"now\" reads nothing and returns the stream's current end.\n",
                    "- Without since it starts at the oldest event still held: 512 WebSocket, 2048 script and 4096 native events.\n",
                    "- The cursor moves past events the filters skipped.\n",
                    "- In one read, ws events come before bridge events, and bridge before native.\n\n",
                    "Returns {source, events, cursor, dropped: {ws, bridge, native}, notes}. ",
                    "Events are {cursor, source, name, received_at (Unix ms; poll time for bridge), tick (bridge: script tick; native: the server tick of the read), body}. ",
                    "dropped counts events lost before this read; notes say why bridge events were skipped or restarted."
                ),
                "inputSchema": object(json!({
                    "since": { "type": "string", "description": "A cursor from an earlier events_read or from an event, or \"now\" for the current end of the stream" },
                    "names": { "type": "array", "items": { "type": "string" }, "description": "Only these event names, ignoring case, e.g. [\"PlayerMessage\", \"block_break\"]" },
                    "sources": { "type": "array", "items": { "enum": ["ws", "bridge", "native"] }, "description": "Only events from these sources (default all)" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "description": "Default 100" },
                    "wait_ms": { "type": "integer", "minimum": 0, "maximum": 300000, "description": "Wait up to this long for at least one event that passes the filters (default 0, no wait)" }
                }), &[])
            }),
        ),
        tool(
            Hints::READ,
            "Wait for a condition",
            |s, a| reply(s.wait_for(arguments(a)?)?),
            json!({
                "name": "wait_until",
                "description": concat!(
                    "Use this to wait for a screen, an entity, a log entry or an event instead of sleeping.\n\n",
                    "It blocks until the condition holds and fails after timeout_ms. ",
                    "detail.source says what answered; notes explain fallbacks.\n\n",
                    "- screen: the top screen of any live scene stack starts with name (chat_screen, pause_screen, hud_screen, ...). Needs the screens capability; polls every 50 ms.\n",
                    "- in_world: the scene stacks, else the WebSocket; fails at once when neither tells.\n",
                    "- content_log: an entry written after the call, read live with the unstable layer (level matched by number), else from the file.\n",
                    "- text: an OCR line containing it, ignoring case, from a capture every 250 ms; region is in logical window pixels. detail.line has its box; a timeout lists the last capture's text.\n",
                    "- event: the first event in events_read's stream after the call; name ignores case, contains matches the body's JSON.\n",
                    "- chunks_loaded and entity connect the WebSocket if needed.\n\n",
                    "Returns {satisfied, waited_ms, detail, notes}."
                ),
                "inputSchema": object(json!({
                    "condition": {
                        "description": "Exactly one of: \"in_world\", \"camera_finished\", {\"screen\": {\"name\": \"chat\"}}, {\"chunks_loaded\": {\"radius\": 4}}, {\"entity\": {\"matches\": \"minecraft:zombie\", \"radius\": 32}}, {\"content_log\": {\"level\": \"error\", \"contains\": \"text\"}}, {\"text\": {\"contains\": \"Resume Game\", \"region\": {\"x\": 0, \"y\": 0, \"width\": 640, \"height\": 360}}}, {\"event\": {\"name\": \"block_break\", \"contains\": \"Steve\"}}; only screen.name, entity.matches and text.contains are required (radius defaults: chunks 2, entity 32)",
                        "oneOf": [{ "type": "string" }, { "type": "object" }]
                    },
                    "timeout_ms": { "type": "integer", "minimum": 0, "description": "Give up after this long (default 60000)" }
                }), &["condition"])
            }),
        ),
        tool(
            Hints::SET,
            "Save a test spot",
            |s, a| reply(s.scene_save(arguments(a)?, player::player_get)?),
            json!({
                "name": "scene_save",
                "description": concat!(
                    "Use this once per test spot, so later runs can return to it with scene_reset {spot}.\n\n",
                    "It saves the player's feet position, yaw, pitch and dimension under a name, or the place given in at. ",
                    "Saving a name again replaces it.\n\n",
                    "- Where the player stands comes from player_get: the Bridge pack, else in single player the unstable layer's native read (no cheats needed). Commands give no feet position, so with neither, pass at.\n",
                    "- Spots are stored per world folder in scenes/<folder>.json in the agent directory.\n",
                    "- The world is the one the game wrote last (the open one) unless world names another.\n\n",
                    "Returns {source (where the spot came from: bridge, native or argument), notes, saved, spot, world: {name, folder}, spots}. spots lists every spot name saved for that world."
                ),
                "inputSchema": object(json!({
                    "name": { "type": "string", "minLength": 1, "description": "Spot name, e.g. \"ice\"" },
                    "at": place_schema("Save this place instead of where the player stands"),
                    "world": { "type": "string", "description": "World name or folder id (default: the open world)" }
                }), &["name"])
            }),
        ),
        tool(
            Hints::DESTROY,
            "Reset a test scene",
            |s, a| reply(s.scene_reset(arguments(a)?, player::player_get)?),
            json!({
                "name": "scene_reset",
                "description": concat!(
                    "Use this at the start of every test run to put the player, world and inventory into a known state in one call.\n\n",
                    "It runs only the steps you give, in order: teleport, clear_entities, time, weather, difficulty, gamemode, effects, clear_inventory, give, heal, hotbar, perspective, warm_up. ",
                    "Then it waits settle_ms and reads the player. Commands need cheats.\n\n",
                    "- teleport (spot or teleport, not both) uses the Bridge pack's Script API, silent in chat, else /tp, then waits up to 5 s for chunks.\n",
                    "- clear_entities moves entities below the world instead of killing them, so nothing dies in view. Rain fades in over seconds. effects clears all effects first. heal gives instant health and saturation.\n",
                    "- hotbar uses the Bridge pack, else the slot key. perspective needs the unstable camera capability, else it is skipped with a note.\n",
                    "- warm_up swaps the hotbar slot and back and walks a block out and back to the spot, so a freshly loaded attachable animates; with player_render it checks and retries once. It reads the player through the Bridge pack, else natively in single player (taking the next slot unread and putting the player back with /tp); with neither it is skipped with a note.\n",
                    "- Nothing to remove, or no selector match, is ok with nothing_to_do: true; any other failed command stops with an error naming the step and the steps run.\n",
                    "- After a teleport the Bridge pack's raycast checks the spot: legs and head clear, not in water, 3 blocks open ahead. Without the pack, testforblock checks that the same blocks are air, so passable blocks count too, and notes say so. Problems go to spot_problems and make ok false.\n\n",
                    "Returns {ok, spot_problems, player, warm_up: {done, detail}, notes}. ",
                    "player is player_get's {source, dimension, location (eye instead when only commands answered), yaw, pitch, health, selectedSlot, perspective}. ",
                    "verbose adds world, spot, the full player and every step with its command and output."
                ),
                "inputSchema": object(json!({
                    "spot": { "type": "string", "description": "A spot saved with scene_save in this world" },
                    "teleport": place_schema("Teleport here instead of to a saved spot"),
                    "world": { "type": "string", "description": "World name or folder id whose spots to use (default: the open world)" },
                    "clear_entities": {
                        "oneOf": [{ "type": "boolean" }, { "type": "array", "items": { "type": "string" }, "minItems": 1 }],
                        "description": "true removes every entity except players, dropped items and experience orbs included, as /kill @e[type=!player] would; a list removes only these types, e.g. [\"minecraft:zombie\", \"minecraft:item\"]"
                    },
                    "time": { "oneOf": [{ "enum": ["day", "noon", "sunset", "night", "midnight", "sunrise"] }, { "type": "integer", "minimum": 0 }], "description": "A time name or ticks" },
                    "weather": { "enum": ["clear", "rain", "thunder"] },
                    "difficulty": { "enum": ["peaceful", "easy", "normal", "hard"] },
                    "gamemode": { "enum": ["survival", "creative", "adventure", "spectator"] },
                    "effects": {
                        "type": "array",
                        "items": object(json!({
                            "effect": { "type": "string", "description": "Effect id, e.g. speed" },
                            "seconds": { "type": "integer", "minimum": 1, "description": "Default 600" },
                            "amplifier": { "type": "integer", "minimum": 0, "maximum": 255, "description": "Default 0 (level I)" }
                        }), &["effect"]),
                        "description": "Effects to have afterwards, with particles hidden; [] only clears them"
                    },
                    "clear_inventory": { "type": "boolean", "description": "Empty the inventory before give (default false)" },
                    "give": {
                        "type": "array",
                        "items": object(json!({
                            "item": { "type": "string", "description": "Item id, e.g. demo:ruby_sword" },
                            "count": { "type": "integer", "minimum": 1, "description": "Default 1" },
                            "slot": { "type": "integer", "minimum": 0, "maximum": 8, "description": "Hotbar slot to put it in (replaceitem); without it /give adds it anywhere" }
                        }), &["item"])
                    },
                    "heal": { "type": "boolean", "description": "Fill health and hunger (default false)" },
                    "hotbar": { "type": "integer", "minimum": 0, "maximum": 8, "description": "Hotbar slot to select at the end" },
                    "perspective": { "enum": ["first_person", "third_person_back", "third_person_front"] },
                    "settle_ms": { "type": "integer", "minimum": 0, "maximum": 60000, "description": "Wait this long before reading the player (default 0)" },
                    "warm_up": { "type": "boolean", "description": "Swap the hotbar slot and back and walk a block and back as the last step, so the held item's attachable animates (default true)" },
                    "verbose": { "type": "boolean", "description": "Also return the world, the spot, every step's command and output and the whole player (default false)" }
                }), &[])
            }),
        ),
    ]
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandArgs {
    command: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NamesArgs {
    names: Vec<String>,
}

fn connect(session: &mut Session) -> Result<Value> {
    let world = session.ensure_world()?;
    let socket = world.socket();
    Ok(json!({
        "connected": true,
        "url": format!("ws://127.0.0.1:{}", socket.port()),
        "connections": socket.connections(),
        "bridge": world.bridge(true)?,
    }))
}

fn status(session: &Session) -> Value {
    let world = match session.world() {
        Ok(world) => world,
        Err(why) => return json!({ "listening": false, "error": why.to_string() }),
    };
    let socket = world.socket();
    let connections = socket.connections();
    let stats = socket.stats();
    json!({
        "listening": true,
        "url": format!("ws://127.0.0.1:{}", socket.port()),
        "connected": socket.connected(),
        "connections": connections,
        "subscriptions": stats.subscriptions,
        "observed_events": stats.observed,
        "events_received": stats.events_received,
        "events_dropped": stats.events_dropped,
        "commands_lost": stats.commands_lost,
        "commands_in_flight": socket.in_flight(),
        "max_commands_in_flight": ws::MAX_IN_FLIGHT,
        "bridge": world.cached_bridge(connections),
    })
}

/// The command line to send, without its leading `/`.
fn command_line(command: &str) -> Result<&str> {
    let line = command.trim().trim_start_matches('/');
    if line.is_empty() {
        bail!("command_execute needs a command.");
    }
    if line.contains(['\n', '\r']) {
        bail!("command_execute runs one command; send each line separately.");
    }
    Ok(line)
}

fn execute(session: &mut Session, args: CommandArgs) -> Result<Value> {
    let line = command_line(&args.command)?;
    run_command(&session.ensure_world()?, line)
}

fn run_command(world: &World, line: &str) -> Result<Value> {
    let body = world.raw(line)?;
    let code = body["statusCode"].as_i64().unwrap_or(0);
    let mut notes = Vec::new();
    if code == 0 && reopens_world(line) {
        notes.push(
            "reload all reopens the world: the game leaves it, the WebSocket stops answering \
             until the world is back, and a world that fails to reopen ends on a disconnect \
             screen. Call wait_until in_world, then check frame_capture before the next tool."
                .to_owned(),
        );
    }
    Ok(json!({
        "ok": code == 0,
        "status_code": code,
        "status_message": body["statusMessage"],
        "body": body,
        "notes": notes,
    }))
}

/// Whether a command line is `reload all`, which reopens the world.
fn reopens_world(line: &str) -> bool {
    let mut words = line.split_whitespace();
    words
        .next()
        .is_some_and(|word| word.eq_ignore_ascii_case("reload"))
        && words
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("all"))
}

/// Event names as the game spells them: a letter, then letters, digits or underscores.
fn check_names(names: &[String]) -> Result<()> {
    if names.is_empty() {
        bail!("Pass at least one event name.");
    }
    for name in names {
        let mut chars = name.chars();
        let valid = chars
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic())
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            bail!("{name:?} is not an event name; names look like ItemUsed or PlayerTravelled.");
        }
    }
    Ok(())
}

fn subscribe(session: &Session, args: NamesArgs) -> Result<Value> {
    check_names(&args.names)?;
    let world = session.world()?;
    let socket = world.socket();
    let added = socket.subscribe(&args.names);
    let unknown: Vec<&String> = args
        .names
        .iter()
        .filter(|name| !events::known_ws_event(name))
        .collect();
    let mut notes = Vec::new();
    if !unknown.is_empty() {
        notes.push(
            "Unknown names are subscribed, but the game may never send them; events_list shows the known ones."
                .to_owned(),
        );
    }
    let connected = socket.connected();
    if !connected {
        notes.push(
            "The game is not connected; the subscriptions are sent when it connects (automation_connect)."
                .to_owned(),
        );
    }
    Ok(json!({
        "subscribed": socket.stats().subscriptions,
        "added": added,
        "unknown": unknown,
        "connected": connected,
        "notes": notes,
    }))
}

fn unsubscribe(session: &Session, args: NamesArgs) -> Result<Value> {
    check_names(&args.names)?;
    let world = session.world()?;
    let socket = world.socket();
    let removed = socket.unsubscribe(&args.names)?;
    let not_subscribed: Vec<&String> = args
        .names
        .iter()
        .filter(|name| !removed.contains(name))
        .collect();
    Ok(json!({
        "subscribed": socket.stats().subscriptions,
        "removed": removed,
        "not_subscribed": not_subscribed,
    }))
}

/// A place to stand: feet position, rotation and dimension.
fn place_schema(description: &str) -> Value {
    describe(
        object(
            json!({
                "x": { "type": "number" },
                "y": { "type": "number", "description": "Feet height" },
                "z": { "type": "number" },
                "yaw": { "type": "number", "description": "Bedrock degrees: 0 faces +Z (south), 90 faces -X (west)" },
                "pitch": { "type": "number", "minimum": -90, "maximum": 90, "description": "Degrees, positive looks down (default 0 when yaw is given)" },
                "dimension": { "enum": ["overworld", "nether", "the_end", "minecraft:overworld", "minecraft:nether", "minecraft:the_end"] }
            }),
            &["x", "y", "z"],
        ),
        description,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::fake::FakeGame;

    #[test]
    fn commands_return_the_game_status() {
        let game = FakeGame::start(false);
        let ok = run_command(&game.world, command_line("/time set day").unwrap()).unwrap();
        assert_eq!(ok["ok"], true);
        assert_eq!(ok["status_code"], 0);
        assert_eq!(ok["status_message"], "ran time set day");
        let failed =
            run_command(&game.world, command_line("testfor @e[type=cow]").unwrap()).unwrap();
        assert_eq!(failed["ok"], false);
        assert_eq!(failed["status_code"], 1);
        assert_eq!(failed["status_message"], "No targets matched selector");
        assert_eq!(
            *game.commands.lock().unwrap(),
            ["time set day", "testfor @e[type=cow]"]
        );
        assert!(command_line(" / ").is_err());
        assert!(command_line("say a\nsay b").is_err());
    }

    #[test]
    fn event_names_are_checked() {
        assert!(check_names(&["ItemUsed".into(), "Custom_1".into()]).is_ok());
        assert!(check_names(&[]).is_err());
        assert!(check_names(&["1Bad".into()]).is_err());
        assert!(check_names(&["has space".into()]).is_err());
    }
}
