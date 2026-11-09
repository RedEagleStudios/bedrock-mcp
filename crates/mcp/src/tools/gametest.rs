//! The `gametest` toolset: GameTest simulated players and test runs.

use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    Handler, Hints, Tool, Toolset, arguments, reply,
    schema::{describe, object},
};
use crate::{
    error::Result,
    gametest::{self, Run, Spawn},
    session::Session,
};

pub fn tools() -> Vec<Tool> {
    let tool = |hints: Hints, title: &'static str, run: Handler, definition| {
        Tool::new(Toolset::Gametest, hints, title, run, definition)
    };
    vec![
        tool(
            Hints::ACT,
            "Spawn a simulated player",
            |s, a| reply(spawn(s, arguments(a)?)?),
            json!({
                "name": "gametest_spawn",
                "description": format!(
                    concat!(
                        "Use this when a test needs a second player in the world.\n\n",
                        "Spawns a GameTest simulated player. Scripts, commands and the Bridge see it by name, like a real player.\n\n",
                        "- Without location it spawns at the connected player, in that player's dimension; the two may push each other apart, so read player.location.\n",
                        "- The reply describes the player half a second after the spawn, so it shows items add-ons give on join.\n",
                        "- Fails when the name is taken.\n",
                        "- Needs the bedrock-mcp GameTest pack, as do gametest_list and gametest_remove: {}\n\n",
                        "Returns {{source, notes, player}}, where player is {}."
                    ),
                    gametest::SETUP,
                    PLAYER_FIELDS
                ),
                "inputSchema": object(json!({
                    "name": { "type": "string", "minLength": 1, "maxLength": 64, "description": "The player's name, unique among simulated players" },
                    "location": vec3("Where to spawn, in world coordinates"),
                    "game_mode": { "enum": ["survival", "creative", "adventure", "spectator"], "description": "The player's game mode (default survival)" }
                }), &["name"])
            }),
        ),
        tool(
            Hints::READ,
            "List simulated players",
            |s, _| reply(list(s)?),
            json!({
                "name": "gametest_list",
                "description": format!(
                    concat!(
                        "Use this to see the simulated players in the world and whether they are alive.\n\n",
                        "Lists every GameTest simulated player, whether gametest_spawn or a running test made it. ",
                        "Needs the bedrock-mcp GameTest pack.\n\n",
                        "- A dead one stays listed with alive false and health 0 until removed; it cannot act.\n\n",
                        "Returns {{source, notes, players}}, where each player is {}."
                    ),
                    PLAYER_FIELDS
                ),
                "inputSchema": object(json!({}), &[])
            }),
        ),
        tool(
            Hints::DESTROY,
            "Remove a simulated player",
            |s, a| reply(remove(s, arguments(a)?)?),
            json!({
                "name": "gametest_remove",
                "description": concat!(
                    "Use this to remove a simulated player you no longer need.\n\n",
                    "Disconnects a GameTest simulated player, which removes it from the world. Needs the bedrock-mcp GameTest pack.\n\n",
                    "Returns {source, notes, removed}, where removed is the name."
                ),
                "inputSchema": object(json!({ "name": { "type": "string", "minLength": 1, "description": "The simulated player's name (see gametest_list)" } }), &["name"])
            }),
        ),
        tool(
            Hints::ACT,
            "Run GameTests",
            |s, a| reply(run(s, arguments(a)?)?),
            json!({
                "name": "gametest_run",
                "description": concat!(
                    "Use this to run the add-on's registered GameTests in the open world.\n\n",
                    "Sends /gametest over the WebSocket. The tests come from behavior packs in the world; the game builds each structure next to the player.\n\n",
                    "- test runs `/gametest run`, tag `/gametest runset` (runsetuntilfail with until_fail), neither `runset suite:all`.\n",
                    "- Needs the Beta APIs experiment and cheats, not the bedrock-mcp GameTest pack.\n",
                    "- The reply says whether tests started, not whether they passed: the WebSocket may not carry outcomes, ",
                    "so read content_log_read after the tests' max ticks. gametest_server reports each result instead.\n\n",
                    "Returns {source, notes, command, message, messages}: message is the game's answer, messages the chat lines that arrived within wait_ms."
                ),
                "inputSchema": object(json!({
                    "test": { "type": "string", "description": "A registered test id, <class>:<name>" },
                    "tag": { "type": "string", "description": "A test tag or class name" },
                    "rotation": { "type": "integer", "minimum": 0, "maximum": 3, "description": "Quarter turns clockwise for the test structures" },
                    "repeat": { "type": "integer", "minimum": 1, "description": "Runs of a single test" },
                    "stop_on_failure": { "type": "boolean", "description": "Stop repeating a single test at its first failure" },
                    "until_fail": { "type": "boolean", "description": "With a tag: stop the set at the first failure" },
                    "wait_ms": { "type": "integer", "minimum": 0, "description": "How long to collect chat messages after the command (default 0)" }
                }), &[])
            }),
        ),
        tool(
            Hints::ACT,
            "Run GameTests on a dedicated server",
            |s, a| reply(s.gametest_server(arguments(a)?)?),
            json!({
                "name": "gametest_server",
                "description": concat!(
                    "Use this to run the add-on's GameTests without a game client, such as in CI.\n\n",
                    "Runs the tests on a Bedrock Dedicated Server (BDS) in a scratch folder that links the server's files, and reports every result. ",
                    "The world is a copy of the client world named by world, with its packs, or a fresh flat world.\n\n",
                    "- Turns on Beta APIs, turns off mob spawning, daylight and weather, runs commands, then /gametest as gametest_run does.\n",
                    "- Returns once every expected test has a result and output is quiet for 5 s, after idle_ms without progress, or at timeout_ms.\n",
                    "- Structures come from `structures/<namespace>/<name>.mcstructure` in a pack or the world; make one with ",
                    "`tickingarea add 0 0 0 15 0 15 gt`, `fill 0 -60 0 4 -56 4 air`, `structure save <namespace>:<name> 0 -60 0 4 -56 4 disk` (flat air starts at y -60).\n",
                    "- Default server: ~/.local/share/bedrock-server/current (%LOCALAPPDATA%\\bedrock-server\\current on Windows); ",
                    "download from https://www.minecraft.net/en-us/download/server/bedrock.\n",
                    "- BDS can trail the client: packs needing a newer engine or Script API fail to load (see errors and pack_stack).\n",
                    "- No tests, stalls and failures are normal replies; notes say which.\n\n",
                    "Returns {server, command, expected, passed, failed, results, timed_out, errors, pack_stack, command_replies, log, elapsed_ms, notes}: ",
                    "errors groups distinct ERROR lines, log is the full server output, timed_out also covers idle_ms."
                ),
                "inputSchema": object(json!({
                    "server": { "type": "string", "description": "The BDS folder (default $BEDROCK_MCP_BDS, else the default server folder)" },
                    "packs": { "type": "array", "items": { "type": "string" }, "description": "Behavior and resource pack folders to load and apply to the world; each manifest says which kind" },
                    "world": { "type": "string", "description": "A client world to copy, by name or folder id; without it the run makes a fresh flat world" },
                    "test": { "type": "string", "description": "A registered test id, <class>:<name>" },
                    "tag": { "type": "string", "description": "A test tag or class name" },
                    "rotation": { "type": "integer", "minimum": 0, "maximum": 3, "description": "Quarter turns clockwise for the test structures" },
                    "repeat": { "type": "integer", "minimum": 1, "description": "Runs of a single test" },
                    "stop_on_failure": { "type": "boolean", "description": "Stop repeating a single test at its first failure" },
                    "until_fail": { "type": "boolean", "description": "With a tag: stop the set at the first failure" },
                    "commands": { "type": "array", "items": { "type": "string" }, "description": "Console commands (no slash) to run before the tests; each reply is returned" },
                    "timeout_ms": { "type": "integer", "minimum": 1, "description": "Longest the tests may run (default 900000)" },
                    "idle_ms": { "type": "integer", "minimum": 1, "description": "Stop when no test makes progress for this long (default 120000)" }
                }), &[])
            }),
        ),
    ]
}

const PLAYER_FIELDS: &str = "{name, id, dimension, location, eye, yaw, pitch, headYaw, headPitch, health {current, max}, alive, gameMode, selectedSlot, selectedItem {id, amount} or null, sprinting, onGround}";

fn vec3(description: &str) -> Value {
    let axes = json!({
        "x": { "type": "number" },
        "y": { "type": "number" },
        "z": { "type": "number" }
    });
    describe(object(axes, &["x", "y", "z"]), description)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Named {
    name: String,
}

fn spawn(session: &mut Session, args: Spawn) -> Result<Value> {
    let player = gametest::spawn(&session.ensure_world()?, &args)?;
    Ok(json!({ "source": "gametest", "notes": [], "player": player }))
}

fn list(session: &mut Session) -> Result<Value> {
    let players = gametest::list(&session.ensure_world()?)?;
    Ok(json!({ "source": "gametest", "notes": [], "players": players }))
}

fn remove(session: &mut Session, args: Named) -> Result<Value> {
    gametest::remove(&session.ensure_world()?, &args.name)?;
    Ok(json!({ "source": "gametest", "notes": [], "removed": args.name }))
}

fn run(session: &mut Session, args: Run) -> Result<Value> {
    let mut result = gametest::run(&session.ensure_world()?, &args)?;
    result["source"] = json!("command");
    result["notes"] = json!([]);
    Ok(result)
}
