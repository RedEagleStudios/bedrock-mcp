//! Live data from the unstable layer: content-log cursors and entries, and screen stacks.

use protocol::unstable::{LogEntry, SceneStack};
use serde_json::{Value, json};

use crate::{content_log::Cursor, error::Result};

/// The game's `LogLevel` and `LogArea` numbers with known names, from LeviLamina's public
/// `LogLevel.h` and `LogArea.h`. Entries keep their numbers either way.
const LEVEL_NAMES: &[(u32, &str)] = &[(0, "verbose"), (1, "info"), (2, "warning"), (3, "error")];
const AREA_NAMES: &[(u32, &str)] = &[
    (0, "Actor"),
    (1, "Addon"),
    (2, "Ai"),
    (3, "Animation"),
    (4, "AutomatedTests"),
    (5, "BiomeRegistry"),
    (6, "Blocks"),
    (7, "Camera"),
    (8, "Cereal"),
    (9, "Commands"),
    (10, "Components"),
    (11, "Dialogue"),
    (12, "Effects"),
    (13, "Entity"),
    (14, "FeatureRegistry"),
    (15, "Geometry"),
    (16, "Item"),
    (17, "Json"),
    (18, "LevelStorage"),
    (19, "Lighting"),
    (20, "Localization"),
    (21, "Log"),
    (22, "Molang"),
    (23, "Messaging"),
    (24, "Persona"),
    (25, "Recipes"),
    (26, "Rendering"),
    (27, "RuntimeCamera"),
    (28, "Scripting"),
    (29, "Sound"),
    (30, "Store"),
    (31, "Structure"),
    (32, "Texture"),
    (33, "Ui"),
    (34, "Volumes"),
    (35, "LootTable"),
    (36, "Packs"),
    (37, "DisconnectionScreen"),
    (38, "VoxelShape"),
    (39, "Dimensions"),
];

const LIVE_PREFIX: &str = "live@";

fn name(table: &[(u32, &'static str)], value: u32) -> Option<&'static str> {
    table
        .iter()
        .find(|(known, _)| *known == value)
        .map(|(_, name)| *name)
}

/// A `content_log_read` cursor: a byte position in a log file, or a live sequence number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogCursor {
    File(Cursor),
    Live(u64),
}

impl LogCursor {
    pub fn parse(text: &str) -> Result<Self> {
        match text.strip_prefix(LIVE_PREFIX).map(str::parse::<u64>) {
            Some(Ok(seq)) => Ok(Self::Live(seq)),
            _ => Cursor::parse(text).map(Self::File),
        }
    }
}

pub fn live_cursor(seq: u64) -> String {
    format!("{LIVE_PREFIX}{seq}")
}

/// The name of a live entry's area, when known.
pub fn area_name(area: u32) -> Option<&'static str> {
    name(AREA_NAMES, area)
}

pub fn entry_json(entry: &LogEntry) -> Value {
    json!({
        "seq": entry.seq,
        "area": entry.area,
        "area_name": name(AREA_NAMES, entry.area),
        "level": entry.level,
        "level_name": name(LEVEL_NAMES, entry.level),
        "message": entry.message,
    })
}

/// Whether `level` (lower-case name or number) names this live level.
pub fn level_matches(level: &str, value: u32) -> bool {
    level == value.to_string()
        || name(LEVEL_NAMES, value).is_some_and(|known| known.eq_ignore_ascii_case(level))
}

/// The top screen of the first stack whose top starts with `prefix`.
pub fn top_screen_matching<'a>(stacks: &'a [SceneStack], prefix: &str) -> Option<&'a str> {
    stacks
        .iter()
        .filter_map(|stack| stack.screens.last())
        .map(String::as_str)
        .find(|top| top.starts_with(prefix))
}

/// The top screen of the gameplay stack (the one holding `in_game_play_screen`), or None
/// when the player is not in a world.
pub fn gameplay_top(stacks: &[SceneStack]) -> Option<&str> {
    stacks
        .iter()
        .find(|stack| {
            stack
                .screens
                .iter()
                .any(|name| name == "in_game_play_screen")
        })
        .and_then(|stack| stack.screens.last())
        .map(String::as_str)
}

/// The Ore UI page the game keeps over the HUD during play; it takes no input of its own. The
/// game's public `hbui/routes.json` names its file.
const OREUI_GAMEPLAY: &str = "/hbui/gameplay.html";

/// Whether gameplay has the input: the gameplay stack's top is the HUD itself.
pub fn hud_on_top(top: &str) -> bool {
    top.starts_with("hud") || top == "in_game_play_screen" || top.ends_with(OREUI_GAMEPLAY)
}

/// The loading screen; its scene name also names the job it shows.
const LOADING_SCREEN: &str = "world_loading_progress_screen";
/// The jobs of a world this game hosts itself.
const LOCAL_LOADS: [&str; 2] = ["start_local_world_progress", "local_world_load"];

/// What the loading screens in `stacks` show: Some(None) for loading a local world,
/// Some(Some(job)) for another job, such as joining another host's game (named
/// `joining_xboxLive_game` for a friend's world), and None when no loading screen names its job.
pub fn loading_job(stacks: &[SceneStack]) -> Option<Option<&str>> {
    let mut found = None;
    for name in stacks.iter().flat_map(|stack| &stack.screens) {
        let Some(job) = name
            .strip_prefix(LOADING_SCREEN)
            .map(|rest| rest.trim_start_matches([' ', '-']))
            .filter(|job| !job.is_empty())
        else {
            continue;
        };
        if !LOCAL_LOADS.contains(&job) {
            return Some(Some(job));
        }
        found = Some(None);
    }
    found
}

/// Where the game is outside play, as the screen stacks show it.
#[derive(Debug, PartialEq, Eq)]
pub enum Menu<'a> {
    /// A gameplay stack exists: a world is open.
    InWorld,
    /// A world is loading.
    Loading,
    /// The main menu (`start_screen`) is on top.
    Main,
    /// An Ore UI page (the world list after Play or after leaving a world) is over the main
    /// menu.
    Page,
    /// Another screen is on top.
    Other(&'a str),
}

/// Where the game is outside play. The saving screen that shows while a world closes also
/// names the loading screen, so it does not count as loading.
pub fn menu(stacks: &[SceneStack]) -> Menu<'_> {
    if gameplay_top(stacks).is_some() {
        return Menu::InWorld;
    }
    let loading = stacks
        .iter()
        .flat_map(|stack| &stack.screens)
        .any(|name| name.contains("loading_progress") && !name.contains("saving"));
    if loading {
        return Menu::Loading;
    }
    for stack in stacks {
        let Some(at) = stack.screens.iter().position(|name| name == "start_screen") else {
            continue;
        };
        return match stack.screens[at + 1..].last() {
            None => Menu::Main,
            Some(top) if crate::oreui::is_page(top) => Menu::Page,
            Some(top) => Menu::Other(top),
        };
    }
    Menu::Other(
        stacks
            .iter()
            .find_map(|stack| stack.screens.last())
            .map_or("", String::as_str),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_menu_tells_the_main_menu_the_world_list_and_loading_apart() {
        let stacks = |screens: &[&[&str]]| -> Vec<SceneStack> {
            screens
                .iter()
                .enumerate()
                .map(|(id, names)| SceneStack {
                    id: id as u64,
                    screens: names.iter().map(|name| (*name).to_owned()).collect(),
                    frames_since_change: 0,
                })
                .collect()
        };
        let base = ["cubemap_background_screen", "debug_screen", "toast_screen"];
        let with = |extra: &[&'static str]| -> Vec<&'static str> { [&base[..], extra].concat() };
        let main = stacks(&[&with(&["start_screen"]), &[]]);
        assert_eq!(menu(&main), Menu::Main);
        let list = stacks(&[&with(&["start_screen", "ui://demo/hbui/index.html"]), &[]]);
        assert_eq!(menu(&list), Menu::Page);
        let saving = with(&[
            "start_screen",
            "ui://demo/hbui/index.html",
            "world_saving_progress_screen for world_loading_progress_screen",
        ]);
        assert_eq!(
            menu(&stacks(&[&saving, &[]])),
            Menu::Other("world_saving_progress_screen for world_loading_progress_screen")
        );
        let loading = with(&["start_screen", "world_loading_progress_screen"]);
        assert_eq!(menu(&stacks(&[&loading, &[]])), Menu::Loading);
        let playing = stacks(&[&base, &["in_game_play_screen", "hud_screen"]]);
        assert_eq!(menu(&playing), Menu::InWorld);
    }

    #[test]
    fn loading_screens_tell_local_worlds_from_other_jobs() {
        let stack = |screens: &[&str]| SceneStack {
            id: 1,
            screens: screens.iter().map(|s| (*s).to_owned()).collect(),
            frames_since_change: 0,
        };
        assert_eq!(loading_job(&[stack(&["start_screen"])]), None);
        assert_eq!(
            loading_job(&[stack(&["start_screen", "world_loading_progress_screen"])]),
            None,
            "no job named yet"
        );
        let local = stack(&[
            "start_screen",
            "world_loading_progress_screen start_local_world_progress",
            "world_loading_progress_screen local_world_load",
        ]);
        assert_eq!(loading_job(std::slice::from_ref(&local)), Some(None));
        let other = stack(&["start_screen", "world_loading_progress_screen demo_join"]);
        assert_eq!(loading_job(&[local, other]), Some(Some("demo_join")));
    }

    #[test]
    fn live_and_file_cursors_are_distinct() {
        assert_eq!(LogCursor::parse("live@42").unwrap(), LogCursor::Live(42));
        assert_eq!(live_cursor(7), "live@7");
        assert_eq!(
            LogCursor::parse("ContentLog__1.txt@10").unwrap(),
            LogCursor::File(Cursor {
                file: "ContentLog__1.txt".into(),
                offset: 10
            })
        );
        assert!(LogCursor::parse("live@x").is_err());
    }

    #[test]
    fn live_levels_match_by_number_or_name() {
        assert!(level_matches("3", 3));
        assert!(level_matches("ERROR", 3));
        assert!(!level_matches("warning", 3));
        let entry = LogEntry {
            seq: 1,
            area: 4,
            level: 3,
            message: "boom".into(),
        };
        let value = entry_json(&entry);
        assert_eq!(value["level"], 3);
        assert_eq!(value["level_name"], "error");
    }

    #[test]
    fn screens_match_on_the_top_of_any_stack() {
        let stack = |screens: &[&str]| SceneStack {
            id: 1,
            screens: screens.iter().map(|s| (*s).to_owned()).collect(),
            frames_since_change: 0,
        };
        let stacks = [
            stack(&["hud_screen", "pause_screen"]),
            stack(&[]),
            stack(&["chat_screen"]),
        ];
        assert_eq!(top_screen_matching(&stacks, "chat"), Some("chat_screen"));
        assert_eq!(top_screen_matching(&stacks, "pause"), Some("pause_screen"));
        assert_eq!(
            top_screen_matching(&stacks, "hud"),
            None,
            "covered by the pause screen"
        );
    }

    #[test]
    fn the_gameplay_stack_top_tells_whether_the_hud_has_the_input() {
        let stack = |screens: &[&str]| SceneStack {
            id: 1,
            screens: screens.iter().map(|s| (*s).to_owned()).collect(),
            frames_since_change: 0,
        };
        let overlay = stack(&["cubemap_background_screen", "debug_screen", "toast_screen"]);
        let paused = [
            stack(&[
                "in_game_play_screen",
                "hud_crosshair_screen",
                "hud_screen",
                "pause_screen",
            ]),
            overlay.clone(),
        ];
        assert_eq!(gameplay_top(&paused), Some("pause_screen"));
        assert!(!hud_on_top("pause_screen"));
        assert!(hud_on_top("ui://demo/hbui/gameplay.html"));
        let playing = [
            overlay.clone(),
            stack(&["in_game_play_screen", "hud_crosshair_screen", "hud_screen"]),
        ];
        assert!(hud_on_top(gameplay_top(&playing).unwrap()));
        assert_eq!(gameplay_top(&[overlay]), None, "not in a world");
    }
}
