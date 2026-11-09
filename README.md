# Bedrock MCP for Minecraft

`bedrock-mcp` is an [MCP](https://modelcontextprotocol.io) server that lets an AI agent develop and
test Minecraft Bedrock add-ons. The agent launches the game and plays it with real keyboard and
mouse input. It reads player, entity, block and inventory state, runs commands and GameTest
simulated players, and checks packs against Mojang's rules and schemas. Screenshots, on-screen
text and recordings show it what happened.

It runs on Windows, and on Linux with the game under Wine or Proton. It starts the game itself or
attaches to a game that any launcher started.

> [!NOTE]
> Bedrock MCP is in early development. Tool names, arguments, replies and the packs' commands can
> change between releases. [Pin a release](#install) if you need them to stay the same.

**NOT AN OFFICIAL MINECRAFT PRODUCT. NOT APPROVED BY OR ASSOCIATED WITH MOJANG OR MICROSOFT.**

> [!WARNING]
> Use it only with local worlds and a dedicated server you run yourself. Never point it at Realms,
> featured servers, other people's servers or a friend's world. It loads DLLs into the game and
> sends synthetic input, which anti-cheat can treat as cheating and which can get the account
> banned. `world_open` opens only local worlds and backs out if a click starts joining another
> host's game. It cannot stop you from joining one by hand while it runs.

## How it works

| Part | What it does |
| --- | --- |
| `bedrock-mcp` | The stdio MCP server. It launches the game, injects the DLLs, runs the WebSocket server the game connects to with `/connect`, follows the game's logs and captures the window. |
| `bedrock_agent.dll` | Runs inside the game. It sends keyboard and mouse input, keeps the game active in the background and copies frames for recordings. It hooks GameInput, DXGI, D3D12 and the game's `user32` imports, never game code. |
| `bedrock_unstable.dll` | Optional. Reads game internals such as screens, UI controls, the camera and entities. See [Unstable layer](#unstable-layer). |
| Bridge pack | Optional behavior pack whose `re_mcp:*` commands answer with JSON. See [Packs](#packs). |
| GameTest pack | Optional behavior pack for GameTest simulated players. |

## Install

Linux, x86_64:

```bash
curl -fsSL https://github.com/RedEagleStudios/bedrock-mcp/releases/latest/download/install.sh | sh
```

Windows, x86_64, in PowerShell:

```powershell
irm https://github.com/RedEagleStudios/bedrock-mcp/releases/latest/download/install.ps1 | iex
```

The installer downloads the newest release, checks it against `SHA256SUMS` and prints the
configuration to add to your MCP client. On Linux it installs to
`${XDG_DATA_HOME:-~/.local/share}/bedrock-mcp` and links `~/.local/bin/bedrock-mcp`. On Windows it
installs to `%LOCALAPPDATA%\bedrock-mcp\bin` and adds that folder to `PATH`.

Run the same command again to update. It replaces the files in place, so the client configuration
stays valid. A running server keeps the old version until the client restarts it.
`bedrock-mcp --version` prints the installed version. To install a specific release, pass its tag:

```bash
curl -fsSL https://github.com/RedEagleStudios/bedrock-mcp/releases/latest/download/install.sh | sh -s -- v0.1.0
```

```powershell
& ([scriptblock]::Create((irm https://github.com/RedEagleStudios/bedrock-mcp/releases/latest/download/install.ps1))) v0.1.0
```

To uninstall, delete the install folder, and on Linux also `~/.local/bin/bedrock-mcp`. To install
from a mirror with the same layout as GitHub Releases, set `BEDROCK_MCP_RELEASES` to its URL.

To install by hand, download `bedrock-mcp-linux-x86_64.tar.gz` or `bedrock-mcp-windows-x86_64.zip`
from a [release](https://github.com/RedEagleStudios/bedrock-mcp/releases), unpack it anywhere and
point your MCP client at `bedrock-mcp`. Keep the files together, because the server looks for its
DLLs in its own folder. Each release also has the Bridge and GameTest packs as `.mcpack` files, for
adding them to a world on another machine.

## Quick start

On Linux you need the game under Wine or Proton. `game_launch` starts it with
[mcbe-gdk-linux](https://gitlab.com/veedy-dev/mcbe-gdk-installer) by default, and
[Linux](#linux) covers other launchers. On Windows you need the game installed. Recordings and
`video_analyze` need `ffmpeg`. Some tools need more:

- Node.js 22 or later for the `docs` tools. Bun can replace Node for `molang_eval` and the
  package downloads.
- `tesseract` for on-screen text the game's pixel fonts cannot explain.
- `hyprctl` and `grim` on Hyprland, to place, size and capture the window.

Add the server to your MCP client with the configuration the installer prints. The server gives
the agent these steps in its instructions:

1. `game_launch` starts the game, or attaches to one that runs.
2. `packs_install` writes the Bridge and GameTest packs into the game's development pack folders.
   The agent asks you first. The game needs one restart to see a new pack.
3. `world_open {name}` opens a local world. The world needs cheats on.
4. `automation_connect` opens the WebSocket for commands, events and the Bridge pack.
5. `scene_save` names a starting spot once. Before each test, `scene_reset` restores the spot,
   entities, time, weather, inventory, health, hotbar slot and perspective.
6. One `action_sequence` with `record: true` runs the test, records it and returns a summary per
   target.

The game needs two settings, and `game_launch` sets both before it starts the game. In Settings,
General, *Enable WebSockets* must be on and *Require Encrypted WebSockets* off. Set them yourself
for a game another launcher started.

The world needs cheats because the WebSocket opens when the player runs `/connect`. Without
cheats, commands, events and every tool that reads the world stop working. Input, screenshots, OCR
and the unstable layer still work. The server types `/connect` itself the first time a tool needs
the WebSocket. It closes a pause or chat screen first and refuses while another screen covers the
game. The connection stays open when you change worlds.

## Tools

[docs/tools.md](docs/tools.md) describes every tool and argument. The server generates it from its
tool definitions, and a test fails when it is out of date.

| Toolset | Tools |
| --- | --- |
| `game` | `game_launch`, `world_open`, `world_leave`, `server_join`, `game_status`, `hud_get`, `game_stop`, `game_crash` |
| `player` | `player_get`, `player_set`, `inventory_get`, `hotbar_select`, `look_at` |
| `world` | `target_get`, `entities_list`, `entity_inspect`, `entity_components_get`, `entity_properties_get`, `dynamic_properties_get`, `block_inspect`, `raycast`, `container_get`, `item_inspect`, `world_info`, `scoreboard_get` |
| `actions` | `input_send`, `ui_click`, `ui_tree`, `item_use`, `item_use_on`, `attack`, `fight`, `entity_interact`, `block_break`, `block_place`, `item_drop`, `movement_set`, `entity_spawn`, `action_sequence`, `sequence_status`, `sequence_stop` |
| `gametest` | `gametest_spawn`, `gametest_list`, `gametest_remove`, `gametest_run`, `gametest_server` |
| `automation` | `automation_connect`, `automation_status`, `command_execute`, `chat_send`, `events_list`, `event_subscribe`, `event_unsubscribe`, `events_read`, `wait_until`, `scene_save`, `scene_reset` |
| `media` | `frame_capture`, `video_start`, `video_stop`, `video_analyze`, `timeline_read`, `camera_play`, `camera_get`, `camera_set` |
| `dev` | `content_log_read`, `log_levels_set`, `log_levels_get`, `packs_install`, `packs_global`, `worlds_list`, `world_packs_set`, `skin_install`, `unstable_status`, `player_render_get`, `perf_get`, `game_reload`, `bds_check` |
| `docs` | `pack_validate`, `pack_info`, `pack_fix_versions`, `pack_check`, `schema_list`, `schema_get`, `script_api`, `component_docs`, `command_docs`, `molang_docs`, `molang_eval` |

Every read returns `source`, which names where the data came from, and `notes`, which say why a
tool fell back or what it could not confirm. Actions report `confirmed` and the evidence for it.
Player and action tools take `actor`, either `self` for the real player or the name of a GameTest
simulated player.

The `docs` tools need no running game. On first use they download Minecraft Creator Tools,
MolangJS, the rockide schemas, the bedrock-samples metadata and the `@minecraft/*` packages into
the server's cache. `bedrock-mcp check <path>` runs `pack_check` from a shell for CI. It prints one
line per finding and exits with 1 on errors, or 2 when it could not run.

After you edit an add-on, `game_reload {packs}` copies the packs over the ones the game loaded,
runs `/reload all` and returns the new errors and warnings with file and line. It never adds packs
to a world, and a pack the game has never loaded needs one restart first.

The server logs every tool call to `timeline/` in the [agent folder](#troubleshooting), and
`timeline_read` returns the recent calls. During a recording, each call also becomes a chapter in
the video.

### Input lock

While the server plays, it locks your keyboard and mouse out of the game, so a stray key press
cannot spoil a test. The action bar says so when the WebSocket is connected. Press F9 to take the
game back. A running `action_sequence` then stops with `taken_over: true`, and other calls stop
sending input with an error. The lock returns with the next tool call, and the game unlocks itself
if the server goes away. `BEDROCK_MCP_INPUT_LOCK=0` turns the lock off.

### Prompts

The server offers MCP prompts that spell out every tool call of a common flow. Many clients show
them as slash commands. [docs/tools.md](docs/tools.md#prompts) lists their arguments.

| Prompt | Flow |
| --- | --- |
| `playtest_item` | Tests an item against a spawned entity in one recorded `action_sequence` and checks the summary. |
| `record_combat` | Records a clean third-person clip of the same fight and checks it with `video_analyze`. |
| `debug_pack_errors` | Groups the content log's errors, validates the pack, checks the world's packs, reloads and reads the log again. |
| `test_ui_flow` | Clicks labels in order, such as `Settings > Video`, through menus or add-on forms and checks each screen. |

## Packs

The server carries the Bridge and GameTest packs, so they always match it. `packs_install` writes
them to the game's development behavior packs, and `packs_install {check: true}` reports whether
each is missing, outdated or current. The agent never installs a pack or adds one to a world
without asking you. `world_packs_set` is marked destructive, so clients that confirm such tools ask
too.

The game finds new development packs only when it starts. Restart it once after the first install,
then add the packs to a world. From then on, `/reload all` reloads every pack in the world,
including scripts and new `re_mcp:*` commands, and the game reopens the world by itself.
`/reload` alone reloads only scripts and functions.

The Bridge pack answers `re_mcp:*` commands with player, inventory, entity, block, container, item,
raycast, world and scoreboard data, and buffers script events. It is optional. Without it,
single-player tests read the game natively through the unstable layer and use vanilla commands.
These fallbacks parse command replies in English (United States). These tools still need the pack:
`inventory_get`, `target_get`, `entity_components_get`, `block_inspect`, `raycast`,
`container_get` and `item_inspect`.

The GameTest pack uses the beta `@minecraft/server-gametest` module, so the world needs the
**Beta APIs** experiment as well as cheats. Keep it out of worlds that must stay free of
experiments. The Bridge pack works without it.

## GameTest on a dedicated server

`gametest_server` runs an add-on's GameTests on a Bedrock Dedicated Server without a game client,
so it also works in CI. Download BDS from
[minecraft.net](https://www.minecraft.net/en-us/download/server/bedrock) and unzip it to
`~/.local/share/bedrock-server/current`, or `%LOCALAPPDATA%\bedrock-server\current` on Windows. To
use another folder, set `BEDROCK_MCP_BDS` or pass the tool's `server` argument.

Each call runs in a scratch server folder, so the BDS folder stays unchanged. It loads your packs
in a flat world or a copy of a client world, turns on the Beta APIs experiment, runs `/gametest`
and returns each test's result with the server log. A test builds its structure from
`structures/<namespace>/<name>.mcstructure` in a behavior pack, or from a structure saved in the
world. To save one from the server console:

```text
tickingarea add 0 0 0 15 0 15 gt
fill 0 -60 0 4 -56 4 air
structure save demo:walk 0 -60 0 4 -56 4 disk
```

BDS can be older than the client. It fails to load a pack whose `min_engine_version` or Script API
module versions it does not know, and the reply's `errors` and `pack_stack` show it.
`bds_check {packs}` loads packs on the same server without running tests and returns the errors
and warnings they caused.

## Settings

| Variable | Effect |
| --- | --- |
| `BEDROCK_MCP_TOOLSETS` | Comma-separated toolsets to offer, such as `game,player,actions,docs`. Unset offers all. An unknown name stops the server with the valid ones. |
| `BEDROCK_MCP_UNSTABLE` | `0` keeps the unstable layer from loading. |
| `BEDROCK_MCP_INPUT_LOCK` | `0` turns off the [input lock](#input-lock). |
| `BEDROCK_MCP_MCT` | Path to a Minecraft Creator Tools `mct` executable. Otherwise the server uses `mct` on `PATH`, then `npx`. |
| `BEDROCK_MCP_HOME` | Windows: the server's folder for the agent, screenshots and recordings. Default `%LOCALAPPDATA%\bedrock-mcp`. |
| `BEDROCK_MCP_WORKSPACE` | Linux: the Hyprland workspace the game window opens on, without switching you to it. |
| `BEDROCK_MCP_LAUNCH_COMMAND` | Linux: a shell command that starts the game instead of mcbe-gdk-linux. It runs detached through `sh -c` with the game arguments as `$@`, and the server then attaches to the game. |
| `BEDROCK_MCP_HEADLESS` | Linux: `1` for 1920x1080, or a size such as `1280x720`. Runs the game in [gamescope](https://github.com/ValveSoftware/gamescope)'s headless backend, which renders at 60 fps on no screen, so the game never covers your windows. Needs `gamescope`. |
| `BEDROCK_MCP_WINDOWS_DIR` | Linux: where the Windows build outputs are. |
| `MCBE_GDK_ROOT`, `BOL_WINEPREFIX` | Linux: where mcbe-gdk-linux is installed, default `${XDG_DATA_HOME:-~/.local/share}/mcbe-gdk-linux`, and its Wine prefix. Used when no game runs. |

`BEDROCK_MCP_LAUNCH_COMMAND` and `BEDROCK_MCP_HEADLESS` write the launch output to
`${XDG_DATA_HOME:-~/.local/share}/bedrock-mcp/launch-command.log`.

## Linux

The server finds a running game by its `Minecraft.Windows.exe` process and takes the Wine prefix
and game folders from it, so it works with any launcher. When several games run, `game_launch`
lists them and `game_launch {pid}` picks one. When none runs, `game_launch` runs
`BEDROCK_MCP_LAUNCH_COMMAND`, or mcbe-gdk-linux, or asks you to start the game.

It works on any desktop. On Hyprland it places, sizes and captures the window, and sets
`render_unfocused` so a hidden game keeps rendering at `misc:render_unfocused_fps`, which is 15 by
default. On other desktops the server cannot resize the window or move it to a workspace, and a
compositor that throttles hidden windows slows the game. `BEDROCK_MCP_HEADLESS` avoids that.

## Build from source

The server embeds the packs it finds in `mc/bridge/build` and `mc/gametest/build`, so build them
first. Both are [rgl](https://github.com/ink0rr/rgl) projects and need [Bun](https://bun.sh):

```bash
cd mc/bridge && bun install && rgl run build
cd mc/gametest && bun install && rgl run build
```

`rgl run` without `build` exports a pack straight to the game's development packs instead. Then
build the server.

Linux:

```bash
scripts/build-windows.sh                 # agent, unstable layer and injector, with MinGW or Docker
cargo build --release -p bedrock-mcp     # the server
```

The server finds the Windows files next to itself, in `target/x86_64-pc-windows-gnu/release`, or
in `BEDROCK_MCP_WINDOWS_DIR`. Run it from your desktop session.

Windows:

```powershell
scripts\build-windows.ps1                # server, agent and unstable layer in target\release
```

MCP client configuration:

```json
{ "mcpServers": { "minecraft": { "command": "/path/to/bedrock-mcp/target/release/bedrock-mcp" } } }
```

## Unstable layer

`bedrock_unstable.dll` reads game internals that the WebSocket, commands and scripts cannot reach.
The server injects it the first time a tool needs it. Its capabilities:

- `content_log`: content log entries as the game emits them.
- `screens`: the live screen stacks. `wait_until` needs it for screen waits.
- `ui_tree`: each JSON UI control's name, rectangle, text and toggle state, for `ui_tree` and
  `ui_click`.
- `oreui`: the clickable elements, lists and text of Ore UI pages such as the world list and
  settings, for `ui_tree` and `ui_click`. It runs a fixed, read-only script in each page.
- `camera`: the perspective, field of view and view direction, for `camera_get` and `camera_set`.
- `hud_text`: toasts, the action bar, popups, tips, chat lines and the death message, for
  `hud_get` and `game_status`.
- `player_render`: the player's animation controllers and attachables, for `player_render_get`
  and the warm-up check.
- `perf`: frame timings and the game's update time, for `perf_get`.
- `world_state`: the entities and dynamic properties on the game's local server, so single player
  only. `entities_list` and `entity_inspect` use it when the Bridge pack does not answer, and
  `dynamic_properties_get` always does.

It uses no hard-coded addresses. On a build that ships RTTI, it finds each class by name. On a
build without RTTI, it uses string anchors, rules that find a class by the strings, lambdas and
imports its code uses. Every capability works through anchors, but `ui_tree` then knows only text,
toggle, button and scroll view controls and reports the rest as `unknown`. The `anchor-rules` skill
in `.agents/skills/` explains the rules.

Each capability checks its vtable layout and confirms itself with a live call before it reports
`active`. One that fails reports `unavailable` or `failed` with the reason, and only the tools that
need it fall back. When the DLL is missing or disconnects, the other tools use the WebSocket, OCR
and the content log file instead, with a note that says why. `unstable_status` shows each
capability's state and whether RTTI or anchors found its classes.
`BEDROCK_MCP_UNSTABLE_ANCHORS_ONLY=1` in the game's environment makes the layer ignore RTTI, which
tests the anchors. A game that `game_launch` starts inherits it from the server.

## Troubleshooting

Start with `game_status`. Its `attention` names a screen that stops play and the next step, and
`warnings` lists modes that change how tools behave. `timeline_read` shows the recent tool calls
with their errors.

| Symptom | Cause and fix |
| --- | --- |
| `automation_connect` fails and chat says "Websocket server request rejected" | WebSockets are off. Turn them on in Settings, General, or let `game_launch` start the game. |
| Chat says "Cheats aren't enabled in this world." | The world has cheats off. Turn them on in the world's settings. |
| `re_mcp:*` commands are unknown | The Bridge pack is missing from the game or the world. Run `packs_install`, restart the game, add the pack with `world_packs_set`, then open the world. |
| A capability in `unstable_status` is `unavailable` or `failed` | Its `detail` gives the reason, and `bedrock-unstable.log` has more. After a game update, see the `update-game-version` skill in `.agents/skills/`. |
| `game_status` warns `other_mods` | Another mod is loaded and may hook the same functions as the agent. Launch the game without it. |
| `game_launch` cannot inject | On Linux, `injector.log` names the step that failed. |

The agent folder holds the DLLs' logs, `injector.log`, screenshots, recordings and the timeline:

- Windows: `%LOCALAPPDATA%\bedrock-mcp\agent`, or `agent` in `BEDROCK_MCP_HOME`.
- Linux with mcbe-gdk-linux's prefix: its `profile/agent`.
- Linux with any other prefix: `${XDG_DATA_HOME:-~/.local/share}/bedrock-mcp/games/<prefix path>/agent`.

## Limits

- **Hidden windows.** `frame_capture` reads the window's own buffer, and recordings come from
  inside the game, so both work on a hidden workspace or under other windows.
- **Background play.** While the MCP is connected, the agent withholds the game's focus-loss
  messages, answers `GetForegroundWindow` and `GetFocus` with the game's window, and drops
  `ClipCursor` requests while another window is in front. It never moves the real focus or your
  cursor.
- **Recording on Linux.** `ffmpeg` encodes with VAAPI and copies each frame through memory, about
  1.6 GB/s at 2522×1360 and 120 fps. `encoder_behind_frames` counts frames the encoder missed, and
  a game stall shows as repeated frames.
- **Recording on Windows.** The server uses the first `ffmpeg` encoder that works, in the order
  NVENC, AMF, Quick Sync, Media Foundation, x264. Audio comes from the default output device.
- **OCR.** It reads the game's pixel fonts, Mojangles and the Ore UI Minecraft fonts. It cannot
  read signs, icons, textures, or vector-font text such as Noto and non-Latin scripts.
- **Windows is unverified on real hardware.** The Windows build compiles and passes its tests under
  Wine. Launching, window handling, injection, encoding and audio capture have not run on a real
  Windows machine yet.

## Contributing

[AGENTS.md](AGENTS.md) is the contributor guide for people and coding agents. It covers the layout,
the checks to run, how to write tools, the rules for the unstable layer, releases, and what must
never go into the repository. Report security problems privately, as [SECURITY.md](SECURITY.md)
describes.

## Credits

- [LeviLamina](https://github.com/LiteLDev/LeviLamina) by LeviMC (LGPL-3.0): the class layouts the unstable layer reads, used as reference only.
- [EnTT](https://github.com/skypjack/entt) by Michele Caini (MIT): the layout of the game's entity component pools.
- [Coherent Gameface API reference](https://docs.coherent-labs.com/cpp-gameface/api_reference/classes/classcohtml_1_1_view/): the order of the Ore UI view's methods.
- [bedrock-samples](https://github.com/Mojang/bedrock-samples) by Mojang: vanilla command replies and the metadata the `docs` tools read.
- The `@minecraft/*` npm packages by Mojang (MIT): the Script API docs `script_api` shows.
- [MolangJS](https://github.com/JannisX11/MolangJS) by JannisX11 (MIT): runs `molang_eval`.
- [Minecraft Creator Tools](https://github.com/Mojang/minecraft-creator-tools) by Mojang: runs `pack_validate`, `pack_info` and `pack_fix_versions`.
- [rockide/schemas](https://github.com/rockide/schemas) by rockide: the schemas and vanilla names the `docs` tools use.

## License

MIT; see [LICENSE](LICENSE).
