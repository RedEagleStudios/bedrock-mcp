//! The `dev` toolset: logs, packs, skins and the unstable layer.

use serde_json::json;

use super::{Handler, Hints, Tool, Toolset, arguments, reply, schema::object};

pub fn tools() -> Vec<Tool> {
    let tool = |hints: Hints, title: &'static str, run: Handler, definition| {
        Tool::new(Toolset::Dev, hints, title, run, definition)
    };
    vec![
        tool(
            Hints::READ,
            "Read the content log",
            |s, a| reply(s.content_log(arguments(a)?)?),
            json!({
                "name": "content_log_read",
                "description": concat!(
                    "Use this after packs load or reload, and after each test step, to catch pack and script errors.\n\n",
                    "Reads this game session's content log entries (pack and script errors and warnings) after since, waiting up to wait_ms for one. ",
                    "Pass the reply's cursor back as since to continue.\n\n",
                    "- auto reads live entries from the unstable layer's content_log capability when active, else the log file; a failed live read falls back to the file with a note.\n",
                    "- Cursors are live@<seq> or <file>@<offset>. A file cursor continues the file; a live cursor fails with source file and restarts the file when auto falls back.\n",
                    "- File entries are {time, area, level, message} with level names; live entries are {seq, area, level, area_name, level_name, message} with numeric area and level.\n",
                    "- Use summary first when a pack floods the log: hundreds of errors usually come from a few causes.\n\n",
                    "Returns {entries, cursor, source, notes}, plus dropped (entries the live buffer lost). ",
                    "summary replaces entries with total and groups [{level, area, pattern, count, values, example}], most frequent first: ",
                    "pattern is the first line with variable parts as *, values the distinct count and up to 5 samples per *."
                ),
                "inputSchema": object(json!({
                    "since": { "type": "string", "description": "Cursor from a previous call" },
                    "levels": { "type": "array", "items": { "type": "string" }, "description": "Only entries at these levels, ignoring case: names (verbose, info, warning, error) or, for live entries, numbers too (default every level)" },
                    "summary": { "type": "boolean", "description": "Group entries by cause instead of listing them (default false)" },
                    "wait_ms": { "type": "integer", "minimum": 0, "description": "Wait up to this long for at least one matching entry (default 0, no wait)" },
                    "source": { "enum": ["auto", "live", "file"], "description": "Where entries come from: the unstable layer (live), the log file, or live when it can (auto, the default)" }
                }), &[])
            }),
        ),
        tool(
            Hints::ACT,
            "Reload edited packs",
            |s, a| reply(s.game_reload(arguments(a)?)?),
            json!({
                "name": "game_reload",
                "description": concat!(
                    "Use this after you edit an add-on's files, to apply the edit in the open world and see the errors it caused.\n\n",
                    "Copies the packs over the development pack folders the game loaded them from (or runs `rgl run` for an rgl project), runs /reload all, ",
                    "waits for the world to reopen, and maps the content log entries the reload produced to files and lines. ",
                    "Works without the Bridge pack and never adds packs to a world.\n\n",
                    "- Each pack matches its folder in development_behavior_packs or development_resource_packs by manifest uuid. ",
                    "A pack the game has never loaded fails before anything is copied: the game finds new folders only when it starts, so install it once and restart.\n",
                    "- project runs `rgl run` when rgl is on PATH (up to 300 s, with COM_MOJANG set to the game's data folder when unset); otherwise it copies the project's pack folders as they are and notes that filters did not run.\n",
                    "- Errors and warnings come from the unstable layer's content_log capability when active, else the content log file. settle_ms after the world is back catches script errors.\n",
                    "- diagnostics hold the problems in the given packs' files; other groups the rest (other packs in the world, entries naming no file) by cause.\n",
                    "- line is null when the entry states none and the key or identifier it names is not unique in the file.\n",
                    "- Needs a world with cheats, as /reload all does.\n\n",
                    "Returns {reloaded, world_back, synced: [{kind, name, uuid, source, installed, method}], rgl, diagnostics: [{file, line, column, severity, rule, message, source}], ",
                    "counts {error, warning, info}, other {count, groups}, entries, source, elapsed_ms, notes}: file is BP/... or RP/..., rule a kebab-case code from the log area, ",
                    "groups are as content_log_read's summary, entries the number of log entries read, source live or file; rgl is {command, output, elapsed_ms} or null."
                ),
                "inputSchema": object(json!({
                    "packs": { "type": "array", "items": { "type": "string" }, "description": "Behavior and resource pack folders you edited; each manifest says which kind" },
                    "project": { "type": "string", "description": "An rgl project folder (with config.json), instead of packs" },
                    "timeout_ms": { "type": "integer", "minimum": 1, "description": "Longest wait for the world to reopen (default 90000)" },
                    "settle_ms": { "type": "integer", "minimum": 0, "maximum": 30000, "description": "Time after the world is back for scripts to run and log (default 3000)" }
                }), &[])
            }),
        ),
        tool(
            Hints::READ,
            "Check packs on a dedicated server",
            |s, a| reply(s.bds_check(arguments(a)?)?),
            json!({
                "name": "bds_check",
                "description": concat!(
                    "Use this to check that packs load cleanly without a game client, such as in CI or before adding them to a world.\n\n",
                    "Starts a Bedrock Dedicated Server (BDS) in a scratch folder with a fresh flat world that applies the packs, ",
                    "collects the errors and warnings it prints while it loads them and runs their scripts, then stops it and removes the folder.\n\n",
                    "- Waits for \"Server started.\", then until output is quiet for 2 s (at most 15 s); timeout_ms bounds the whole check.\n",
                    "- A given pack missing from the server's pack stack is an error with rule pack-not-loaded: BDS can trail the client, so packs needing a newer engine or Script API fail to load.\n",
                    "- Default server: $BEDROCK_MCP_BDS, else ~/.local/share/bedrock-server/current (%LOCALAPPDATA%\\bedrock-server\\current on Windows); ",
                    "download from https://www.minecraft.net/en-us/download/server/bedrock.\n",
                    "- A server that does not start is a normal reply with started false and the log's last lines in notes.\n\n",
                    "Returns {server {path, version}, started, timed_out, diagnostics, counts, pack_stack, log, elapsed_ms, notes}: ",
                    "diagnostics are as game_reload gives them, with source bds; log is the server's output, kept after the check."
                ),
                "inputSchema": object(json!({
                    "packs": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "Behavior and resource pack folders to load; each manifest says which kind" },
                    "server": { "type": "string", "description": "The BDS folder (default $BEDROCK_MCP_BDS, else the default server folder)" },
                    "beta_apis": { "type": "boolean", "description": "Turn on the Beta APIs experiment, for packs that use beta Script API modules (default false)" },
                    "timeout_ms": { "type": "integer", "minimum": 1, "description": "Longest the whole check may take (default 120000)" }
                }), &["packs"])
            }),
        ),
        tool(
            Hints::SET,
            "Set log levels",
            |s, a| reply(s.log_filter_set(arguments(a)?)?),
            json!({
                "name": "log_levels_set",
                "description": concat!(
                    "Use this when you need more or less detail from an area in the game's debug log (in its logs folder).\n\n",
                    "Sets the debug log filter in options.txt for the next launch. ",
                    "Minecraft may rewrite options.txt when it exits, so this fails while the game runs.\n\n",
                    "- Levels are verbose, info, warn and error; area all means every area.\n",
                    "- Bedrock keeps one set of areas and one of priorities, so every named area logs from the most verbose level named.\n\n",
                    "Returns {spec, effective, applies_on_next_launch}; effective is what log_levels_get would report."
                ),
                "inputSchema": object(json!({ "spec": { "type": "string", "description": "Comma-separated area=level directives, e.g. all=warn,ui=error" } }), &["spec"])
            }),
        ),
        tool(
            Hints::READ,
            "Get log levels",
            |s, _| reply(s.log_filter_get()?),
            json!({
                "name": "log_levels_get",
                "description": concat!(
                    "Use this to see which debug log areas and levels are on.\n\n",
                    "Reads the debug log filter from options.txt, whether or not the game runs.\n\n",
                    "Returns {areas, priorities, spec}."
                ),
                "inputSchema": object(json!({}), &[])
            }),
        ),
        tool(
            Hints::ACT,
            "Global resource packs",
            |s, a| reply(s.global_packs(arguments(a)?)?),
            json!({
                "name": "packs_global",
                "description": concat!(
                    "Use this to switch global resource packs.\n\n",
                    "Reports the installed resource packs and the active global ones, top priority first. ",
                    "activate and deactivate stage changes in this session; apply writes global_resource_packs.json.\n\n",
                    "- activate puts available[index] at the top of active; deactivate removes active[index].\n",
                    "- The game reads the file at startup, so applying while it runs reports restart_required: true.\n\n",
                    "Returns {available, active, pending, restart_required}: pending says staged changes are not applied yet; restart_required is null unless this call applied."
                ),
                "inputSchema": object(json!({
                    "action": { "oneOf": [
                        { "enum": ["apply"] },
                        object(json!({ "activate": object(json!({ "index": { "type": "integer", "minimum": 0 } }), &["index"]) }), &["activate"]),
                        object(json!({ "deactivate": object(json!({ "index": { "type": "integer", "minimum": 0 } }), &["index"]) }), &["deactivate"])
                    ], "description": "apply, {activate: {index}} or {deactivate: {index}}; indices follow the last reported available or active list" }
                }), &[])
            }),
        ),
        tool(
            Hints::READ,
            "List worlds",
            |s, a| reply(s.worlds_list(arguments(a)?)?),
            json!({
                "name": "worlds_list",
                "description": concat!(
                    "Use this to find a world's name, folder id and packs before world_open or world_packs_set.\n\n",
                    "Lists the local worlds from their files, whether or not the game runs. ",
                    "Each pack is its name, or its uuid when not installed.\n\n",
                    "- verbose or world gives each pack as {pack_id, version, name, location, folder}, with null name, location and folder when not installed.\n",
                    "- Worlds can share a name; the folder id tells them apart.\n\n",
                    "Returns {worlds: [{name, folder, last_played, behavior_packs, resource_packs}]}; last_played is UTC text or null."
                ),
                "inputSchema": object(json!({
                    "verbose": { "type": "boolean", "description": "Every pack in full: uuid, version, name, location and folder (default false)" },
                    "world": { "type": "string", "minLength": 1, "description": "Only this world, in full: a folder id, or a name (every world of that name)" }
                }), &[])
            }),
        ),
        tool(
            Hints::DESTROY,
            "Set a world's packs",
            |s, a| reply(s.world_packs_set(arguments(a)?)?),
            json!({
                "name": "world_packs_set",
                "description": concat!(
                    "Use this to add your add-on, the Bridge pack or the GameTest pack to a closed world, when the user asked for it.\n\n",
                    "Replaces the behavior or resource pack list (or both) a local world applies. The game reads them when it next opens the world. ",
                    "Never add the Bridge or GameTest pack to a world on your own: ask the user first.\n\n",
                    "- Each pack is looked up in the world's packs, then development_*_packs, then *_packs; the version comes from its manifest.\n",
                    "- Refused while the game is in a world, because the game may rewrite the lists when it closes the world (call world_leave first); when that cannot be told, it writes and notes the risk.\n",
                    "- Fails when neither list is given or a pack is not found.\n",
                    "- Does not turn on experiments such as Beta APIs.\n\n",
                    "Returns the world as worlds_list with world shows it, plus note and notes."
                ),
                "inputSchema": object(json!({
                    "world": { "type": "string", "description": "World name or folder id" },
                    "behavior": { "type": "array", "items": { "type": "string" }, "description": "The world's full behavior pack list, top priority first: each a pack uuid, folder name or manifest name" },
                    "resource": { "type": "array", "items": { "type": "string" }, "description": "The world's full resource pack list, top priority first: each a pack uuid, folder name or manifest name" }
                }), &["world"])
            }),
        ),
        tool(
            Hints::SET,
            "Install the bedrock-mcp packs",
            |s, a| reply(s.packs_install(arguments(a)?)?),
            json!({
                "name": "packs_install",
                "description": concat!(
                    "Use this when the user agreed to install the Bridge or GameTest pack, or with check: true to see whether they are missing ",
                    "or older than this server.\n\n",
                    "Writes the Bridge and GameTest behavior packs built into this server into development_behavior_packs, so their commands match it.\n\n",
                    "- A new pack shows up after a restart, an updated one after /reload all; notes say which while the game runs.\n",
                    "- Never install without check: true on your own: suggest it to the user when a tool says it needs the pack, and wait for a yes.\n",
                    "- Then add them with world_packs_set; the GameTest pack also needs the Beta APIs experiment.\n",
                    "- Fails, naming the export commands, when this build embeds no packs.\n\n",
                    "Returns {packs: [{uuid, version, folder, path, status, before, written}], notes}: status is missing, outdated, current, ",
                    "or updated with before; written says this call wrote it."
                ),
                "inputSchema": object(json!({
                    "check": { "type": "boolean", "description": "Only compare the installed copies (default false)" }
                }), &[])
            }),
        ),
        tool(
            Hints::SET,
            "Install a skin pack",
            |s, a| reply(s.import_skin(arguments(a)?)?),
            json!({
                "name": "skin_install",
                "description": concat!(
                    "Use this to test a skin pack.\n\n",
                    "Installs a skin pack folder, .mcpack or .zip (manifest.json and skins.json) into development_skin_packs; ",
                    "the Dressing Room lists it after a restart.\n\n",
                    "- Single PNG skins are refused: Bedrock imports them only through the Dressing Room's file picker.\n\n",
                    "Returns {installed, name, restart_required, note}; installed is the new folder."
                ),
                "inputSchema": object(json!({ "path": { "type": "string", "description": "A skin pack folder, .mcpack or .zip on this machine" } }), &["path"])
            }),
        ),
        tool(
            Hints::ACT,
            "Unstable layer status",
            |s, _| reply(s.unstable_status()),
            json!({
                "name": "unstable_status",
                "description": concat!(
                    "Use this when a tool notes that an unstable capability is missing, failed or pending.\n\n",
                    "Reports the optional unstable layer (bedrock_unstable.dll, which reads game internals), loading it once the stable agent is attached.\n\n",
                    "- It loads on any build; BEDROCK_MCP_UNSTABLE=0 turns it off.\n",
                    "- A failed load is retried only after the game restarts.\n",
                    "- Each capability checks itself and, when failed or unavailable, says why without affecting the others.\n",
                    "- Stable tools never fail because of it; they fall back with a note.\n\n",
                    "Returns {setting, enabled, pid, load, status, note}: load is {state: not_loaded, loaded or failed, detail}; ",
                    "status is the DLL's build and capabilities, or null."
                ),
                "inputSchema": object(json!({}), &[])
            }),
        ),
        tool(
            Hints::READ,
            "Read the player's animation state",
            |s, a| reply(s.player_render_get(arguments(a)?)?),
            json!({
                "name": "player_render_get",
                "description": concat!(
                    "Use this to check which animations and animation controllers play on the player.\n\n",
                    "Reads the local player's client animation components, with their controllers, animations and attachables, ",
                    "through the unstable layer's player_render capability. The game reads each one on its own thread the next time it evaluates it.\n\n",
                    "- In first person the client component's root controller is in first_person and draws the arms and held item; client_hud is the model menus show.\n",
                    "- A frozen attachable is a child with no players, controllers in the wrong state for the held item, or frames_behind and applied.age_ms growing between calls.\n",
                    "- Without a WebSocket connection the layer knows the player only once a menu showed its model or first person ran; detail says so.\n",
                    "- Fails when the capability is not active.\n\n",
                    "Returns {frame, components, visits_by_group, detail}. Each component has group, owner (AnimationComponentID), last_update_frame, ",
                    "frames_behind (lag behind frame), applied (last pose application, or null), read_age_ms, read_us, players and children; ",
                    "each player has kind, name, state, state_index (-1: none yet), last_state_index, state_time (s) and animations. ",
                    "visits_by_group counts evaluations per group index since load."
                ),
                "inputSchema": object(json!({
                    "wait_ms": { "type": "integer", "minimum": 0, "maximum": 2000, "description": "Wait up to this long for each component to be evaluated again (default 250)" }
                }), &[])
            }),
        ),
        tool(
            Hints::READ,
            "Read frame timings",
            |s, a| reply(s.perf_get(arguments(a)?)?),
            json!({
                "name": "perf_get",
                "description": concat!(
                    "Use this to measure frame times while your add-on runs.\n\n",
                    "Summarises the game's recent frame and update timings (last 1024 samples, or window_ms) through the unstable layer's perf capability.\n\n",
                    "- frame_ms: time between frame starts (MinecraftGame::update); update_ms: that call's wall time; ",
                    "game_update_ms: getGameUpdateDurationInSeconds; scene_frame_ms: time between updates of one UI scene stack.\n",
                    "- A large newest_age_ms means the game is stalled or not drawing.\n",
                    "- Fails when the capability is not active.\n\n",
                    "Returns {window_ms, long_frame_ms, newest_age_ms, frame_ms, update_ms, game_update_ms, scene_frame_ms, recent}: ",
                    "each series is {count, mean, min, p50, p90, p95, p99, max} in ms (nearest rank) or null, ",
                    "frame_ms and scene_frame_ms add fps and long; recent holds each series' newest values."
                ),
                "inputSchema": object(json!({
                    "window_ms": { "type": "number", "minimum": 0, "description": "Only samples from the last window_ms (default every kept sample)" },
                    "recent": { "type": "integer", "minimum": 0, "maximum": 256, "description": "Newest values to list per series (default 16)" },
                    "long_frame_ms": { "type": "number", "minimum": 0, "description": "Frames longer than this count as long (default 50)" }
                }), &[])
            }),
        ),
    ]
}
