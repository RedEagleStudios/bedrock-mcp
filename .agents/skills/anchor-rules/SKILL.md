---
name: anchor-rules
description: Use when adding, fixing or reviewing a string-anchor rule in crates/rtti/src/anchors (a class the unstable layer needs on builds without RTTI), or when the anchors example prints missing or ambiguous, or a capability reports that string anchors did not find its class.
---

# Anchor rules

Public game builds ship without the game's RTTI, so the unstable layer cannot find a class by
name there. An anchor rule finds the class's vtable through things its code still uses: string
literals, the type names of `std::function` lambdas, and imported functions. Read the module
comment at the top of `crates/rtti/src/anchors.rs` first; it explains how vtables and function
extents are found.

## Where things live

- `crates/rtti/src/anchors.rs`: the resolver, `Source`, `Anchor` and `Slots`.
- `crates/rtti/src/anchors/rules.rs`: the shared rule list (`RULES`) and the rules for the
  capabilities that predate the per-capability files.
- `crates/rtti/src/anchors/rules/<capability>.rs`: one file per newer capability.
- `crates/unstable/src/vtable.rs`: `Resolver::table` and `table_at_least` use RTTI when the build
  has it and fall back to the rules otherwise.
- `crates/rtti/examples/anchors.rs`: resolves every rule against an executable.

## What a rule may use

Every part of a rule must hold on public builds and come from a public source:

- **Class name:** a class LeviLamina's headers declare, by LeviLamina's name.
- **Slot numbers and counts:** from LeviLamina's headers. Count `virtual` declarations base
  class first, destructor at 0; an override keeps its base slot.
- **`Literal`:** a string that exists in the public executable:
  `grep -c -a -F -- "<string>" <public Minecraft.Windows.exe>` must be at least 1.
- **`Lambda("Class::method")`:** a method LeviLamina's headers list for that class.
- **`Import("<decorated name>")`:** a function the game imports by name from a DLL it ships
  (for example Gameface's exports), visible in the public executable's import table.

Never take a string, slot, count or name from a build that is not public, even when it would
make a rule easier. A build with RTTI is only for checking that a rule found the right class.

## Sources and anchors

`Source` says what code uses:

| Source | Matches code that |
| --- | --- |
| `Literal(s)` | loads the address of the NUL-terminated string `s` |
| `Lambda("Class::method")` | builds a `std::function` from a lambda declared in that method |
| `Import(name)` | calls the imported function through its import address table slot |

`Anchor` says how the source ties to the vtable. Every anchor of a rule must hold; the first one
picks the candidates, so put the most selective first.

| Anchor | Holds when | Example |
| --- | --- | --- |
| `Slot { slot, source }` | the function in `slot` uses `source` | `ClientAnimationComponent` slot 4, a lambda in `ensureClientAnimationComponentIsInitialized` |
| `Calls { slot, source, depth }` | the function in `slot` uses `source` itself or in a function it reaches within `depth` direct calls | `ActorAnimationControllerStatePlayer` slot 1, whose error string can sit in a helper |
| `Holds { source }` | some slot uses `source`; for classes that may gain virtuals, where the slot moves | `ServerLevel`, by `ServerLevel::_subTick` and its profiler scope |
| `Stored { source, depth }` | a function that uses `source` (or one it reaches) stores the vtable at the rule's offset: a constructor | `OreUI::EmptyLogHandler`, whose constructor calls Gameface's exported base constructor |
| `Sibling { class, offset }` | every store of an earlier rule's vtable stores this one next to it through the same register: two subobjects of one object | `ServerInstance` +0x38 beside +0x18 |
| `After { class, offset }` | this vtable starts right after an earlier rule's | `VanillaCameraAPI` after `CameraAPI` |
| `Beside { offset, slots }` | every store of this vtable also stores a run of `slots` code pointers at `offset`: a base class there | `GuiData` with its 21-slot listener at +8 |

`Slots::Exact(n)` requires exactly `n` slots; use it when LeviLamina's count and public builds
agree and the class rarely changes. `Slots::AtLeast(n)` accepts more: the vtable then ends where
code loads the next one. Use it for big interfaces that may gain virtuals between game versions
(`MinecraftGame`, `ClientInstance`, `OreUI::View`, `ServerLevel`), and make the capability find
what it calls inside the table (decode the function, or scan for a shape) instead of trusting a
fixed slot.

`shared: Some(slot)` lets wrapper templates that reuse the class's function in `slot` resolve as
one family instead of being ambiguous (`ContentLogFileEndPoint`).

## Writing or fixing a rule

1. Read the class in LeviLamina: its bases, virtuals and slot count. Pick the slots whose
   functions are most likely to keep a distinctive string, lambda or import.
2. Find a candidate source in the public executable. Error and log strings, profiler scope names
   and Ore UI routes are good; short or generic strings are not.
3. Add the rule to the capability's file under `rules/`. Write a comment that says which header
   gives the slots and why the source belongs to that method.
4. Resolve it:

   ```bash
   cargo run --release -p rtti --example anchors -- <public Minecraft.Windows.exe>
   cargo run --release -p rtti --example anchors -- <a build with RTTI, if you have one>
   ```

   The public build must print `unique` for the rule. A build with RTTI must print
   `matched RTTI`. `ambiguous` means add a second anchor (another slot, a `Calls`, or a
   `Sibling`); `missing` gives the reason, such as a slot count mismatch.
5. Wire the capability: look the class up with `Resolver::table` (exact) or `table_at_least`,
   keep the capability's own checks (slot counts, a live confirmation call), and report
   `unavailable` with the reason when the rule does not resolve.
6. Test live with `BEDROCK_MCP_UNSTABLE_ANCHORS_ONLY=1` in the server's environment, so the
   layer ignores RTTI. `unstable_status` shows `found by string anchors` for the capability.
7. Update README "Unstable layer" (which capabilities work without RTTI). Cite any new LeviLamina
   header in a code comment; Credits changes only for a new project.

## When no anchor exists

Some classes keep no string, lambda or import of their own, such as EnTT's storage templates.
Identify them from the data at runtime instead: follow pointers from an object an anchor did find
and validate what you reach against LeviLamina's layout (sizes, back-pointers, value ranges), as
`world_state.rs` does for entity pools and component layouts. Every guess needs a check that
rejects a wrong answer; a capability that cannot confirm its data reports `pending` or
`unavailable`, never a guess.
