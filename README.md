# Bedrock MCP for Minecraft

`bedrock-mcp` is an [MCP](https://modelcontextprotocol.io) server for agentic AI add-on development
and testing on Minecraft Bedrock. An agent launches the game, plays it with real keyboard and mouse
input, reads player, entity, block and inventory state, runs commands and GameTest simulated
players, watches game events, validates packs against Mojang's rules and schemas, and checks its
work with screenshots, on-screen text and recordings.

It runs on Windows, and on Linux with the game under Wine or Proton: it starts the game through
[mcbe-gdk-linux](https://gitlab.com/veedy-dev/mcbe-gdk-installer) or your own command, and attaches
to a game started by any launcher.

**NOT AN OFFICIAL MINECRAFT PRODUCT. NOT APPROVED BY OR ASSOCIATED WITH MOJANG OR MICROSOFT.**

> [!WARNING]
> Use it only with local worlds and a dedicated server you run yourself. Never point it at
> Realms, featured servers, other people's servers or a friend's world. It loads DLLs into the
> game and sends synthetic input, which a server's anti-cheat can treat as cheating and which can
> get the account banned. `world_open` opens only local worlds and backs out of a click that starts
> joining someone else's game, but it cannot stop you from joining one by hand while it runs.

## How it works

| Part | Where | What it does |
| --- | --- | --- |
| Agent | `crates/agent` builds `bedrock_agent.dll` | Loaded into `Minecraft.Windows.exe`. Posts key and text messages to the game window and merges synthetic buttons, motion, wheel and pointer into the game's GameInput mouse readings. Keeps the game active in the background and answers its cursor queries with the MCP's pointer. For recordings it copies presented frames out of the game on the GPU (DXGI `Present` and D3D12 queue hooks). It hooks GameInput, DXGI, D3D12 and the game's `user32` imports, never game code. |
| Injector | `crates/injector` | Loads the agent into the running game: `bedrock-inject.exe` under Wine on Linux, a library call inside the server on Windows. |
| Server | `crates/mcp` builds `bedrock-mcp` | The stdio MCP server. Launches the game, injects the agent, runs the WebSocket server the game connects to (`/connect`), follows the game's logs and captures the window. Platform code lives in `crates/mcp/src/platform/`. |
| Unstable layer | `crates/unstable` builds `bedrock_unstable.dll` | Optional. Reads game internals: content log, screen stacks, UI controls and Ore UI pages, camera, HUD text, server entities and dynamic properties, player animations and frame timings. See [Unstable layer](#unstable-layer). |
| OCR | `crates/ocr` | Reads text in captures by matching the game's own pixel fonts. |
| Protocol | `crates/protocol` | The JSON-lines messages between server and DLLs. |
| Bridge pack | `mc/bridge/` | Optional, but most add-on tests need it. A behavior pack whose `re_mcp:*` commands answer with JSON over the WebSocket: player, inventory, entities, blocks, containers, items, raycasts, world info, scoreboards and a buffer of script events. See [Packs](#packs) for what works without it. |
| GameTest pack | `mc/gametest/` | Optional behavior pack for GameTest simulated players. Needs the Beta APIs experiment. |

## Install

Linux (x86_64):

```bash
curl -fsSL https://github.com/RedEagleStudios/bedrock-mcp/releases/latest/download/install.sh | sh
```

Windows (x86_64), in PowerShell:

```powershell
irm https://github.com/RedEagleStudios/bedrock-mcp/releases/latest/download/install.ps1 | iex
```

The installer downloads the newest release, checks it against the release's `SHA256SUMS` and
prints the MCP client configuration. On Linux it installs to
`${XDG_DATA_HOME:-~/.local/share}/bedrock-mcp` and links `~/.local/bin/bedrock-mcp`. On Windows it
installs to `%LOCALAPPDATA%\bedrock-mcp\bin` and adds that folder to your `PATH`.

To update, run the same command again. It replaces the files in place, so the MCP client
configuration stays valid. A running server keeps the old version until the MCP client restarts
it. `bedrock-mcp --version` prints the installed version.

To install a specific release, pass its tag:

```bash
curl -fsSL https://github.com/RedEagleStudios/bedrock-mcp/releases/latest/download/install.sh | sh -s -- v0.2.0
```

```powershell
& ([scriptblock]::Create((irm https://github.com/RedEagleStudios/bedrock-mcp/releases/latest/download/install.ps1))) v0.2.0
```

To uninstall, delete the install folder (and `~/.local/bin/bedrock-mcp` on Linux).

`BEDROCK_MCP_RELEASES` points either installer at another location with the same layout as GitHub
Releases, such as a mirror. `scripts/test-install.sh` and `scripts/test-install.ps1` use it with a
local copy of a release (`scripts/release-server.py`) to test fresh installs, updates, pinned tags
and bad checksums; CI runs them on Linux and Windows.

Each [GitHub Release](https://github.com/RedEagleStudios/bedrock-mcp/releases) has:

- `bedrock-mcp-linux-x86_64.tar.gz`: the server with the agent, unstable layer and injector it
  runs inside the game under Wine.
- `bedrock-mcp-windows-x86_64.zip`: the server with the agent and unstable layer.
- `bedrock-mcp-bridge.mcpack` and `bedrock-mcp-gametest.mcpack`: the same packs the server
  carries, for adding them to a world by hand or on another machine.
- `install.sh`, `install.ps1` and `SHA256SUMS`.

To install by hand, unpack the archive anywhere and point your MCP client at `bedrock-mcp`
(`bedrock-mcp.exe` on Windows). The server finds the DLLs next to itself. The Bridge and GameTest
packs ship inside the server: `packs_install` writes them to the game's development behavior
packs, so they always match the server.

To publish a release, set `version` in the workspace `Cargo.toml`, then push a matching tag
(`v0.2.0` for `0.2.0`). The Release workflow refuses a tag that does not match. Asset names carry
no version, so the `releases/latest/download/` links always point at the newest release.

## Quick start

What the machine needs:

- **Linux:** the game under Wine or Proton. [mcbe-gdk-linux](https://gitlab.com/veedy-dev/mcbe-gdk-installer)
  sets one up and is what `game_launch` starts by default; a game started by any other launcher
  works too (see [Build](#build)). `ffmpeg` for recordings and `video_analyze`. On Hyprland,
  `hyprctl` and `grim` place, size and capture the window.
- **Windows:** the game installed, and `ffmpeg` on `PATH` for recordings.
- **Optional on both:** Node.js 22 or later for the `docs` tools (Bun also runs `molang_eval` and
  fetches the schemas), and `tesseract` for on-screen text the game's pixel fonts cannot explain.

Add the server to your MCP client with the configuration the installer prints. The server sends
its client a short playbook with the steps below; an agent testing an add-on goes through them:

1. `game_launch` starts the game, or attaches to one that runs.
2. `packs_install` writes the Bridge and GameTest packs into the game's development pack folders.
   The AI only runs it when you agree; it suggests the pack when a test needs it. A new pack needs
   one game restart.
3. `world_open {name}` opens a local world. The world needs cheats on. Without the Bridge pack most
   of a play test still works in single player, through the unstable layer and vanilla commands,
   but inventories, items, blocks and raycasts need it; with your OK the AI adds it with
   `world_packs_set` (`worlds_list` shows a world's packs). See [Packs](#packs).
4. `automation_connect` connects the game's WebSocket, for commands, events and the Bridge pack.
5. `scene_save` names a good spot once; `scene_reset` restores it before each test: place,
   entities, time, weather, inventory, health, slot and perspective in one call.
6. One `action_sequence` with `record: true` runs the test, records exactly that, and returns a
   `summary` per target.

The [prompts](#prompts) package common flows as ready-made instructions.

## Build

Linux:

```bash
scripts/build-windows.sh                 # agent, unstable layer and injector (MinGW, or Docker without it)
cargo build --release -p bedrock-mcp     # the server
```

The server finds the Windows files next to itself, in `target/x86_64-pc-windows-gnu/release`, or
in `BEDROCK_MCP_WINDOWS_DIR`. Run it from your desktop session.

It works on any Linux desktop. On Hyprland it uses `hyprctl` and `grim` to find, size, place and
capture the window, and keeps a hidden game rendering. On other desktops the agent reports the
window size and copies frames for screenshots; resizing and `BEDROCK_MCP_WORKSPACE` only add a note,
scale is 1, and a compositor that throttles hidden windows slows the game.

The game on Linux: the server finds a running game by its process (`Minecraft.Windows.exe` under
Wine) and takes its Wine prefix, Wine build, game folder and data folders from that process, so
`game_launch` attaches to a game started by any launcher. When several games run, it lists them
and `game_launch {pid}` picks one. When none runs, `game_launch` starts one with
`BEDROCK_MCP_LAUNCH_COMMAND` if set, else with mcbe-gdk-linux; without either it asks you to start
the game with your launcher and call `game_launch` again. The agent folder (agent DLLs, endpoint,
screenshots, recordings, timeline) is mcbe-gdk-linux's `profile/agent` for its prefix, and
`${XDG_DATA_HOME:-~/.local/share}/bedrock-mcp/games/<prefix path>/agent` for any other prefix.
`game_stop` uses `mcbe-gdk-linux stop` for its own game and ends other games with SIGTERM.
`game_status` warns (`other_mods`) when DLLs from outside the game, Wine and this server are
loaded, since another mod may hook the same functions as the agent.

Windows:

```powershell
scripts\build-windows.ps1                # server, agent and unstable layer in target\release
```

MCP client configuration:

```json
{ "mcpServers": { "minecraft": { "command": "/path/to/bedrock-mcp/target/release/bedrock-mcp" } } }
```

## Settings

| Variable | Effect |
| --- | --- |
| `BEDROCK_MCP_TOOLSETS` | Comma-separated toolsets to offer, for example `game,player,actions,docs`. Unset offers all of them. An unknown name stops the server with the list of valid ones. |
| `BEDROCK_MCP_WORKSPACE` | Linux: the Hyprland workspace the game window opens on, without switching you to it. |
| `BEDROCK_MCP_UNSTABLE` | `0` keeps the unstable layer from loading. |
| `BEDROCK_MCP_WINDOWS_DIR` | Linux: where the Windows build outputs are. |
| `BEDROCK_MCP_LAUNCH_COMMAND` | Linux: a shell command `game_launch` runs (detached, through `sh -c`, with the game arguments as `$@`) to start the game when none runs, instead of mcbe-gdk-linux. The server then waits for the game process and attaches. Its output goes to `${XDG_DATA_HOME:-~/.local/share}/bedrock-mcp/launch-command.log`. |
| `MCBE_GDK_ROOT`, `BOL_WINEPREFIX` | Linux: where mcbe-gdk-linux is installed (default `${XDG_DATA_HOME:-~/.local/share}/mcbe-gdk-linux`) and its Wine prefix, used when no game runs. |
| `BEDROCK_MCP_HOME` | Windows: the server's folder for the agent, screenshots and recordings. The default is `%LOCALAPPDATA%\bedrock-mcp`. |
| `BEDROCK_MCP_MCT` | Path to a Minecraft Creator Tools `mct` executable for the pack tools. Otherwise the server uses `mct` on `PATH`, then `npx`. |

## Requirements in the game

- **WebSockets:** Settings, General, *Enable WebSockets* on and *Require Encrypted WebSockets*
  off. `game_launch` sets both before it starts the game. When it attaches to a game another
  launcher started, it cannot change them; turn them on in the game's settings yourself. The game
  then shows "Websocket server request rejected" in chat, and `automation_connect` says what to
  turn on.
- **Cheats in the world.** The WebSocket opens when the player runs `/connect`. In a world without
  cheats, `/connect` gets the answer "Cheats aren't enabled in this world." (the game's
  `commands.generic.disabled` text, in the vanilla `en_US.lang` of
  [Mojang/bedrock-samples](https://github.com/Mojang/bedrock-samples)), so commands, events and
  every tool that reads the world are unavailable there. Input, screenshots, OCR and the unstable
  layer still work.
- The server types `/connect` itself the first time a tool needs the WebSocket, or when you call
  `automation_connect`. It closes a pause or chat screen first, and refuses while another screen
  covers the game. The connection stays up across world changes.

## Packs

`mc/bridge/` and `mc/gametest/` are [Regolith](https://regolith-docs.readthedocs.io) projects.

```bash
cd mc/bridge && bun install && rgl run      # exports "bedrock-mcp Bridge"
cd mc/gametest && bun install && rgl run    # exports "bedrock-mcp GameTest"
```

`rgl run build` writes the packs to each project's `build` folder instead. A server built after
that embeds them, and `packs_install` writes the missing or outdated ones and reports them as
`updated`, with `before` saying which they were. `check: true` writes nothing and reports each
pack as missing, outdated or current. A server built without them embeds no packs, and
`packs_install` gives the export commands above. Release builds set `BEDROCK_MCP_REQUIRE_PACKS=1`,
which fails the build without them.

The AI never installs these packs or adds them to a world on its own. The server's instructions,
its prompts and every "pack missing" error tell it to say what the pack adds and ask you first,
and `world_packs_set` is marked destructive, so MCP clients that confirm such tools ask you too.

Minecraft finds new development packs only when it starts, so restart it once after the first
export, then add the packs to a world. After that, `/reload all` applies edits to any pack, the
add-on's and these alike: the game reopens the world with behavior and resource packs, scripts,
models and new `re_mcp:*` commands reloaded, so there is no need to leave and reopen it by hand.
`/reload` only reloads scripts and functions. Script data such as dynamic properties belongs to
each pack, so the bridge cannot read other add-ons' dynamic properties. Scoreboards and tags are
shared.

The GameTest pack uses the beta `@minecraft/server-gametest` module, so the world needs the
**Beta APIs** experiment as well as cheats. Keep it out of worlds that must stay free of
experiments; the bridge pack works on its own.

The bridge pack is optional. Without it, the play-test paths fall back to the unstable layer's
native reads (single player only, since they read the game's own local server) and to vanilla
commands (cheats needed), and every reply says which in `source` and `notes`:

- **Fights:** `attack` confirms a hit when the target's health, read natively, drops, and a kill
  when it reaches 0 or the entity is gone; a miss gets the reason `no_damage`. While an
  `action_sequence` runs, the same reads produce `entity_hurt` and `entity_die` events (source
  `native`), so `until`, `stop_when` and the summary's kills work as with the pack. A health read
  cannot tell who did the damage, and `approach` still needs the pack.
- **Spawning:** `entity_spawn` and `spawn` steps find the new entity's id with `/querytarget`;
  line of sight is unknown and the spot is not checked for solid blocks.
- **The player:** `player_get`, `scene_save` and `game_status` read the feet position, health,
  hunger, saturation, selected slot and effects natively, and the eye, game mode and tags with
  commands. The warm-up reads the slot and position natively and puts the player back with `/tp`,
  and `hotbar_select` presses the slot key and confirms it with the native read. `scene_reset`
  checks the spot with `testforblock` instead of the pack's raycast, so any non-air block counts.
- **Tags, scoreboards, world info, entity properties:** read with `tag … list`, `scoreboard …
  list`, `time query`, `weather query`, `gamerule` and the `has_property` selector. The replies
  are parsed in English (United States); a game in another language fails with that reason.
- **Still needs the pack:** `inventory_get`, `target_get`, `entity_components_get`,
  `block_inspect`, `raycast`, `container_get` and `item_inspect`.

## GameTest on a dedicated server

`gametest_server` runs the add-on's GameTests on a Bedrock Dedicated Server (BDS) without a game
client, so it also works in CI. Download BDS from
[minecraft.net](https://www.minecraft.net/en-us/download/server/bedrock) and unzip it to
`~/.local/share/bedrock-server/current` (`%LOCALAPPDATA%\bedrock-server\current` on Windows), or
point `BEDROCK_MCP_BDS` or the tool's `server` argument at it.

Each call makes a scratch server folder in the agent folder that links the server's files, so the
BDS folder stays as it was. It loads the pack folders you pass, makes a flat world (or copies a
client world with `world`), turns on the Beta APIs experiment, runs your console `commands`, then
runs `/gametest` and returns each test's result, the distinct ERROR lines and the server log.

A test builds its structure from `structures/<namespace>/<name>.mcstructure` in a behavior pack,
or from a structure saved in the world. To save one from the console:

```text
tickingarea add 0 0 0 15 0 15 gt
fill 0 -60 0 4 -56 4 air
structure save demo:walk 0 -60 0 4 -56 4 disk
```

BDS can trail the client's version. A pack whose `min_engine_version` or Script API module
versions the server does not know fails to load; the reply's `errors` and `pack_stack` show it.

## Tools

Every read returns `source` (`bridge`, `command`, `event`, `native`, `gametest` or `input`) and
`notes`, which say why a tool fell back or what it could not confirm. Player and action tools take
`actor`: `self` for the real player, or the name of a GameTest simulated player.

[docs/tools.md](docs/tools.md) lists every tool with its description and arguments. It is
generated from the server's tool definitions with `cargo run -p bedrock-mcp -- docs > docs/tools.md`,
and a test fails when it falls out of date.

| Toolset | Tools |
| --- | --- |
| `game` | `game_launch`, `world_open`, `world_leave`, `server_join`, `game_status`, `hud_get`, `game_stop`, `game_crash` |
| `player` | `player_get`, `player_set`, `inventory_get`, `hotbar_select`, `look_at` |
| `world` | `target_get`, `entities_list`, `entity_inspect`, `entity_components_get`, `entity_properties_get`, `dynamic_properties_get`, `block_inspect`, `raycast`, `container_get`, `item_inspect`, `world_info`, `scoreboard_get` |
| `actions` | `input_send`, `ui_click`, `ui_tree`, `item_use`, `item_use_on`, `attack`, `entity_interact`, `block_break`, `block_place`, `item_drop`, `movement_set`, `entity_spawn`, `action_sequence`, `sequence_status`, `sequence_stop` |
| `gametest` | `gametest_spawn`, `gametest_list`, `gametest_remove`, `gametest_run`, `gametest_server` |
| `automation` | `automation_connect`, `automation_status`, `command_execute`, `chat_send`, `events_list`, `event_subscribe`, `event_unsubscribe`, `events_read`, `wait_until`, `scene_save`, `scene_reset` |
| `media` | `frame_capture`, `video_start`, `video_stop`, `video_analyze`, `timeline_read`, `camera_play`, `camera_get`, `camera_set` |
| `dev` | `content_log_read`, `log_levels_set`, `log_levels_get`, `packs_install`, `packs_global`, `worlds_list`, `world_packs_set`, `skin_install`, `unstable_status`, `player_render_get`, `perf_get` |
| `docs` | `pack_validate`, `pack_info`, `pack_fix_versions`, `schema_list`, `schema_get`, `molang_eval` |

The `docs` tools need no running game.

- `pack_validate`, `pack_info` and `pack_fix_versions` run Mojang's
  [Minecraft Creator Tools](https://github.com/Mojang/minecraft-creator-tools). They need Node 22
  or later. `pack_fix_versions` changes nothing unless `dry_run` is false.
- `schema_list` and `schema_get` read `@minecraft/bedrock-schemas` for the installed game
  version, cached after the first download.
- `molang_eval` evaluates an expression with [MolangJS](https://github.com/JannisX11/MolangJS). It
  does not validate: unknown names count as 0 and malformed syntax gives a number. The reply lists
  the names you did not supply. The game reports real Molang errors in the content log.

### Prompts

The server offers MCP prompts: flows with every tool call spelled out, which a client lists with
`prompts/list` (many show them as slash commands). [docs/tools.md](docs/tools.md#prompts) lists
their arguments.

| Prompt | Arguments | Flow |
| --- | --- | --- |
| `playtest_item` | `item`, `world`; optional `target`, `distance`, `spot` | Tests an item against a spawned entity in one recorded `action_sequence` and checks the summary. |
| `record_combat` | the same | Records a clean third-person clip of the same fight and checks it with `video_analyze`. |
| `debug_pack_errors` | `pack`, `world` | Groups the content log's errors, validates the pack, checks the world's packs, reloads and reads the log again. |
| `test_ui_flow` | `clicks`; optional `expect` | Clicks labels in order (`Settings > Video`) through menus or add-on forms and checks each screen. |

### Worlds and menus

- `world_open` opens a local world by the exact name the world list shows, or by its folder id,
  with mouse clicks. It selects the Worlds tab, scrolls and turns the list's pages until the card
  shows, and waits until the player is in the world. It opens only local worlds: when a click
  starts joining another host's game, it cancels the join and fails. `world_leave` clicks Save &
  Quit and waits for the main menu.
- `worlds_list` shows every world with its packs. `world_packs_set` sets a closed world's packs by
  uuid, folder or pack name.
- `ui_click` clicks a button by its label. It looks in the JSON UI control tree first, then on the
  Ore UI page, then reads the screen with OCR, and scrolls a list to find the button. With OCR it
  also matches labels read with letters missing, such as the death screen's large Respawn button.
- `game_status.attention` names a screen that stops play, the death screen or a disconnect
  screen, and the next step to take. `game_status.warnings` also lists modes that change how
  tools behave: the unstable layer limited to string anchors, disabled or failed, capabilities
  that do not work, a world without the WebSocket, and a screen over gameplay.
- `scene_save` names the spot the player stands on in the open world. `scene_reset` returns to
  it in one call and also clears entities, sets time, weather, difficulty, game mode and effects,
  refills the inventory, heals, selects a slot and sets the perspective. It reports each
  command, the player afterwards, and whether a block is in the way.
- `content_log_read` with `summary: true` groups errors by cause. Hundreds of errors from one
  broken pack usually come down to a few patterns.

### Actions

Each action reports `confirmed` and the evidence for it.

- **The real player (`actor: self`).** The action goes through the agent's input:
  1. The player turns to the target, using the bridge for an exact look or `/tp` with cheats.
  2. The server checks the crosshair.
  3. It presses the key and waits for the matching event from that player.
- **A simulated player.** The action calls the GameTest method and reads the result.
  `SimulatedPlayer.attackEntity` works at any distance (the
  [SimulatedPlayer reference](https://learn.microsoft.com/en-us/minecraft/creator/scriptapi/minecraft/server-gametest/simulatedplayer)),
  so the server checks reach itself: a simulated player attacks within 3 blocks in survival and 5
  in creative, the Bedrock entity reach on the
  [Minecraft Wiki](https://minecraft.wiki/w/Interaction_range), and anything farther is refused
  with a note.
- **A whole fight in one call.** `action_sequence` runs spawn, hotbar, look, attack and use steps
  on game ticks. A `spawn` step with `as: "A"` lets later steps name the new entity as `"$A"`.
  `record: true` records from the first step to the last. The reply ends with a `summary` per
  target: hits, misses, the run that killed it, how it ended, and why unconfirmed steps failed.
  A call that runs longer than `blocking_limit_ms` (25 s by default) answers with its `run_id`
  and keeps running; `sequence_status` returns the result later.
- **Warm-up.** Right after a world loads, the held item's attachable can stay frozen until the
  slot changes and the player moves. `scene_reset` ends with a warm-up: another hotbar slot and
  back, a block forward and back, and back on the spot. `action_sequence` runs the same warm-up
  before a recording, and before its first run in a newly loaded world (`warm_up: "auto"`). With
  the `player_render` capability, the warm-up checks that the attachable animates and repeats
  once when it does not.

### Timeline and recordings

- The server writes one JSON line per tool call to `timeline/<session start>.jsonl` in the agent
  folder: the tool, a short form of its arguments, when it started, how long it took, whether it
  failed (with the first line of the error), the game tick its reply reported and where it starts
  in a running recording. `timeline_read` returns the recent entries; `game_status.timeline`
  names the file.
- While a recording runs, every tool call becomes a chapter in the MP4, such as
  `attack -326417514479 (tick 445260)`. `video_stop` also writes them to
  `<video>.chapters.json`. Code inside the server can add its own with `Session::mark`.
- `video_start` with `pause_on_idle_ms` leaves out the time between calls: once no call has run
  for that long, the video and its audio are cut until the next call starts. A running call or
  background `action_sequence` never pauses it. `video_stop` lists the cuts.

## Unstable layer

`bedrock_unstable.dll` reads game internals through the game's own vtables. It covers what
the stable tools can only approximate. The server stages and injects it like the agent, once the
agent is attached and a tool first wants it. Its capabilities:

- `content_log`: content-log entries as the game emits them.
- `screens`: the live screen stacks. If a read's names disagree with the stack's size, the layer
  keeps the last good read and retries on the next update; only 30 bad reads in a row fail it.
- `ui_tree`: the control tree of the JSON UI screen on top of each stack, with each control's
  name, rectangle, components, text and toggle state. The game thread reads it on the next frame,
  only when a tool asks or the top screen changes. `ui_tree` returns it, and `ui_click` clicks a
  button through it before it tries OCR, once the whole button, or all but a quarter of its
  height, lies inside its list's visible part.
- `camera`: the view perspective and field of view from the game's camera API, and the
  render camera's direction (yaw and pitch). `camera_get` returns it, `game_status` shows it under
  `unstable.camera`, and `camera_set` presses the perspective key until the camera reads the
  wanted perspective.
- `hud_text`: toasts as the game queues them, the action bar, popup, tip and chat lines, and
  whether the player is dead (from the camera's damage parameters) with the death message.
  `hud_get` returns it, and `game_status.attention` reports a death from it before it asks the
  Bridge pack.
- `oreui`: the elements of the pages that Ore UI (Coherent Gameface) draws, such as the world list,
  settings and add-on forms on builds that draw them with Ore UI. When a tool asks, the DLL runs a
  fixed, read-only script on each live page on its next frame. The script collects the elements a
  player can focus or click, elements labelled for tests, scrolling lists and text, with their
  rectangles, and prints the result to the page's console, where the DLL picks it up from the
  game's Gameface log handler. A probe script confirms that path before the capability reports
  `active`. `ui_tree` shows these pages in the same shape as JSON UI screens (`source: oreui`), and
  `ui_click` clicks and scrolls through them before it tries OCR. The script never writes to the
  page and bounds what it scans, keeps and prints.
- `player_render`: the local player's animation components with their attachables, as
  `player_render_get` returns them: each controller's state and state time, the animations
  playing, and when the game last applied each component. Play-test advice: in case a held
  item's attachable has no animations yet right after a world loads, the warm-up in
  `scene_reset` and `action_sequence` switches the hotbar slot away and back and checks the
  attachable with `player_render_get`.
- `perf`: frame timings from `MinecraftGame::update` and the game's own update duration, plus
  the cadence of the UI scene stack, as `perf_get` returns them with percentiles.
- `world_state`: the entities on the game's own local server and every pack's dynamic properties.
  `entities_list` and `entity_inspect` read entities through it (`source: "native"`, no cheats)
  when the Bridge pack does not answer, and `dynamic_properties_get` always uses it. It checks the
  entity pool's layout against several real actors first, stays pending while a world is still
  loading, and keeps checking after a failure, so it turns active once the world settles. A game
  joined to someone else's server runs no local server, so it has nothing to read there. Only
  `ServerLevel` is found by its vtable; the layer recognizes the rest by content: `Minecraft` by
  its reference back to the server's `GameCallbacks`, the entity pools by the component name and
  hash in each pool's EnTT type descriptor, and `MobEvents` by its events' names.

It loads on any game build and uses no hard-coded addresses. It finds each class one of two ways:

- **By RTTI.** On a build that ships RTTI, a pointer to each class's type information sits before
  its vtable, so the layer finds classes by name. A game update usually needs no remapping.
- **By string anchors.** A build without RTTI keeps no class names, so the layer falls back to the
  rules in `crates/rtti/src/anchors/rules.rs` and `rules/<capability>.rs` (the `anchor-rules`
  skill in `.agents/skills/` explains how to write one). A rule names something one of the class's functions
  uses: a string literal, a `std::function` lambda declared in one of its methods (the compiler
  keeps each lambda's type name, which spells out the method), or a function the game imports by
  name from another DLL, such as Gameface's. The exception directory gives each
  function's extent. The vtable is the run of code pointers in read-only data that holds that
  function (or one that calls it, where a game version moves the use into a helper) at the
  expected slot, or at any slot for a class whose virtuals move, and that code stores into an
  object at the expected offset. It ends where code loads the next word as another vtable, or
  where the code pointers end, and must have exactly the expected number of slots, or at least
  that many for a class that may gain virtuals between game versions. A rule that matches no
  vtable, or more than one, leaves its class unavailable. The anchors resolve once at load, in
  under a second, and only when RTTI lacks one of their classes.
  `cargo run --release -p rtti --example anchors -- <exe>...` checks every rule against RTTI in
  an executable that has it.

Three classes may change their virtuals and members between game versions, so the layer finds
what it uses in them by its shape instead of a fixed slot or offset:

- `ClientInstance` (`camera`): in LeviLamina's header `getCamera` is followed by
  `getShaderColor`. The layer assumes both are one-instruction getters and that the shader colour
  follows the camera, and accepts only one adjacent pair of getters whose offsets lie exactly the
  camera's 512 bytes apart, decoded at runtime.
- `MinecraftGame` (`perf`): `getGameUpdateDurationInSeconds` is the interface's last virtual, so
  the layer takes the vtable's last slot. It decodes the function first and calls it only if it
  loads a double from the object and returns.
- `OreUI::View` (`oreui`): its `cohtml::View*` is the first member after the listener subobject
  in LeviLamina's header. The layer takes the first pointer in the next 32 bytes whose object has
  a vtable outside the game's image (Gameface is its own module) with at least 69 code slots.

`unstable_status` says how each capability found its classes: `found by RTTI` or `found by string
anchors`. Set `BEDROCK_MCP_UNSTABLE_ANCHORS_ONLY=1` in the game's environment to make the layer
ignore RTTI, which tests the anchors on a build that ships RTTI. A game that `game_launch` starts
inherits it from the server's environment.

Through anchors, `content_log`, `screens`, `ui_tree`, `camera`, `hud_text`, `perf`,
`player_render`, `oreui` and `world_state` work without RTTI. `world_state` anchors `ServerLevel`
and recognizes the rest of its path by content: EnTT's pools by their component type names, the
other objects by their links and layouts. `ui_tree` then names only text,
toggle, button and scroll view components; others read as `unknown`, so text boxes, sliders and
dropdowns lose their kind.
When a build's vtable has a slot count that a capability does not accept, the capability reports
`unavailable` with the count the anchors found.

Each capability does two checks before it reports `active`: it checks its vtable layout, then
confirms itself with a live call. Capabilities succeed or fail on their own. One that cannot
resolve on a build reports `unavailable` or `failed` with the reason, and only the tools that need
it fall back.

`unstable_status` reports the load result and each capability's state.

It never breaks the stable tools. A missing DLL, a failed injection or a dropped connection makes
them fall back to the WebSocket, OCR and the content log file, with a note that gives the reason.
Screen waits need the `screens` capability; without it, `wait_until` fails at once and says why.

## Troubleshooting

Start with `game_status`: `attention` names a screen that stops play and the next step, and
`warnings` lists modes that change how tools behave. `timeline_read` shows the recent tool calls
with their errors.

| Symptom | Cause and fix |
| --- | --- |
| `automation_connect` fails and chat says "Websocket server request rejected" | WebSockets are off. Turn them on in Settings, General, or let `game_launch` start the game. |
| Chat says "Cheats aren't enabled in this world." | `/connect` gets this answer in a world without cheats. Turn them on in the world's settings. |
| `re_mcp:*` commands are unknown | The Bridge pack is missing from the game or the world. Run `packs_install`, restart the game once, add the pack with `world_packs_set`, then open the world. |
| A capability in `unstable_status` is `unavailable` or `failed` | Its `detail` gives the reason; `bedrock-unstable.log` in the agent folder has more. After a game update, see the `update-game-version` skill in `.agents/skills/`. `BEDROCK_MCP_UNSTABLE=0` turns the layer off. |
| `game_status` warns `other_mods` | Another mod is loaded in the game and may hook the same functions. Launch the game without it. |
| `game_launch` cannot inject | On Linux, `injector.log` in the agent folder names the step that failed. |

The agent folder holds the DLLs' logs (`bedrock-agent.log`, `bedrock-unstable.log`), screenshots,
recordings and the timeline. On Windows it is `%LOCALAPPDATA%\bedrock-mcp\agent` (or `agent` in
`BEDROCK_MCP_HOME`); on Linux see [Build](#build).

## Limits

- **Hidden windows.** `frame_capture` reads the window's own buffer, so it works on a hidden
  workspace or under other windows. Recordings come from inside the game, so they work hidden too.
- **Recording on Linux.** `ffmpeg` encodes with VAAPI and copies each frame through memory, about
  1.6 GB/s at 2522×1360 and 120 fps. `encoder_behind_frames` counts frames the recorder could not
  keep up with. A game stall appears as repeated frames.
- **Recording on Windows.** The server picks the first `ffmpeg` encoder that works: NVENC, AMF,
  Quick Sync, Media Foundation, then x264. Audio records the default output device.
- **OCR.** It reads the game's pixel fonts (Mojangles and the OreUI Minecraft fonts). It does not
  read world signs, icons, textures or vector-font text such as Noto and non-Latin scripts.
- **Background play.** While the MCP is connected, the agent keeps the game active in the
  background:
  - It withholds the window's focus-loss messages.
  - It answers the game's `GetForegroundWindow` and `GetFocus` with the game's own window.
  - It drops `ClipCursor` requests while another window is in front.

  It never moves the real focus or your cursor.
- **Hidden-window frame rate on Hyprland.** `game_launch` sets `render_unfocused` on the window.
  Hyprland then renders the hidden game at `misc:render_unfocused_fps` (15 by default, per the
  [Hyprland wiki](https://wiki.hypr.land/configuring/core/config-options/)). Without it, a hidden
  game renders very slowly.
- **Windows is unverified on real hardware.** The Windows build compiles and passes its tests under
  Wine. Launching, window handling, injection, encoding and audio capture have not run on a real
  Windows machine yet.

## Contributing

[AGENTS.md](AGENTS.md) is the contributor guide, for people and coding agents alike: the layout,
the checks to run, how to write tools, the rules for the unstable layer and what must never go
into the repository. The skills in `.agents/skills/` cover writing anchor rules and moving to a
new game version. Report security problems privately, as [SECURITY.md](SECURITY.md) describes.

## Credits

- [LeviLamina](https://github.com/LiteLDev/LeviLamina) by LeviMC (LGPL-3.0): the class layouts
  the unstable layer reads (content log, screens, UI controls, camera, HUD text, animation, Ore UI,
  and the local server's entities and player). Used as reference only: no LeviLamina code is
  included, each layout is checked on the running game, and the code comments name each header.
- [EnTT](https://github.com/skypjack/entt) by Michele Caini (MIT): the layout of the game's entity
  component pools.
- [Coherent Gameface API reference](https://docs.coherent-labs.com/cpp-gameface/api_reference/classes/classcohtml_1_1_view/):
  the order of the Ore UI view's methods.
- [bedrock-samples](https://github.com/Mojang/bedrock-samples) by Mojang: the vanilla command
  replies, command syntax and effect ids the Bridge-free fallbacks parse.
- [MolangJS](https://github.com/JannisX11/MolangJS) by JannisX11 (MIT): runs `molang_eval`.
- [Minecraft Creator Tools](https://github.com/Mojang/minecraft-creator-tools) by Mojang: runs
  `pack_validate`, `pack_info` and `pack_fix_versions`.
- [`@minecraft/bedrock-schemas`](https://github.com/Mojang/bedrock-schemas) by Mojang (MIT): the
  schemas for `schema_list` and `schema_get`.

The `docs` tools download MolangJS, Creator Tools and the schemas into the server's cache on first
use; the releases do not bundle them.

## License

MIT; see [LICENSE](LICENSE).
