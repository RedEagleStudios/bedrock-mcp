//! The `game` toolset: launching, joining, inspecting and stopping Minecraft.

use serde_json::json;

use super::{
    Handler, Hints, Tool, Toolset, arguments, reply,
    schema::{describe, object},
};

pub fn tools() -> Vec<Tool> {
    let tool = |hints: Hints, title: &'static str, run: Handler, definition| {
        Tool::new(Toolset::Game, hints, title, run, definition)
    };
    vec![
        tool(
            Hints::SET,
            "Launch Minecraft",
            |s, a| reply(s.launch_client(arguments(a)?)?),
            json!({
                "name": "game_launch",
                "description": concat!(
                    "Use this first in a session, to start Minecraft or attach to the running game.\n\n",
                    "It starts Minecraft if needed, waits for the main menu, and injects and connects the developer agent. ",
                    "A running game is only attached.\n\n",
                    "- A start first turns WebSockets on and encrypted WebSockets off in options.txt.\n",
                    "- Linux starts the game with the BEDROCK_MCP_LAUNCH_COMMAND shell command, else mcbe-gdk-linux; Windows uses the shell.\n",
                    "- headless (Linux, with gamescope installed) starts the game inside a headless gamescope: it renders at full speed on no screen, so it never covers the user's windows and the user cannot see it; the agent supplies its window, clicks and frames. Use it when the user does not need to watch or wants their screen free. Without the argument, BEDROCK_MCP_HEADLESS decides. A game already running stays where it runs; one attached inside gamescope is reported as headless.\n",
                    "- Linux finds a game from any launcher by its process; it fails, listing them, when several run and pid picks none.\n",
                    "- size applies on Hyprland (and makes the window floating) and on Windows; elsewhere notes say it was left alone.\n",
                    "- A running game ignores args and env. Windows takes no env and at most one minecraft:// link.\n\n",
                    "Returns {pid, already_running, agent, window, headless, menu_text, websocket_port, notes}: headless is {width, height} when the game runs inside gamescope, else null. menu_text is the menu text OCR read."
                ),
                "inputSchema": object(json!({
                    "args": { "type": "array", "items": { "type": "string" }, "description": "Game arguments, e.g. one minecraft:// link" },
                    "env": { "type": "object", "additionalProperties": { "type": "string" }, "description": "Extra environment for the launcher (Linux only)" },
                    "size": { "type": "array", "items": { "type": "integer", "minimum": 1 }, "minItems": 2, "maxItems": 2, "description": "[width, height] in logical pixels" },
                    "headless": { "type": ["boolean", "string"], "description": "Linux: true or a size such as \"1280x720\" runs a game this call starts in a headless gamescope (default 1920x1080); false starts it on the desktop. Default: the BEDROCK_MCP_HEADLESS setting" },
                    "timeout_ms": { "type": "integer", "minimum": 0, "description": "Main-menu wait (default 300000)" },
                    "pid": { "type": "integer", "minimum": 1, "description": "The game process to attach to when several run" }
                }), &[])
            }),
        ),
        tool(
            Hints::ACT.open_world(),
            "Open a world",
            |s, a| reply(s.world_open(arguments(a)?)?),
            json!({
                "name": "world_open",
                "description": concat!(
                    "Use this to enter a local world before testing in it.\n\n",
                    "It clicks through the menus like a player: Play, the Worlds tab, then the card labelled exactly name, paging through the list. ",
                    "It waits until the player is in the world and gameplay shows. Call automation_connect next.\n\n",
                    "- An unknown name fails at once and lists the worlds; a name several worlds share fails and lists their folder ids and last-played times.\n",
                    "- For a folder id whose name others share, it picks that world's card by last-played order (needs the UI tree), and leaves and fails if another world loaded.\n",
                    "- It opens only local worlds: if the loading screen names another job or the world's files stay unwritten, it backs out and fails.\n",
                    "- The unstable screens capability tells the menus apart; without it, it looks for the name before clicking Play.\n",
                    "- It knows the player is in from the screens capability or the WebSocket, else from OCR seeing no menu or loading text (noted).\n",
                    "- It then waits up to 20 s for the HUD, or a pause or chat screen.\n",
                    "- A world already loading is waited for without clicks.\n",
                    "- Fails when already in a world (call world_leave first).\n\n",
                    "Returns {world: {name, folder}, clicked, waited, notes, next}. waited is wait_until's reply plus hud_ms."
                ),
                "inputSchema": object(json!({
                    "name": { "type": "string", "minLength": 1, "description": "The world's folder id, or the name the world list shows when no other world shares it (see worlds_list)" },
                    "timeout_ms": { "type": "integer", "minimum": 0, "description": "Wait for the player to be in the world (default 120000)" }
                }), &["name"])
            }),
        ),
        tool(
            Hints::ACT,
            "Leave the world",
            |s, _| reply(s.world_leave()?),
            json!({
                "name": "world_leave",
                "description": concat!(
                    "Use this to return to the menus, for example before world_packs_set changes the world's packs.\n\n",
                    "It opens the pause menu with Escape when gameplay has the input, clicks Save & Quit, and waits up to 60 s for the main menu or the world list. ",
                    "The WebSocket stays connected; commands answer \"Unknown command\" until a world is open again.\n\n",
                    "- Without the unstable layer's screens capability it looks for Save & Quit before pressing Escape, and confirms the menu by OCR.\n",
                    "- Fails when the game is not in a world, or is still in it after Save & Quit.\n\n",
                    "Returns {clicked, waited_ms, notes}."
                ),
                "inputSchema": object(json!({}), &[])
            }),
        ),
        tool(
            Hints::DESTROY.open_world(),
            "Join a server",
            |s, a| reply(s.connect(arguments(a)?)?),
            json!({
                "name": "server_join",
                "description": concat!(
                    "Use this to test on a server instead of a local world.\n\n",
                    "It starts the game with minecraft://connect/?serverUrl=H&serverPort=P (the connect link on Microsoft Learn's Deep Link Handlers page), stopping a running game first, as game_launch does. ",
                    "Call wait_until in_world to know when the join finished.\n\n",
                    "- Refuses a non-loopback address unless allow_remote is set, since a remote join uses the signed-in account.\n",
                    "- local_server starts before the game and must bind its UDP port within 60 s; it fails when the port is already in use or the server exits.\n\n",
                    "Returns {address, link, restarted, note, local_server, launch}. local_server is {pid, port, log}; launch is game_launch's reply."
                ),
                "inputSchema": object(json!({
                    "address": { "type": "string", "description": "host:port" },
                    "allow_remote": { "type": "boolean", "description": "Allow a non-loopback address; the join uses the signed-in account (default false)" },
                    "local_server": describe(object(json!({
                        "command": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "Program and its arguments" },
                        "port": { "type": "integer", "minimum": 1, "maximum": 65535, "description": "UDP port the server binds" }
                    }), &["command", "port"]), "A server to start first; game_stop or the next local_server stops it, and its output goes to local-server.log in the agent directory"),
                    "timeout_ms": { "type": "integer", "minimum": 0, "description": "Main-menu wait after the restart (default 300000)" }
                }), &["address"])
            }),
        ),
        tool(
            Hints::READ,
            "Game status",
            |s, _| reply(s.state()),
            json!({
                "name": "game_status",
                "description": concat!(
                    "Use this to see the whole state at once before a test, or when a tool fails for an unclear reason.\n\n",
                    "It reads the game, agent, window, WebSocket, world and unstable layer in one call. ",
                    "Read attention and warnings first.\n\n",
                    "- attention is null while play can go on, else {kind, detail, next, source}: death (the live death screen, or Bridge pack health 0 when the unstable layer cannot tell) or disconnected. Check it when actions stop confirming.\n",
                    "- warnings are {kind, detail}; empty means a normal session. Kinds: death, disconnected, unstable_anchors_only (seen on Linux only), unstable_disabled, unstable_failed, unstable_capabilities, websocket_disconnected, screen_open, other_mods.\n",
                    "- in_world comes from the screens capability, else the WebSocket; null, with notes, when neither tells.\n",
                    "- world needs the WebSocket: with the Bridge pack, re_mcp:state (the player), re_mcp:world and nearby re_mcp:entities; without it, eye, yaw, pitch (bisected), dimension, nearby actors, time_of_day and game_time. Without the pack, in a world with the unstable layer's world_state capability, world.player adds the native player_get fields (source native), even without the WebSocket.\n",
                    "- On generic Linux desktops the window size is at scale 1 and workspace is null.\n",
                    "- input_lock is {enabled, locked, taken_over, banner, note}: while the server plays, the player's real keyboard and mouse are locked out of the game; taken_over is true once the player pressed F9 since the last lock started. BEDROCK_MCP_INPUT_LOCK=0 turns it off.\n\n",
                    "Returns {running, pid, crash_debugger, agent, window, headless, in_world: {value, source, notes}, content_log_errors, attention, warnings, websocket, world, camera, recording, timeline, held, input_lock, unstable}. ",
                    "window.desktop is hyprland, generic or windows; headless is {width, height} while the game runs inside gamescope (the user cannot see it), else null; unstable is null without the layer, else {capabilities, screens (scene stacks, bottom first), camera}."
                ),
                "inputSchema": object(json!({}), &[])
            }),
        ),
        tool(
            Hints::READ,
            "Read the HUD text",
            |s, a| reply(s.hud_get(arguments(a)?)?),
            json!({
                "name": "hud_get",
                "description": concat!(
                    "Use this to read the toasts, titles, action bar or chat your add-on shows.\n\n",
                    "It reads the HUD through the unstable layer's hud_text capability, without the Bridge pack or OCR, and fails when that is not active.\n\n",
                    "- toasts holds every toast since the DLL loaded, oldest first, at most 32.\n",
                    "- title, subtitle, actionbar, popup (item name) and tip are {text, changed_ms} or null. changed_ms is when the DLL first saw the text (Unix ms), null if it was there before.\n",
                    "- actionbar may still hold text after it faded from the screen: compare changed_ms with title_ticks (50 ms ticks). title and subtitle may read null even while the screen shows them.\n",
                    "- death comes from the camera's damage parameters or a JSON UI death screen (not Ore UI); shown is null when nothing tells.\n\n",
                    "Returns {toasts, title, subtitle, actionbar, title_ticks, popup, tip, chat, detail, fresh, death, notes}. ",
                    "Toasts are {seq, at_ms, kind (achievement, recipe_unlocked, message, ...), title, message, message_id, persistent}. ",
                    "chat lines are {kind (chat, system, ...), username, message, full (as drawn), duration (seconds left), seen}, oldest first. ",
                    "detail says why texts could not be read; fresh is whether they come from the answering frame. ",
                    "death is {shown, message, ticks, screen, texts, respawn_available}."
                ),
                "inputSchema": object(json!({
                    "chat_limit": { "type": "integer", "minimum": 0, "maximum": 100, "description": "Newest chat lines to return (default 20)" }
                }), &[])
            }),
        ),
        tool(
            Hints::DESTROY,
            "Stop Minecraft",
            |s, _| reply(s.quit()),
            json!({
                "name": "game_stop",
                "description": concat!(
                    "Use this to end a session or before a clean restart.\n\n",
                    "It releases held input, stops the camera path, finishes any recording and stops the local server, then stops Minecraft.\n\n",
                    "- Linux runs mcbe-gdk-linux stop for a game it launched, else sends the game SIGTERM.\n",
                    "- Windows closes the game window and terminates the game if it still runs 15 s later.\n\n",
                    "Returns {input_released, camera, recording, local_server, game}. game is {stopping, output}, {stopping: false, note} when Minecraft was not running, or {error}."
                ),
                "inputSchema": object(json!({}), &[])
            }),
        ),
        tool(
            Hints::DESTROY,
            "Crash Minecraft",
            |s, a| reply(s.crash(arguments(a)?)?),
            json!({
                "name": "game_crash",
                "description": concat!(
                    "Use this only to test crash handling.\n\n",
                    "It makes the agent crash Minecraft on purpose, the way kind names. ",
                    "Wine's or Windows' crash handling takes over, and the game is gone afterwards.\n\n",
                    "Returns {crashed, note}. The agent replies before it crashes the game; game_status crash_debugger shows whether the crash handler runs."
                ),
                "inputSchema": object(json!({ "kind": { "type": "string", "enum": ["panic", "access_violation", "stack_overflow", "abort"], "description": "How the agent crashes the game" } }), &["kind"])
            }),
        ),
    ]
}
