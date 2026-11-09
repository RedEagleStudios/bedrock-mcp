//! The `actions` toolset: player input and gameplay actions. Gameplay actions run through a
//! driver (`crate::drivers`): synthetic input for the connected player, GameTest method calls
//! for a simulated player.

use serde_json::{Value, json};

use super::{
    Handler, Hints, Tool, Toolset, arguments,
    player::vector_schema,
    reply,
    schema::{describe, object, region},
};
use crate::drivers::{self, ACTIONS};

/// The bullets and `Returns` line every driver action shares; tools end their own bullets
/// with it.
const OUTCOME: &str = concat!(
    "- driver is input for actor self: synthetic input that aims with the Bridge pack's exact look, or /tp in a world with cheats, and checks the crosshair before clicking. It is gametest for a simulated player: SimulatedPlayer calls through the GameTest pack.\n",
    "- confirmed is true only when the game showed the result: a Bridge pack event by this actor (evidence.event, with its tick), a legacy WebSocket event without the pack, or a readback (source names it).\n",
    "\n",
    "Returns {actor, driver, confirmed, source, evidence, tick, notes, reason}. notes say why an action is unconfirmed (a blocked crosshair, an event by someone else, no way to aim); reason {class, detail} names a known cause: out_of_reach, blocked, cooldown, no_item, projectile_in_flight, no_damage, no_event or error."
);

const INPUT_SEND: &str = concat!(
    "Use this to send raw keys, mouse moves and menu clicks that no gameplay tool covers.\n",
    "\n",
    "Sends synthetic keyboard and mouse input through the agent injected into Minecraft (key messages and GameInput mouse readings), so bindings, menus and add-ons see real input. The window may be hidden or covered, and the real cursor stays free. To click a button by its label, ui_click is shorter.\n",
    "\n",
    "- Order: release_all, pointer/cursor, release, move/jump/sneak/sprint, hold, hotbar, press, look, mouse, wheel/scroll, text, hold_ms, release_control.\n",
    "- When the request also presses or holds a mouse button, the pointer hovers 500 ms first, since some screens (pause menu, world list) ignore a click before they saw the hover.\n",
    "- look uses the Bridge pack's re_mcp:look when active (exact, no cheats, no chat feedback, one per frame), else /tp over the WebSocket, which needs cheats.\n",
    "- Held controls stay down until released or the MCP session ends.\n",
    "- Keep the window from being minimized.\n",
    "\n",
    "Returns {held}: the controls still held down."
);

const UI_CLICK: &str = concat!(
    "Use this to press a menu button, toggle, list entry or tab you can read on screen.\n",
    "\n",
    "Moves the pointer over the label and clicks, as a player would. Works on every menu, including add-on forms, and while the window is hidden.\n",
    "\n",
    "- First it looks for a visible button or toggle in the live UI tree whose text, or a label inside it, matches (unstable ui_tree capability, source ui_tree). Ore UI screens (world list, settings, and add-on forms when Ore UI draws them) have no tree; the oreui capability searches their elements the same way (source oreui).\n",
    "- Both scroll the button's list with the wheel until at least three quarters of its height is visible, up to max_scrolls, and click its centre.\n",
    "- Otherwise it reads the screen with OCR and clicks the matching line (source ocr), with a note saying why.\n",
    "- A label that is exactly text wins over one that contains it, so \"Villager\" does not pick \"Zombie Villager\".\n",
    "- OCR then tries a fuzzy match: one line, or neighbouring lines on a row joined, with at least 70% of the letters in order. This finds big labels OCR splits, such as Respawn.\n",
    "- With OCR and scroll: true it scrolls the list under region (or the window centre) until the text appears, the list stops moving, or max_scrolls run out.\n",
    "- OCR reads the game's pixel fonts, not icons or images; click those with input_send pointer at ui_tree's rectangles.\n",
    "- Fails with what OCR read when the text is not found.\n",
    "\n",
    "Returns {clicked: {text, x, y, width, height}, match (exact, contains or fuzzy), button, scrolls, source, notes}; the tree and Ore UI add control and screen, the names of the button and its screen."
);

const UI_TREE: &str = concat!(
    "Use this to see which buttons, toggles and texts a menu or the HUD shows, with their rectangles, before clicking or to check a screen's state.\n",
    "\n",
    "Reads the live UI control tree of the top screen of each scene stack through the unstable ui_tree capability (see unstable_status). JSON UI screens have one: menus, settings, add-on forms, containers and the HUD. Read-only: the game thread reads it on its next frame.\n",
    "\n",
    "- Ore UI screens are named by URL (ending in hbui/index.html for the world list and settings, hbui/gameplay.html for add-on forms when Ore UI draws them). The oreui capability reads them with a fixed read-only page script: focusable or clickable elements, elements labelled for tests, scrolling lists and text (source oreui, href). Without it, detail says why.\n",
    "- Rectangles are [x, y, width, height] in logical window pixels, the units of input_send pointer and ui_click region.\n",
    "- view controls lists buttons, toggles, text boxes, sliders, dropdowns, scroll views and free text as {kind, name, rect, text, shown}, with enabled: false, hover or checked when they apply.\n",
    "- view tree nests every control under root with name, rect, components (class names such as ButtonComponent), text, checked and children.\n",
    "- name is the control's JSON UI name; for Ore UI the data-testid, else data-focusable-id, else the tag, and components is the JSON UI component matching the element's role. A button's text is the labels inside it.\n",
    "- shown is false when the centre lies outside its scroll view's visible part, so ui_click would scroll first.\n",
    "- For Ore UI, hover means focused, and checked appears only where the page marks it (aria-checked or a checkbox).\n",
    "- A read stops at 2000 controls or max_depth (Ore UI: 1500 elements or 12000 scanned) and sets truncated.\n",
    "- When the game does not answer within a second, a screen is the last read, with fresh: false.\n",
    "- Fails with the reason when the ui_tree capability is not active.\n",
    "\n",
    "Returns {screens, window, notes}. Each screen has stack (the id game_status unstable.screens uses), screen (its name), nodes, truncated, read_us (game-thread read time), fresh, ui_scale (window pixels per UI unit, or per CSS pixel for Ore UI), and controls or root. notes say why an Ore UI page could not be read."
);

const ITEM_USE: &str = concat!(
    "Use this to throw, shoot, eat, drink or block with the held item once. For a fight or several uses in a row, put item_use steps in one action_sequence.\n",
    "\n",
    "mode instant clicks use once (throw, start a drink, place a boat). hold keeps use down for ms or ticks, then releases (eat, draw a bow, throw a charged weapon, block with a shield); with neither, until a call with mode release. release lets go of a held use, such as shooting the bow.\n",
    "\n",
    "- hand offhand only states the intent for actor self, since the game picks the hand; it fails for simulated players, which only use hotbar slots.\n",
    "- When actor self's use shows no event, reason says why with the Bridge pack: no_item (empty hand), cooldown (ticks left on the item's cooldown), projectile_in_flight (the item's projectile is still out near the player, such as a returning thrown weapon) or no_event.\n",
    "- A simulated player's timed hold reports the event that ended the use (item_complete_use, item_release_use or item_stop_use) as evidence.completed, and the player after it as evidence.player.\n"
);

const ITEM_USE_ON: &str = concat!(
    "Use this to open a door or chest, press a button, till dirt, fill a bucket or place a block on a given block face.\n",
    "\n",
    "The player aims at the centre of the face, then uses the held item on it.\n",
    "\n",
    "- The block must be in sight and within the player's reach, which https://minecraft.wiki/w/Interaction_range gives as 5 blocks on Bedrock.\n",
    "- When a simulated player's use goes unconfirmed, notes blame reach if the block is farther than the server's limit (4.5 blocks, 6 in creative).\n",
    "- Confirmed by player_interact_block, item_start_use_on, item_use, or block_place on the neighbouring block.\n"
);

const ATTACK: &str = concat!(
    "Use this for one melee swing at an entity or at the crosshair. To fight mobs until they die, use fight.\n",
    "\n",
    "With entity_id, actor self turns toward the entity's middle and clicks only when the crosshair shows it within reach. With the Bridge pack the turn is one re_mcp:aim write, which also reads the crosshair.\n",
    "\n",
    "- Reach is 3 blocks in survival and 5 in creative, along the sight to the body. Beyond it the call returns at once without clicking (reason out_of_reach), unless approach is true.\n",
    "- approach walks straight at the entity, re-aiming every 150 ms, until half a block inside reach (then it clicks) or approach_ms passes. evidence.approach is {walked_ms, reached, distance}.\n",
    "- A walk that gets less than 0.3 blocks closer in 700 ms stops with reason blocked, evidence.approach.blocked and a note naming the block in the way (on the sight or at the player's feet).\n",
    "- Confirmed by entity_hit_entity (entity_hit_block also counts without entity_id). No WebSocket event reports a melee hit, so without the Bridge pack an entity_id attack reads the target's health natively (unstable world_state capability, single player): before the swing and then once a tick for up to 2 s. A drop confirms it (source native, evidence.health {before, after, max}); a read of 0 or the entity gone confirms a kill (evidence.killed, evidence.seen health_zero or gone, where gone also covers a despawn). No drop gives reason no_damage. Without the capability it stays unconfirmed, and notes say why.\n",
    "- SimulatedPlayer.attackEntity works at any distance (Microsoft Learn's SimulatedPlayer reference), so the driver refuses an entity_id beyond reach (eyes to body) and reports evidence.distance.\n",
    "- When the game refuses a hit, notes say why if the pack knows (the target was just hurt, or is dead).\n"
);

const ENTITY_INTERACT: &str = concat!(
    "Use this to trade with, ride, feed, name or shear an entity.\n",
    "\n",
    "Right-clicks the entity with the held item, or whatever is under the crosshair. With entity_id the player turns toward the entity and clicks only when the crosshair shows it within reach.\n",
    "\n",
    "- approach walks closer first and stops early when blocked, as for attack.\n",
    "- Confirmed by player_interact_entity, or MobInteracted without the Bridge pack.\n",
    "- A simulated player refuses an entity_id beyond reach, as for attack.\n"
);

const BLOCK_BREAK: &str = concat!(
    "Use this to mine one block.\n",
    "\n",
    "Mines until the block breaks or timeout_ms passes. Actor self holds attack, aiming at the block's centre when location is given, else on the block under the crosshair; a simulated player calls breakBlock.\n",
    "\n",
    "- Confirmed by a block_break event at that location; without the Bridge pack by BlockBroken (self) or testforblock finding air (simulated players).\n",
    "- On timeout evidence.block shows the block still there, and notes say when it may be out of reach.\n",
    "- evidence.elapsed_ms is how long it took.\n"
);

const BLOCK_PLACE: &str = concat!(
    "Use this to place the held block, or the one in hotbar slot, against a face of an existing block.\n",
    "\n",
    "Works as a right click on that face: the new block goes to the neighbouring position across the face, reported as evidence.placed_at.\n",
    "\n",
    "- Confirmed by a block_place event there, BlockPlaced without the Bridge pack, or testforblock for simulated players.\n",
    "- When nothing is placed, notes say if the block may be out of reach.\n"
);

const ITEM_DROP: &str = concat!(
    "Use this to drop items from the selected hotbar slot.\n",
    "\n",
    "Actor self presses key.drop once per item, or Ctrl with key.drop for the whole stack. A simulated player drops exactly quantity in one call (the pack splits the stack before dropSelectedItem).\n",
    "\n",
    "- Actor self is confirmed by entity_spawn events of item entities near the player whose amounts add up to quantity (ItemDropped without the Bridge pack); evidence.dropped is the count seen.\n",
    "- A simulated player is confirmed when the slot, read in the same tick, holds that many fewer.\n"
);

const MOVEMENT_SET: &str = concat!(
    "Use this to walk to a position or an entity (to), or to press movement keys (forward, strafe, jump, sneak, sprint).\n",
    "\n",
    "With to, actor self finds a path around walls and walks it as a player would: the view turns toward each waypoint in eased mouse steps, key.forward walks, and key.jump climbs one-block ledges, until it is within arrival_radius blocks horizontally or timeout_ms passes. A simulated player path-finds with navigateToLocation or navigateToEntity.\n",
    "\n",
    "- Actor self's path: A* over block cells within 32 blocks of the start, bounded in cells searched and time per plan. It steps up one block, drops at most 3, never enters lava, and enters water only with avoid_water: false.\n",
    "- Blocks come from the Bridge pack's re_mcp:blocks, else testforblock in a world with cheats (unknown blocks count as solid), else the player walks straight at the target. evidence.path.source says which (bridge, commands or none), and notes say why it fell back.\n",
    "- It plans again when an entity target moves, and when the player leaves the path or gets stuck (after one jump); 6 such retries at most. reason blocked says when no path leads on, with a path that ends at the spot nearest the target.\n",
    "- The view turns by mouse motion, measured on the first turn. When mouse motion does not turn it, the view turns with exact looks, which snap, and a note says so.\n",
    "- Walking straight with the Bridge pack, actor self stops at the edge of water or lava ahead (evidence.liquid) unless avoid_water is false; a player already in water walks on.\n",
    "- A simulated player must also be within arrival_radius plus 1 block vertically (evidence.vertical_gap is the target's height above it). It gives up at once when navigation finds no path, and after 2 s without moving.\n",
    "- Manual keys stay down for duration_ms, or without it until a later call sets them to 0 or false, or the stop action.\n",
    "- A simulated player moves with moveRelative and cannot sneak.\n",
    "- evidence holds the start and end positions. For actor self evidence.path is {source, waypoints (block positions where the path turns or changes level), length, reached, bound (radius, nodes or time when a search stopped early), expanded, replans, loads, camera {turns: mouse or look, counts_per_degree}}.\n"
);

const FIGHT: &str = concat!(
    "Use this to fight hostile mobs with the connected player until they die, such as clearing the zombies a test spawned.\n",
    "\n",
    "Picks the nearest mob that matches target, walks to it along a path as movement_set does, and fights it as a player would: keeps it within reach and steps back when it comes closer than 1.5 blocks, follows it with the view in eased steps, swings once its hurt immunity wore off (every 550 ms) and the view is on its torso, strafes side to side between swings and opens with a sprint hit for knockback. With all it moves on to the next mob until none is left within radius, timeout_ms passes or max_targets were fought.\n",
    "\n",
    "- target is an entity id, a type id with a colon (minecraft:zombie) or a family (monster, the default). Families match with querytarget, which needs cheats; types use the native entity list in single player when the unstable layer runs, else querytarget.\n",
    "- A swing goes out when the read view is within the mob's body angle of its torso (0.62 of its height, from a per-type height). With the Bridge pack, a view still off 300 ms after the swing was due is aimed with its re_mcp:aim, which shows nothing in chat, and the click follows at once (aim is ignored). Without the pack the view is aimed as aim says.\n",
    "- Without the pack aim chooses how. tp (default) is accurate: /tp turns the view on the server and the click follows a tenth of a second later, and in clean test fights about 3 in 4 swings hit, but each /tp prints a line in chat. mouse is quiet, for silent recordings: a mouse flick at the measured sensitivity, then the reads are watched until they show it (at most twice the measured read lag, 400 ms) and one correction flick follows if needed; the reads lag 150 to 300 ms, so only about 1 in 3 swings hit (evidence.flicks; a click still off the aim counts as off_aim). evidence.look_aims counts pack and /tp aims.\n",
    "- With the Bridge pack, hits are its entity_hurt events by the player and kills its entity_die events (source bridge). Without it, the unstable layer's native health reads count a drop after a swing as a hit, and health 0 or the mob gone as a kill (source native). Without either, hits are null and a mob gone from querytarget counts as killed but unconfirmed (source command), since a despawn looks the same.\n",
    "- A mob that no path reaches is left with left.class blocked, and the fight moves on to the next.\n",
    "- Plays actor self only; for a simulated player use attack steps in action_sequence.\n",
    "\n",
    "Returns {actor, driver, confirmed, source, evidence, notes, reason}. confirmed is true when at least one mob was fought and every one fought died with a confirmed kill. evidence is {targets: [{id, type, swings, sprint_swings, hits, misses, no_damage (native: swings whose read saw no drop), off_aim, phases_ms {walking, aiming (a swing due, waiting for the aim), cadence, ticks}, killed, kill {confirmed, source, seen, tick}, time_to_kill_ms, left {class, detail}}], kills, ended (no_targets_left, max_targets or timeout), elapsed_ms, flicks, look_aims, stray_hits (hits on other mobs than the one fought), read_lag_ms, path (as movement_set's)}. reason {class, detail} is no_target, blocked, timeout or no_event."
);

const ENTITY_SPAWN: &str = concat!(
    "Use this to put a test mob or other entity in front of a player and get its id for attack, look or conditions. Inside a fight, prefer a spawn step in action_sequence.\n",
    "\n",
    "Summons type with /summon at ahead, right and up blocks from the actor's feet along its facing, or at position, and finds the new entity's id. Needs cheats; works without the Bridge pack.\n",
    "\n",
    "- With the Bridge pack the id comes from its entity_spawn event of that type near the spot (evidence.matched_by entity_spawn), else the nearest entity of the type within 2 blocks (matched_by nearest, with a note that it may be an older one).\n",
    "- Without the pack the actor's place comes from querytarget, and the id is the entity of the type within 2 blocks that querytarget lists after the /summon but not before it (matched_by querytarget), looked for up to 2 s; else the nearest one there (matched_by nearest, with a note).\n",
    "- With the pack it refuses a spot whose feet or head block is solid or outside the loaded chunks, naming the block; without it the spot is not checked.\n",
    "- Each effect is applied with /effect on the nearest entity of the type at the spot; evidence.effects is [{id, applied, error}].\n",
    "- evidence also holds id, type, location, requested_at and line_of_sight {clear, blocked_by {id, location}}: whether a ray from the actor's eyes to the entity's middle meets a block. Without the pack clear is null (unknown), with a note.\n"
);

const ACTION_SEQUENCE: &str = concat!(
    "Use this for any play-test with more than one action, such as a whole fight.\n",
    "\n",
    "Runs up to 64 steps in one call (spawn mobs, switch items, aim, attack until a target dies, record), so the server reacts within a tick and a recording holds no waits between tool calls. Each step names an action and its args as the matching tool takes them.\n",
    "\n",
    "- **Actions:** spawn takes entity_spawn's arguments and about 10 ticks. hotbar_select takes slot (0 to 8). look takes position, entity_id or block (with face). fight takes the fight tool's arguments and runs until its mobs died or its timeout_ms. stop releases held keys and stops a simulated player, which keeps looking at a look target until its next look.\n",
    "- **References:** after a spawn step with as: \"A\", args and conditions may write \"$A\" (or \"$spawn:<index>\") for its id; an event contains of just \"$A\" matches that id, so {event: {name: \"entity_die\", contains: \"$A\"}} waits for its death. An item_use step targets the entity its actor last looked at, attacked or walked to.\n",
    "- **Without the Bridge pack:** the sequence reads the health of every spawned or targeted entity natively about once a tick (unstable world_state capability, single player) and adds each drop as an entity_hurt event and each death (health 0, or gone after a read showed it, which also covers a despawn) as an entity_die event, source native and the read's server tick. Event conditions, killed_by and final use them as they use the pack's. If the native read is unavailable, notes say so and kills go unseen.\n",
    "- **Timing:** use at_tick (game ticks by the Bridge pack's clock, 50 ms each without it) or after_ms throughout; a step without either runs right after the one before. Steps that would start after timeout_ms are skipped.\n",
    "- **Sending ahead:** with at_tick and the pack, a simulated player's step goes a second ahead and runs on its exact tick, so tick 0 is a second after the call. An actor self step that aims (attack or entity_interact with entity_id, item_use_on, block_place, block_break with location) starts up to 8 ticks early and clicks on its tick; a look starts 3 ticks early; neither before the player's previous step or the spawn step it names. started_tick is when a step ran, or when actor self's input went out.\n",
    "- **Overlap:** a step finishes (event, walk, mining, hold) while later steps start. An actor self step waits only for a control an earlier one still holds: a timed item_use hold keeps key.use and the view (hotbar_select waits too), block_break keeps key.attack and the view, movement_set keeps the view and movement keys with to or approach, and the movement keys with duration_ms, and fight keeps the view, the movement keys and key.attack. notes list actions that started more than a tick late.\n",
    "- **Conditions:** a step waits for wait_for (up to wait_ms), runs only if its if holds, then runs once or as repeat says; such a step is not sent ahead and holds back later steps. Conditions are checked once a tick; event conditions count events since the previous step started (since the call for the first step and stop_when).\n",
    "- **Repeat:** until and stop_when are checked right before each run, so no run starts after until held. A run whose target already died or went away is skipped with reason target_dead, is not a failure, and ends the repeat.\n",
    "- **Stopping:** stop_when ends the run as soon as it holds or cannot be checked, as fail_fast, max_failures, sequence_stop and a client disconnect do: steps not started are skipped, running ones are marked interrupted, and every actor that acted gets the stop action (actor self releases every held control).\n",
    "- **Failures:** a failed step is reported and the rest still run. An error, an unconfirmed outcome and an unmet expect each count toward fail_fast and max_failures; expect is checked as each action settles.\n",
    "- **Warm-up:** first the connected player warms up as scene_reset's warm_up does, so its held item is not frozen after a world load. \"auto\" does it when the run records, or plays actor self and no warm-up ran since the world loaded.\n",
    "- **Recording:** record starts after the warm-up; the step clock starts with it, each step marks a chapter, and it stops once the last step settled.\n",
    "- **Blocking:** a call answers when the run ends, or after blocking_limit_ms with {run_id, state: running, cursor, note} while the run goes on; fetch the result with sequence_status, which keeps the last 8 runs. background: true answers at once with {run_id, clock, planned: [{index, action, at_tick or after_ms}]}.\n",
    "- **During a run:** other calls are answered between steps, and a slow one such as frame_capture delays the next step. One run goes at a time. Refused meanwhile: another action_sequence; the actor tools, look_at, hotbar_select and player_set for an actor it plays; input_send, ui_click and chat_send when it plays actor self.\n",
    "- **Input lock:** while a run plays actor self, the player's real keyboard and mouse are locked out of the game and the action bar says so (with the WebSocket). F9 gives the player the game back: the run stops as sequence_stop stops it, with stopped.by take_over and taken_over: true, and nothing locks again until the next tool call.\n",
    "- **Hits:** an attack or interaction hits when confirmed on the target. A throw or shot hits when its projectile hit the target, or the target was hurt before the next use aimed at it. An action that never went out counts only in reasons; one after the target died counts as after_death.\n",
    "- **Reasons:** the action reasons plus target_dead and interrupted; projectile_in_flight also covers an empty hand right after using an item now gone from the inventory, such as a returning throw.\n",
    "- **verbose: true** adds clock (tick or wall), confirmed (a count), the whole summary (first_tick, last_tick, hits_mean), warm_up and actors_stopped, and steps: each step's times, wait_for and if details, outcome (the action tool's reply) or error or skipped, expect {met, confirmed, holds}, and for a repeat its runs and what ended them.\n",
    "\n",
    "Returns {run_id, state, timing {wall_ms, first_tick, last_tick}, summary, failed_steps, stopped, failures, error, warm_up {done, detail}, recording, taken_over, notes}:\n",
    "- summary.targets maps each entity id to {label, spawned_by, type, steps, hits, misses, after_death, killed_by, final, reasons}. killed_by is {step, run, tick, cause, killer, source} of the last action before its entity_die (source bridge or native), or null; final.state is dead, alive, gone or unknown.\n",
    "- summary.totals counts actions, confirmed, unconfirmed, hits, misses, kills and reasons.\n",
    "- failed_steps has one line per step that was skipped, failed, went unconfirmed or missed its expect: {index, action, target, ok: false, confirmed, reason, error}, where reason is an action reason, expect_unmet or skipped. A repeat gives {runs, failed, reasons} instead.\n",
    "- stopped is null, or {at_ms, detail or error, by, actors_stopped} when something ended the run early. failures comes with a failure limit; recording is video_stop's report."
);

const SEQUENCE_STATUS: &str = concat!(
    "Use this to follow a background action_sequence, or to fetch the result of one whose blocking call answered with state running.\n",
    "\n",
    "Reads what a run (default the latest) did since cursor since, waiting up to wait_ms for a new step result or the run's end. The last 8 runs are kept, blocking and background alike.\n",
    "\n",
    "- Results come in the order they settled, shaped as action_sequence's steps. A repeated step's runs come one at a time, then its whole entry once the repeat ends.\n",
    "- failed_steps has action_sequence's lines for the newly settled results; verbose: true puts them whole in steps.\n",
    "- Once the run ended, the reply adds action_sequence's summary, timing, warm_up, recording and error where they apply, in its default or verbose shape; verbose also adds confirmed.\n",
    "\n",
    "Returns {run_id, state (running, finished, stopped or failed), cursor (the next since), settled (results since since), failed_steps, current, failures, stopped, notes}. current is {index, run, started_tick, started_ms} of the last action started that still runs, or null."
);

const SEQUENCE_STOP: &str = concat!(
    "Use this to end a running action_sequence early.\n",
    "\n",
    "Ends the run as stop_when does: steps not started are skipped, running actions are marked interrupted, and every actor that acted gets the stop action, which for actor self releases every held control. Answers once the run wound down.\n",
    "\n",
    "- A run that already ended returns its result unchanged.\n",
    "\n",
    "Returns action_sequence's result (whole with verbose: true) with run_id and state; stopped.by is sequence_stop."
);

fn actor() -> Value {
    json!({
        "type": "string",
        "description": "\"self\" (default) plays the player who ran /connect with synthetic input; the name of a GameTest simulated player calls its SimulatedPlayer methods instead"
    })
}

fn face() -> Value {
    json!({ "enum": ["down", "up", "north", "south", "east", "west"] })
}

fn entity_id(what: &str) -> Value {
    json!({ "type": "string", "description": format!("The entity to {what}, by the id entities_list reports; without it the action applies to whatever is under the crosshair") })
}

fn outcome(text: &str) -> String {
    format!("{text}{OUTCOME}")
}

pub fn tools() -> Vec<Tool> {
    let tool = |hints: Hints, title: &'static str, run: Handler, definition| {
        Tool::new(Toolset::Actions, hints, title, run, definition)
    };
    let controls = json!({
        "type": "array", "items": { "type": "string" },
        "description": "Bindings from options.txt (key.jump, key.use, key.hotbar.1, key.chat), W3C key codes (KeyW, Digit1, Space, ShiftLeft, Escape, F5) or MouseLeft/MouseRight/MouseMiddle/MouseBack/MouseForward"
    });
    let point = json!({
        "oneOf": [
            object(json!({ "x": { "type": "number" }, "y": { "type": "number" } }), &["x", "y"]),
            { "type": "array", "items": { "type": "number" }, "minItems": 2, "maxItems": 2 },
            { "type": "null" }
        ],
        "description": "Absolute logical window pixels from the top left, as {x,y} or [x,y]; with press:[MouseLeft] it clicks menus; null hands the pointer back to the real mouse"
    });
    vec![
        tool(
            Hints::ACT,
            "Send input",
            |s, a| reply(s.input(arguments(a)?)?),
            json!({
                "name": "input_send",
                "description": INPUT_SEND,
                "inputSchema": object(json!({
                    "hold": controls, "release": controls, "press": controls,
                    "press_frames": { "type": "integer", "minimum": 0, "description": "Frames a press stays down (default 1)" },
                    "press_ms": { "type": "integer", "minimum": 0, "description": "Milliseconds a press stays down instead of frames" },
                    "hold_ms": { "type": "integer", "minimum": 0, "description": "Release this request's hold controls after this long" },
                    "move": describe(object(json!({ "forward": { "type": "number" }, "strafe": { "type": "number" } }), &[]), "The sign of each axis holds key.forward/key.back and key.right/key.left; 0 releases both"),
                    "jump": { "type": "boolean", "description": "Hold (true) or release (false) key.jump" },
                    "sneak": { "type": "boolean", "description": "Hold (true) or release (false) key.sneak" },
                    "sprint": { "type": "boolean", "description": "Hold (true) or release (false) key.sprint" },
                    "pointer": point,
                    "cursor": { "oneOf": [{ "type": "array", "items": { "type": "number" }, "minItems": 2, "maxItems": 2 }, { "type": "null" }], "description": "Same as pointer, as [x, y]." },
                    "wheel": describe(object(json!({ "x": { "type": "number" }, "y": { "type": "number" }, "unit": { "enum": ["line", "pixel"] } }), &[]), "Scroll; axes default 0, positive y scrolls up. unit line (default) sends whole notches, one per frame; pixel sends the values as raw GameInput wheel units (120 per notch) in one reading"),
                    "scroll": describe(object(json!({ "x": { "type": "number" }, "y": { "type": "number" }, "pixels": { "type": "boolean" } }), &["y"]), "Same as wheel; pixels: true sends raw GameInput wheel units"),
                    "hotbar": { "type": "integer", "minimum": 1, "maximum": 9, "description": "Press key.hotbar.N" },
                    "text": { "type": "string", "description": "Characters typed into the focused text field" },
                    "look": describe(object(json!({
                        "yaw": { "type": "number" }, "pitch": { "type": "number" },
                        "relative": { "type": "boolean" },
                        "frames": { "type": "integer", "minimum": 0, "description": "Turn over this many frames (0 snaps)" }
                    }), &["yaw", "pitch"]), "Turn the player to Bedrock yaw/pitch degrees (relative: by them)"),
                    "mouse": describe(object(json!({ "x": { "type": "number" }, "y": { "type": "number" } }), &[]), "Raw relative mouse motion in counts (works without cheats); positive x turns right, positive y looks down; about 400 counts is a noticeable turn"),
                    "release_all": { "type": "boolean", "description": "Release every held control and the pointer first" },
                    "release_control": { "type": "boolean", "description": "Release everything at the end, handing input back to the real devices" }
                }), &[])
            }),
        ),
        tool(
            Hints::ACT,
            "Click on-screen text",
            |s, a| reply(s.click_text(arguments(a)?)?),
            json!({
                "name": "ui_click",
                "description": UI_CLICK,
                "inputSchema": object(json!({
                    "text": { "type": "string", "description": "The label to click, matched ignoring case" },
                    "exact": { "type": "boolean", "description": "Only a line that is exactly text, with no contains or fuzzy match (default false)" },
                    "scroll": { "type": "boolean", "description": "With OCR, scroll the list to find it (default false); the UI tree scrolls to a button it knows without it" },
                    "max_scrolls": { "type": "integer", "minimum": 0, "description": "Most wheel scrolls: to bring a UI tree button into view, or with OCR and scroll: true (default 10)" },
                    "right": { "type": "boolean", "description": "Right-click instead" },
                    "region": describe(region(), "Only click (and with OCR, look and scroll) inside this rectangle, in logical window pixels")
                }), &["text"])
            }),
        ),
        tool(
            Hints::READ,
            "Read the UI tree",
            |s, a| reply(s.ui_tree(arguments(a)?)?),
            json!({
                "name": "ui_tree",
                "description": UI_TREE,
                "inputSchema": object(json!({
                    "stack": { "type": "integer", "description": "Only the stack with this id (game_status unstable.screens[].id)" },
                    "max_depth": { "type": "integer", "minimum": 0, "maximum": 64, "description": "Levels below the root to read (default 48)" },
                    "visible_only": { "type": "boolean", "description": "Skip hidden controls and everything inside them (default true)" },
                    "view": { "enum": ["controls", "tree"], "description": "controls (default): a flat list of what a player can use or read; tree: every control, nested" }
                }), &[])
            }),
        ),
        tool(
            Hints::ACT,
            "Use item",
            |s, a| reply(drivers::run(s, "item_use", a)?),
            json!({
                "name": "item_use",
                "description": outcome(ITEM_USE),
                "inputSchema": object(json!({
                    "actor": actor(),
                    "mode": { "enum": ["instant", "hold", "release"], "description": "Default instant" },
                    "ms": { "type": "integer", "minimum": 0, "description": "How long mode hold keeps the item in use" },
                    "ticks": { "type": "integer", "minimum": 0, "description": "Same as ms, in game ticks of 50 ms" },
                    "hand": { "enum": ["mainhand", "offhand"], "description": "Default mainhand" }
                }), &[])
            }),
        ),
        tool(
            Hints::ACT,
            "Use item on block",
            |s, a| reply(drivers::run(s, "item_use_on", a)?),
            json!({
                "name": "item_use_on",
                "description": outcome(ITEM_USE_ON),
                "inputSchema": object(json!({
                    "actor": actor(),
                    "location": vector_schema("The block to use the item on"),
                    "face": describe(face(), "The face to click; without it the player aims at the block centre and uses whatever face the crosshair shows")
                }), &["location"])
            }),
        ),
        tool(
            Hints::ACT,
            "Attack",
            |s, a| reply(drivers::run(s, "attack", a)?),
            json!({
                "name": "attack",
                "description": outcome(ATTACK),
                "inputSchema": object(json!({ "actor": actor(), "entity_id": entity_id("attack"), "approach": approach(), "approach_ms": approach_ms() }), &[])
            }),
        ),
        tool(
            Hints::ACT,
            "Interact with entity",
            |s, a| reply(drivers::run(s, "entity_interact", a)?),
            json!({
                "name": "entity_interact",
                "description": outcome(ENTITY_INTERACT),
                "inputSchema": object(json!({ "actor": actor(), "entity_id": entity_id("interact with"), "approach": approach(), "approach_ms": approach_ms() }), &[])
            }),
        ),
        tool(
            Hints::ACT,
            "Break block",
            |s, a| reply(drivers::run(s, "block_break", a)?),
            json!({
                "name": "block_break",
                "description": outcome(BLOCK_BREAK),
                "inputSchema": object(json!({
                    "actor": actor(),
                    "location": vector_schema("The block to break; required for simulated players"),
                    "timeout_ms": { "type": "integer", "minimum": 0, "maximum": 300000, "description": "How long to keep mining (default 10000)" }
                }), &[])
            }),
        ),
        tool(
            Hints::ACT,
            "Place block",
            |s, a| reply(drivers::run(s, "block_place", a)?),
            json!({
                "name": "block_place",
                "description": outcome(BLOCK_PLACE),
                "inputSchema": object(json!({
                    "actor": actor(),
                    "location": vector_schema("The existing block to place against"),
                    "face": describe(face(), "The face of that block to click"),
                    "slot": { "type": "integer", "minimum": 0, "maximum": 8, "description": "Hotbar slot to select first (0 to 8); default the selected slot" }
                }), &["location", "face"])
            }),
        ),
        tool(
            Hints::ACT,
            "Drop items",
            |s, a| reply(drivers::run(s, "item_drop", a)?),
            json!({
                "name": "item_drop",
                "description": outcome(ITEM_DROP),
                "inputSchema": object(json!({
                    "actor": actor(),
                    "quantity": {
                        "oneOf": [{ "type": "integer", "minimum": 1, "maximum": 64 }, { "const": "stack" }],
                        "description": "How many items, or \"stack\" for the whole stack (default 1)"
                    }
                }), &[])
            }),
        ),
        tool(
            Hints::ACT,
            "Move",
            |s, a| reply(drivers::run(s, "movement_set", a)?),
            json!({
                "name": "movement_set",
                "description": outcome(MOVEMENT_SET),
                "inputSchema": object(json!({
                    "actor": actor(),
                    "to": {
                        "oneOf": [vector_schema("A position"), { "type": "string", "description": "An entity id" }],
                        "description": "Where to walk: a position {x, y, z} or an entity id to follow"
                    },
                    "arrival_radius": { "type": "number", "exclusiveMinimum": 0, "description": "Horizontal distance that counts as arrived (default 1)" },
                    "timeout_ms": { "type": "integer", "minimum": 0, "maximum": 300000, "description": "Give up walking to the target after this long (default 20000)" },
                    "avoid_water": { "type": "boolean", "description": "Keep actor self out of water: paths go around it, and straight walking with the Bridge pack stops at its edge (default true)" },
                    "forward": { "type": "number", "description": "Positive walks forward, negative back, 0 releases both keys" },
                    "strafe": { "type": "number", "description": "Positive steps right, negative left, 0 releases both keys" },
                    "jump": { "type": "boolean", "description": "Hold (true) or release (false) key.jump, also while walking with to; a simulated player jumps once, and ignores it with to" },
                    "sneak": { "type": "boolean", "description": "Hold (true) or release (false) key.sneak; not with to, and simulated players cannot sneak" },
                    "sprint": { "type": "boolean", "description": "Hold (true) or release (false) key.sprint, also while walking with to; a simulated player moves at full speed, and ignores it with to" },
                    "duration_ms": { "type": "integer", "minimum": 0, "maximum": 300000, "description": "Hold the manual keys this long, then release them" }
                }), &[])
            }),
        ),
        tool(
            Hints::ACT,
            "Fight mobs",
            |s, a| reply(drivers::run(s, "fight", a)?),
            json!({
                "name": "fight",
                "description": FIGHT,
                "inputSchema": object(json!({
                    "actor": describe(actor(), "Only \"self\" (default): the player who ran /connect"),
                    "target": { "type": "string", "description": "An entity id, a type id with a colon such as minecraft:zombie, or a family such as monster (default monster)" },
                    "all": { "type": "boolean", "description": "Fight every matching mob in turn, nearest first, not only one (default false)" },
                    "max_targets": { "type": "integer", "minimum": 1, "maximum": 64, "description": "The most mobs to fight (default 1, or 16 with all)" },
                    "radius": { "type": "number", "exclusiveMinimum": 0, "maximum": 64, "description": "How far around the player to look for mobs, in blocks (default 24)" },
                    "timeout_ms": { "type": "integer", "minimum": 0, "maximum": 300000, "description": "End the fight after this long (default 60000)" },
                    "aim": { "enum": ["tp", "mouse"], "description": "Without the Bridge pack: tp (default) aims with /tp, accurate but each prints a chat line; mouse aims with mouse flicks, quiet but less accurate. Ignored with the pack" }
                }), &[])
            }),
        ),
        tool(
            Hints::ACT,
            "Spawn entity",
            |s, a| reply(drivers::run(s, "spawn", a)?),
            json!({
                "name": "entity_spawn",
                "description": outcome(ENTITY_SPAWN),
                "inputSchema": object(json!({
                    "actor": describe(actor(), "Whose position and facing ahead, right and up count from"),
                    "type": { "type": "string", "description": "Entity type id, such as minecraft:zombie or zombie" },
                    "ahead": { "type": "number", "description": "Blocks in front of the actor along its yaw (default 0)" },
                    "right": { "type": "number", "description": "Blocks to the actor's right; negative goes left (default 0)" },
                    "up": { "type": "number", "description": "Blocks above the actor's feet (default 0)" },
                    "position": vector_schema("An absolute spot instead of ahead, right and up"),
                    "name": { "type": "string", "description": "Name tag for the entity" },
                    "effects": {
                        "type": "array",
                        "description": "Effects to give it, such as slowness 255 to keep it in place or fire_resistance against daylight",
                        "items": object(json!({
                            "id": { "type": "string", "description": "Effect id, such as slowness" },
                            "seconds": { "type": "integer", "minimum": 0, "maximum": 1000000, "description": "Default 600" },
                            "amplifier": { "type": "integer", "minimum": 0, "maximum": 255, "description": "Default 0 (level I)" },
                            "hide": { "type": "boolean", "description": "Hide the particles (default false)" }
                        }), &["id"])
                    }
                }), &["type"])
            }),
        ),
        tool(
            Hints::ACT,
            "Run action sequence",
            |s, a| reply(s.action_sequence(a)?),
            json!({
                "name": "action_sequence",
                "description": ACTION_SEQUENCE,
                "inputSchema": object(json!({
                    "actor": describe(actor(), "The actor for steps and conditions that do not name one"),
                    "steps": {
                        "description": "The actions to run, 1 to 64; each names an action, its args, an optional at_tick or after_ms, and optional wait_for, if and repeat",
                        "type": "array", "minItems": 1, "maxItems": 64,
                        "items": object(json!({
                            "action": { "enum": ACTIONS },
                            "args": { "type": "object", "description": "The action's arguments; strings may name spawned entities as $<as> or $spawn:<index>" },
                            "as": { "type": "string", "description": "On a spawn step: the name later steps use as $name for the new entity's id" },
                            "at_tick": { "type": "integer", "minimum": 0, "description": "Game ticks after the start" },
                            "after_ms": { "type": "integer", "minimum": 0, "description": "Milliseconds after the start" },
                            "wait_for": condition("Wait from the step's time until this holds, then run the step; skipped if it does not hold within wait_ms"),
                            "wait_ms": { "type": "integer", "minimum": 0, "maximum": 300000, "description": "How long wait_for may wait (default until timeout_ms)" },
                            "if": condition("Run the step only if this holds when it is due (after wait_for); otherwise skip it"),
                            "expect": describe(object(json!({
                                "confirmed": { "const": true, "description": "The game must confirm the action" },
                                "holds": condition("Must hold when the action's confirmation settles; its event conditions count events from when the action started")
                            }), &[]), "What each of the step's actions must show once its confirmation settles; an action that does not show it counts as a failure for fail_fast and max_failures"),
                            "repeat": describe(object(json!({
                                "times": { "type": "integer", "minimum": 1, "maximum": 1000, "description": "Most runs (default 1000)" },
                                "until": condition("Stop repeating once this holds; checked before each run, so a step whose until already holds does not run"),
                                "every_ticks": { "type": "integer", "minimum": 1, "description": "Ticks between run starts (default 1); with at_tick timing" },
                                "every_ms": { "type": "integer", "minimum": 1, "description": "Milliseconds between run starts (default 50); with after_ms timing" }
                            }), &[]), "Run the action again and again, from when the step starts, until times runs started, until holds, timeout_ms passes or stop_when holds; give times, until or both")
                        }), &["action"])
                    },
                    "stop_when": condition("End the sequence as soon as this holds, e.g. low health; checked once a tick from the call"),
                    "timeout_ms": { "type": "integer", "minimum": 0, "maximum": 300000, "description": "Skip steps that would start later than this, and end waits and repeats then (default 60000)" },
                    "fail_fast": { "type": "boolean", "description": "End the sequence as stop_when does after the first action that errors, ends unconfirmed or misses its expect (default false)" },
                    "max_failures": { "type": "integer", "minimum": 1, "description": "End the sequence after this many such failures; not with fail_fast" },
                    "background": { "type": "boolean", "description": "Return a run_id at once and run the steps while other tool calls are served; watch with sequence_status, end with sequence_stop (default false)" },
                    "blocking_limit_ms": { "type": "integer", "minimum": 0, "description": "Longest a blocking call waits before it answers with the run_id and leaves the run going in the background (default 25000; 0 waits to the end)" },
                    "warm_up": {
                        "oneOf": [{ "type": "boolean" }, { "const": "auto" }],
                        "description": "Warm up the connected player before the first step so its held item animates: \"auto\" (default) when the run records or no warm-up ran since the world loaded, true always, false never"
                    },
                    "record": {
                        "oneOf": [{ "type": "boolean" }, { "type": "object", "description": "video_start's arguments: path, fps, game_fps, codec, audio" }],
                        "description": "Record the screen from the first step until the last one settled (default false)"
                    },
                    "verbose": { "type": "boolean", "description": "Return every step's whole entry with its outcome and evidence, not only the failed_steps lines (default false)" }
                }), &["steps"])
            }),
        ),
        tool(
            Hints::READ,
            "Watch a sequence run",
            |s, a| reply(s.sequence_status(arguments(a)?)?),
            json!({
                "name": "sequence_status",
                "description": SEQUENCE_STATUS,
                "inputSchema": object(json!({
                    "run_id": { "type": "string", "description": "The run_id action_sequence returned; default the latest run" },
                    "since": { "type": "integer", "minimum": 0, "description": "The cursor of the last status; default 0, every result" },
                    "wait_ms": { "type": "integer", "minimum": 0, "maximum": 30000, "description": "How long to wait for news while the run runs (default 0)" },
                    "verbose": { "type": "boolean", "description": "Return the settled results whole in steps, not only the failed_steps lines (default false)" }
                }), &[])
            }),
        ),
        tool(
            Hints::ACT,
            "Stop a background sequence",
            |s, a| reply(s.sequence_stop(arguments(a)?)?),
            json!({
                "name": "sequence_stop",
                "description": SEQUENCE_STOP,
                "inputSchema": object(json!({
                    "run_id": { "type": "string", "description": "The run_id action_sequence returned" },
                    "verbose": { "type": "boolean", "description": "Return every step's whole entry, not only the failed_steps lines (default false)" }
                }), &["run_id"])
            }),
        ),
    ]
}

/// `approach` for attack and entity_interact.
fn approach() -> Value {
    json!({ "type": "boolean", "description": "Actor self with entity_id: walk toward the entity when it is beyond reach, then click (default false)" })
}

fn approach_ms() -> Value {
    json!({ "type": "integer", "minimum": 0, "maximum": 300000, "description": "How long approach may walk (default 5000)" })
}

/// A condition a sequence checks, in an object with exactly one key.
fn condition(what: &str) -> Value {
    let actor = describe(actor(), "Whose state counts; default the sequence's actor");
    let any_condition = json!({ "type": "object", "description": "A condition, in the same form" });
    json!({
        "type": "object",
        "minProperties": 1, "maxProperties": 1, "additionalProperties": false,
        "description": format!("{what}. Exactly one of: event (an event since the step before started, by name ignoring case and text its body contains, as events_read shows them), entity (an entity of a type within radius blocks of the actor), state (a field of the Bridge pack's player state compared with a number or value; needs the pack), near (the actor within radius blocks of a position), not, all or any."),
        "properties": {
            "event": object(json!({
                "name": { "type": "string", "description": "Event name, e.g. entity_die or PlayerMessage" },
                "contains": { "type": "string", "description": "Text the event body's JSON contains, e.g. minecraft:zombie" }
            }), &[]),
            "entity": object(json!({
                "matches": { "type": "string", "description": "Entity type id, e.g. minecraft:zombie" },
                "radius": { "type": "number", "exclusiveMinimum": 0, "description": "Blocks (default 32)" },
                "actor": actor
            }), &["matches"]),
            "state": object(json!({
                "path": { "type": "string", "description": "Dotted path into player_get's state, e.g. health.current, hunger, location.y, flags.onGround, effects.0.id" },
                "below": { "type": "number", "description": "Holds while the number is below this" },
                "above": { "type": "number", "description": "Holds while the number is above this" },
                "equals": { "description": "Holds while the field equals this" },
                "actor": actor
            }), &["path"]),
            "near": object(json!({
                "position": vector_schema("The point to be near"),
                "radius": { "type": "number", "exclusiveMinimum": 0, "description": "Blocks, measured from the actor's feet" },
                "actor": actor
            }), &["position", "radius"]),
            "not": any_condition,
            "all": { "type": "array", "minItems": 1, "items": any_condition, "description": "Holds when every condition holds" },
            "any": { "type": "array", "minItems": 1, "items": any_condition, "description": "Holds when one condition holds" }
        }
    })
}
