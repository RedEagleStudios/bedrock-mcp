---
name: update-game-version
description: Use when a new public Minecraft Bedrock release ships, when LeviLamina moves to a new game version, or when unstable_status reports capabilities unavailable or failed on a new build. Walks the update from headers to a live check.
---

# Updating to a new Minecraft version

The stable tools (input, WebSocket, Bridge and GameTest packs, OCR) rarely need changes for a new
release. The unstable layer (`crates/unstable`, `crates/rtti`) reads game internals, so it needs a
check on every release. Work through the steps in order and stop at the first one that fails.

## Ground rules

- Every fact you write comes from a public source: LeviLamina's headers for the new version, the
  public game executable and its data files, EnTT, Coherent Gameface's docs, Mojang's public
  samples, or Microsoft Learn. Never copy a name, string, slot, offset, size or version from a
  build that is not public. Read `AGENTS.md` "What must not go into the repository" first.
- Keep the public executable and game files outside the repository. Write addresses as `<addr>`
  in notes and commit messages.
- One game is shared on a test machine. Live checks run one at a time.

## 1. Gather the references

1. Check out LeviLamina (https://github.com/LiteLDev/LeviLamina) at the tag or commit for the new
   version. `tooth.json` names the game version it targets.
2. Get the new public game installed with a launcher that leaves a decrypted
   `Minecraft.Windows.exe` on disk. Note the path; it is your public reference executable.
3. Note the public data folder next to it (`data/gui/dist/hbui` holds the Ore UI scripts,
   `data/resource_packs` the JSON UI and texts).

## 2. Resolve the anchor rules

```bash
cargo run --release -p rtti --example anchors -- <public Minecraft.Windows.exe>
```

Every rule must print `unique` (or `unique family` for rules with `shared`). For each
`missing` or `ambiguous` line, follow the `anchor-rules` skill. Do not change a slot count to
whatever the new build has without checking LeviLamina's header first: an `Exact` count that
LeviLamina also changed is an update; one that only the build changed means the class gained
virtuals, so prefer `AtLeast` and let the capability find what it uses.

If a build with RTTI is available to you, run the example on it too: every rule must print
`matched RTTI`. That proves each anchor found the right class. It is a check only; nothing from
that build goes into the repository.

## 3. Re-check what the capabilities read

For each capability file in `crates/unstable/src`, compare every constant it uses with the new
LeviLamina headers. The file's top comment lists the headers it follows.

| File | What to compare |
| --- | --- |
| `content_log.rs` | `ContentLogEndPoint`, `LogEndPoint` slots; `LogLevel` and `LogArea` values |
| `screens.rs` | `ISceneStack` slot indices and count |
| `ui_tree.rs` | `UIScene`/`BaseScreen`/`AbstractScene` counts; `ScreenView`, `VisualTree`, `UIControl`, `UIComponent`, `TextComponent`, `ToggleComponent` member offsets |
| `camera.rs` | `IVanillaCameraAPI`, `ICameraAPI` slots; `mce::Camera` layout and size; `PlayerViewMode` values |
| `hud.rs` | `IToastManager` slots; `ToastMessage`, `GuiData`, `GuiMessage`, `TitleMessage` offsets; enum ranges |
| `perf.rs` | `IMinecraftGame::update` slot; the getter is the interface's last virtual |
| `oreui.rs` | `IView` slots 4, 13, 17 and count; `IViewListener` slots; `ViewState` values; Gameface `View` method order from the Gameface docs |
| `player_render.rs` | `AnimationComponent` slots and member offsets; controller and state player offsets; `AnimationComponentGroupType` values |
| `world_state.rs` | `GameCallbacks` slots; `Actor`, `ServerLevel`, `DynamicPropertiesManager`, component sizes and offsets; EnTT version and page size from LeviLamina's `xmake.lua` |

Offsets in LeviLamina are not written out: sum the `TypedStorage<align, size, T>` and
`UntypedStorage<align, size>` members in declaration order, after the base classes and the
vtable pointer, respecting alignment. Slots: count `virtual` declarations base class first,
destructor at 0; an override keeps its base slot. When a header and the code disagree, change
the code to follow the header, or make the code find the value at runtime with a check, and
update the comment that cites the header.

## 4. Re-check the game strings the server matches

`crates/mcp` matches screen names, loading jobs, Ore UI control ids, options keys, command
output and labels. For each literal in `crates/mcp/src/unstable/live.rs`,
`crates/mcp/src/session/worlds.rs`, `crates/mcp/src/session/ui.rs` and the option handling
(`crates/mcp/src/game_files.rs`, `keys.rs`),
confirm it still exists in a public source:

```bash
grep -c -a -F -- "<string>" <public Minecraft.Windows.exe>
grep -rl -F -- "<string>" <public data folder>
```

Remove or replace any string that is gone. Labels come from the public `en_US.lang`.

## 5. Update versions the repository pins

- `mc/bridge` and `mc/gametest`: the `@minecraft/server` and `@minecraft/server-gametest`
  versions in `package.json` and `packs/BP/manifest.json`, and `min_engine_version`. Run
  `bun install` and `bun run typecheck` in both, then `rgl run build`.
- `crates/mcp/src/docs/mct.rs` and `molang.rs` pin package versions, and `schemas.rs` pins the
  rockide/schemas release (`PINNED`); bump each only when the new release needs it, such as a
  new `format_version` the schemas must know.
- Cite any new LeviLamina header in a code comment. README Credits changes only for a new project.

## 6. Check live

1. Build the DLLs (`scripts/build-windows.sh`) and the server.
2. Restart the game with `BEDROCK_MCP_UNSTABLE_ANCHORS_ONLY=1` in the server's environment so
   the layer ignores RTTI and takes the path public builds take.
3. Open a world. `unstable_status` must show every capability `active`. A capability that
   reports `unavailable` or `failed` gives its reason: fix that before moving on.
4. Exercise each one: `camera_get`, `hud_get`, `perf_get`, `ui_tree`, `ui_click` on an Ore UI
   page, `player_render_get`, `entities_list` (source `native`) and `dynamic_properties_get`,
   `content_log_read`, and `world_open`/`world_leave`.
5. Run the full checks from `AGENTS.md`, regenerate `docs/tools.md`, and scan the change for
   anything copied from a build that is not public.
