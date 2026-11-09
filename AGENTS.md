# Instructions for agents

Bedrock MCP for Minecraft (`bedrock-mcp`) is a stdio MCP server that lets AI agents develop and test
Minecraft Bedrock add-ons: it launches the game, plays it with real input, reads game state, runs
commands and GameTests, and records what happened. Read `README.md` first; this file covers how to
work on the code.

## Layout

| Path | What it is |
| --- | --- |
| `crates/mcp` | The server (`bedrock-mcp`). `src/tools/*.rs` declares every tool, one module per toolset; `src/session/*.rs` implements them; `src/drivers/` runs gameplay actions and `action_sequence`; `src/rpc.rs` is the MCP request loop and the instructions sent to clients; `src/prompts.rs` holds the MCP prompts; `src/platform/` finds, launches and injects the game on each OS. |
| `crates/agent` | `bedrock_agent.dll`, loaded into the game: input, focus, frame capture. Hooks GameInput, DXGI, D3D12 and `user32` imports, never game code. |
| `crates/unstable` | `bedrock_unstable.dll`, optional: reads game internals through vtables found by RTTI class name or by string anchors. |
| `crates/injector` | Loads a DLL into the running game. |
| `crates/protocol`, `crates/endpoint` | JSON-lines messages between the server and the DLLs, and the authenticated local endpoint they use. |
| `crates/rtti` | Finds vtables in a PE image by RTTI name, and by string anchors where RTTI is missing (`src/anchors/rules.rs` and `rules/<capability>.rs`). |
| `crates/ocr` | Reads on-screen text by matching the game's own pixel fonts. |
| `mc/bridge`, `mc/gametest` | Behavior packs (Regolith projects) whose `re_mcp:*` commands answer the server with JSON. |
| `docs/tools.md` | Tool reference, generated from the tool definitions. |
| `.agents/skills/` | Procedures for coding agents: `anchor-rules` and `update-game-version`. |

## Build and check

```bash
cargo build -p bedrock-mcp                      # the server (debug builds are fast)
scripts/build-windows.sh                        # Linux: the Windows DLLs and injector (MinGW, or Docker)
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p bedrock-mcp -p protocol -p rtti -p ocr
cargo run -q -p bedrock-mcp -- docs > docs/tools.md
cd mc/bridge && bun install && bun run typecheck   # same for mc/gametest
```

- Run clippy for the Windows target too when you touch `cfg(windows)` code or the DLL crates
  (`--target x86_64-pc-windows-gnu`; `scripts/windows-build.Dockerfile` has a toolchain). The
  unstable crate's tests run only on Windows (CI runs them; on Linux, under Wine).
- `docs/tools.md` must match the code; a test fails when it is stale. Regenerate it whenever a
  tool's name, description or schema changes.
- Changes to `scripts/install.sh` or `install.ps1`: run `scripts/test-install.sh target/debug/bedrock-mcp`
  (Linux) or `scripts/test-install.ps1` (Windows).
- `cargo fmt --all` before you finish.

## Writing tools

- Declare the tool in the right `src/tools/<toolset>.rs` with honest hints (`READ`, `ACT`, `SET`,
  `DESTROY`), and implement it as a `Session` method.
- Write every description in this shape, with real line breaks (`\n`), because clients show it
  to their model and `docs/tools.md` renders it as Markdown:
  1. One sentence saying when to use the tool: `Use this to ...` or `Use this when ...`.
  2. A blank line, then a short paragraph (two or three sentences) on what it does.
  3. A blank line, then `- ` bullets, one fact each: how it decides, what it falls back to,
     limits, when it fails. Leave out anything an argument's own description already says.
  4. A blank line, then `Returns {...}.` naming the reply's fields, with a word on any that are
     not obvious.

  Describe every argument in its schema, in one line. Say each fact once.
- Replies report `source` and `notes`: where the data came from, why a tool fell back, and what it
  could not confirm. Never fall back silently between drivers or sources.
- Actions report `confirmed` only when the game showed the result (an event or a readback).
- Keep tool calls bounded: every wait has a timeout and a cap.

## The unstable layer

Two skills in `.agents/skills/` cover the procedures: `anchor-rules` for adding or fixing the
string anchors that find classes on builds without RTTI, and `update-game-version` for moving to
a new Minecraft release. Read the matching one before you change `crates/unstable` or
`crates/rtti`.

It runs inside the game, so a mistake crashes the player's game. Every capability:

- finds classes by RTTI name or by a string anchor rule, never by hard-coded addresses or byte
  signatures, and checks the vtable's slot count before using it;
- hooks vtable slots only and never patches code;
- reads on the thread that owns the data, bounds every loop and string, checks pointers are
  readable before reading, and never writes or frees game memory;
- reports `pending` until a live call confirms it, and `unavailable` or `failed` with the reason
  when a check fails, without affecting other capabilities or the stable tools.

Cite the public header or document a layout comes from (for example a LeviLamina header) in a
comment next to the code. The README's Credits list only the projects, with one line each.

## What must not go into the repository

- **No game binaries, game data or captures.** No game files, extracted string or symbol dumps,
  headers generated from a local build, screenshots, recordings, OCR fixtures or logs. OCR fixtures
  live outside the repo (`~/.cache/bedrock-mcp/ocr-fixtures`, or `$OCR_FIXTURES`).
- **Only public game versions.** Write a version number only when that exact version shipped to
  the public. Never write build numbers, PE timestamps, image sizes, addresses or other
  fingerprints of a game executable. Tests use made-up values such as `1.0.0.0` and `0x1234_5678`.
- **Describe builds by what they do** ("builds that ship RTTI", "a build without the class"), not
  by channel or audience.
- **No personal or private details:** no machine paths, user names, gamertags, private add-on or
  project names, or credentials. Tests use neutral names (`Demo BP`, `Bob`).
- Anchor rules may contain only the strings and method names they need, and only ones present in
  public game builds.

## Testing against the game

- Most logic has fake-game tests (`drivers/tests.rs`, `session/tests.rs`, the fake unstable DLL in
  `src/unstable/fake.rs`). Add tests for behaviour an agent depends on, not for wording.
- Live checks need the game: on Linux under Wine or Proton from any launcher (`game_launch`
  attaches to it; mcbe-gdk-linux is the default launcher), on Windows the installed game. Only one
  client should drive the game at a time. `game_launch` waits 5 s after the game exits before it
  launches it again, because a launch right after an exit can fail; leave the same gap when you
  launch the game yourself.
- A new DLL needs a game restart. `unstable_status` shows each capability's state and how its
  classes were found; `BEDROCK_MCP_UNSTABLE_ANCHORS_ONLY=1` in the game's environment (a game
  `game_launch` starts inherits the server's) forces the string-anchor path.

## Style

- Rust: clear names, small functions, comments that explain why. No dead code, no shims.
- Prose in docs, descriptions and notes: plain words, active voice, short sentences, no filler.
- Commit messages say what changed and why.
