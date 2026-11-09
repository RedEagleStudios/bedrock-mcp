//! MCP prompts: ready flows that name the exact tools and arguments for common jobs, offered
//! through `prompts/list` and `prompts/get`. Each text is a template whose `{{name}}`
//! placeholders take the prompt's arguments.

use serde_json::{Map, Value, json};

/// One argument a prompt takes.
pub struct Argument {
    pub name: &'static str,
    pub description: &'static str,
    /// Used when the client leaves the argument out; a required argument has none.
    pub default: Option<&'static str>,
}

impl Argument {
    pub fn required(&self) -> bool {
        self.default.is_none()
    }
}

/// One prompt: its listing and its template.
pub struct Prompt {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub arguments: &'static [Argument],
    template: &'static str,
}

const ITEM: Argument = Argument {
    name: "item",
    description: "The item id to test, e.g. example:ruby_sword",
    default: None,
};
const WORLD: Argument = Argument {
    name: "world",
    description: "The world's name as the world list shows it, or its folder id (see worlds_list)",
    default: None,
};
const TARGET: Argument = Argument {
    name: "target",
    description: "The entity type to fight (default minecraft:zombie)",
    default: Some("minecraft:zombie"),
};
const DISTANCE: Argument = Argument {
    name: "distance",
    description: "How many blocks ahead of the player the target spawns (default 3)",
    default: Some("3"),
};
const SPOT: Argument = Argument {
    name: "spot",
    description: "The scene_save spot the fight starts from; the flow saves it once when the \
        world has none by that name (default arena)",
    default: Some("arena"),
};

/// The setup both combat prompts share: launch, open, connect, reset (which warms up).
macro_rules! combat_setup {
    ($perspective:literal) => {
        concat!(
            "\
1. game_launch {} (it only attaches when the game already runs).
2. world_open {name: \"{{world}}\"}, then automation_connect {}. When its reply shows bridge: \
false, the test still runs in single player on native reads and vanilla commands, but approach \
and the inventory and block reads need the Bridge pack. Never install or add the pack yourself: \
tell the user what it would add and ask. Only when they agree, run packs_install {}, then \
world_leave {}, world_packs_set for \"{{world}}\", and world_open again.
3. scene_reset {spot: \"{{spot}}\", clear_entities: true, time: \"noon\", weather: \"clear\", \
difficulty: \"normal\", gamemode: \"survival\", clear_inventory: true, give: [{item: \"{{item}}\", \
slot: 0}], heal: true, hotbar: 0, perspective: \"",
            $perspective,
            "\", settle_ms: 500}. Check ok and spot_problems in its reply. When the spot \
is unknown, stand on open, flat ground with room ahead (movement_set {to: {x, y, z}} or \
player_set), run scene_save {name: \"{{spot}}\"} once, then scene_reset again.
4. scene_reset ends with a warm-up, so the held item's attachable animates: check warm_up.done \
and warm_up.detail, which says whether the attachable animates. Only when done is false, warm up by hand: hotbar_select {slot: 1}, \
hotbar_select {slot: 0}, then movement_set {forward: 1, duration_ms: 250}.
"
        )
    };
}

/// The fight itself, as one action_sequence call.
macro_rules! combat_sequence {
    () => {
        "\
5. Fight in one action_sequence call, so no model turn falls inside the fight:
{\"record\": true, \"timeout_ms\": 30000, \"max_failures\": 5, \"steps\": [
  {\"action\": \"spawn\", \"args\": {\"type\": \"{{target}}\", \"ahead\": {{distance}}}, \"as\": \"T\"},
  {\"action\": \"attack\", \"args\": {\"entity_id\": \"$T\", \"approach\": true},
   \"repeat\": {\"every_ticks\": 12, \"until\": {\"event\": {\"name\": \"entity_die\", \"contains\": \"$T\"}}}}
]}
If {{item}} acts on use instead of on attack (a bow, a throwable that throws on release), replace \
the attack step with {\"action\": \"look\", \"args\": {\"entity_id\": \"$T\"}} and then \
{\"action\": \"item_use\", \"args\": {\"mode\": \"hold\", \"ms\": 400}} with the attack step's \
repeat.
"
    };
}

pub const PROMPTS: &[Prompt] = &[
    Prompt {
        name: "playtest_item",
        title: "Play-test an item",
        description: "Test an item in a world against a spawned entity in one recorded action_sequence, \
            then report what happened from its summary.",
        arguments: &[ITEM, WORLD, TARGET, DISTANCE, SPOT],
        template: concat!(
            "Play-test the item {{item}} in the world \"{{world}}\" against {{target}}. Use the \
             steps below as written; each is one tool call.\n\n",
            combat_setup!("first_person"),
            combat_sequence!(),
            "\
6. Read the reply's summary, failed_steps and recording: summary.targets has, per spawned \
entity, its hits, misses, killed_by (null when it lived), final.state and reasons (why actions \
did not confirm: out_of_reach, blocked, cooldown, no_item, projectile_in_flight, no_event, \
no_damage, error); summary.totals adds them up; recording.path is the video. failed_steps has one line per \
step that went wrong; sequence_status {run_id, verbose: true} returns every step whole when you \
need an outcome.
7. When the target did not die or actions failed, read content_log_read {summary: true} for pack \
and script errors, and game_status for attention (death or disconnect screens).

Report in a few lines: the item, whether it killed {{target}}, hits and time to kill, failures with \
their reasons, content log errors, and the video path.",
        ),
    },
    Prompt {
        name: "record_combat",
        title: "Record a combat clip",
        description: "Record a clean clip of an item fighting a spawned entity: the whole setup \
            happens before the recording starts, and the fight runs in one action_sequence.",
        arguments: &[ITEM, WORLD, TARGET, DISTANCE, SPOT],
        template: concat!(
            "Record a clean combat clip of {{item}} against {{target}} in the world \"{{world}}\". \
             The recording must show only the fight: no menus, no setup, no pauses while a model \
             thinks. Use the steps below as written.\n\n",
            combat_setup!("third_person_back"),
            combat_sequence!(),
            "\
   record: true starts the video right before the first step and stops it when the last one \
ends, so do all setup before this call and nothing in between.
6. Read the reply's summary.targets for whether the target died (killed_by, final.state) and \
recording for the video's path and length. When the fight failed, fix the cause (summary reasons \
say why actions did not confirm) and run steps 3 to 5 again instead of keeping a bad clip.
7. Check the clip: video_analyze {path: \"<the recording's path>\"} lists sudden jumps (one-frame \
pops, snaps); attack swings show up too, so judge them on its sheets.

Report the video path, its length, whether {{target}} died, and any jumps that are not swings.",
        ),
    },
    Prompt {
        name: "debug_pack_errors",
        title: "Debug pack errors",
        description: "Find out why a pack misbehaves: read the content log, validate the pack, \
            check that the world applies it, and reload.",
        arguments: &[
            Argument {
                name: "pack",
                description: "The behavior or resource pack folder on this machine",
                default: None,
            },
            WORLD,
        ],
        template: "\
Find and fix the errors of the pack at {{pack}}, used in the world \"{{world}}\".

1. content_log_read {summary: true}: pack and script errors of this game session, grouped by \
cause. Keep the returned cursor.
2. pack_validate {path: \"{{pack}}\"}: the pack's own problems, without the game. Fix errors \
before warnings.
3. worlds_list {}: find \"{{world}}\" and check that its behavior_packs or resource_packs list the \
pack. A pack with name null is not installed where the game looks.
4. When the world does not apply the pack: world_leave {} if the game is in that world, then \
world_packs_set {world: \"{{world}}\", behavior: [...], resource: [...]} with the full lists, \
then world_open {name: \"{{world}}\"} and automation_connect {}.
5. After editing the pack while in the world: command_execute {command: \"reload all\"}, which \
reopens the world and picks up the changes.
6. content_log_read {since: \"<cursor from step 1>\", summary: true}: only the errors that came \
after your changes. Repeat 2, 5 and 6 until it is empty.

Report each error you found, its cause, and the fix.",
    },
    Prompt {
        name: "test_ui_flow",
        title: "Test a UI flow",
        description: "Click through menus or add-on forms by their labels, checking each screen \
            with the UI tree.",
        arguments: &[
            Argument {
                name: "clicks",
                description: "The labels to click in order, separated by >, e.g. Settings > Video",
                default: None,
            },
            Argument {
                name: "expect",
                description: "What the last screen must show (default: the screen the last \
                    click opens)",
                default: Some("the screen the last click opens"),
            },
        ],
        template: "\
Click through {{clicks}} and check that the result shows {{expect}}.

1. game_status {}: check attention (it must be null) and which screen is on top.
2. ui_tree {}: the controls a player can use or read on the current screen, with their labels. \
Ore UI screens (source oreui) list their elements the same way.
3. For each label in {{clicks}}: ui_click {text: \"<label>\"}, then ui_tree {} to confirm the new \
screen. Pass exact: true when several controls share the start of the label, and scroll: true \
when the label is in a long list.
4. When a click does not change the screen or an action stops confirming, read game_status {} \
for attention (a death, disconnect or error screen) and deal with it first. When ui_tree has no \
tree for the screen, frame_capture {ocr: true} reads its text.
5. At the end, check that ui_tree shows {{expect}}.

Report each click with the screen it opened, and whether the result matched.",
    },
];

/// The `prompts/list` entries.
pub fn list() -> Vec<Value> {
    PROMPTS
        .iter()
        .map(|prompt| {
            json!({
                "name": prompt.name,
                "title": prompt.title,
                "description": prompt.description,
                "arguments": prompt.arguments.iter().map(|argument| json!({
                    "name": argument.name,
                    "description": argument.description,
                    "required": argument.required(),
                })).collect::<Vec<_>>(),
            })
        })
        .collect()
}

/// The `prompts/get` result for `name` with `arguments` (strings by argument name), or why
/// it cannot be built.
pub fn get(name: &str, arguments: &Map<String, Value>) -> Result<Value, String> {
    let prompt = PROMPTS
        .iter()
        .find(|prompt| prompt.name == name)
        .ok_or_else(|| format!("Unknown prompt: {name}"))?;
    if let Some(unknown) = arguments.keys().find(|key| {
        !prompt
            .arguments
            .iter()
            .any(|argument| argument.name == *key)
    }) {
        return Err(format!("Prompt {name} takes no argument {unknown}"));
    }
    let mut text = prompt.template.to_owned();
    for argument in prompt.arguments {
        let value = match arguments.get(argument.name) {
            Some(Value::String(value)) if !value.is_empty() => value.clone(),
            Some(Value::Number(value)) => value.to_string(),
            Some(Value::String(_) | Value::Null) | None => match argument.default {
                Some(default) => default.to_owned(),
                None => return Err(format!("Prompt {name} needs argument {}", argument.name)),
            },
            Some(_) => return Err(format!("Argument {} must be a string", argument.name)),
        };
        text = text.replace(&format!("{{{{{}}}}}", argument.name), &value);
    }
    Ok(json!({
        "description": prompt.description,
        "messages": [{ "role": "user", "content": { "type": "text", "text": text } }],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(result: &Value) -> &str {
        result["messages"][0]["content"]["text"].as_str().unwrap()
    }

    #[test]
    fn every_placeholder_is_an_argument() {
        for prompt in PROMPTS {
            let arguments = prompt
                .arguments
                .iter()
                .map(|argument| (argument.name.to_owned(), json!("x")))
                .collect();
            let result = get(prompt.name, &arguments).unwrap();
            assert!(
                !text(&result).contains("{{"),
                "{} leaves a placeholder",
                prompt.name
            );
        }
    }

    #[test]
    fn list_shows_arguments_and_which_are_required() {
        let listed = list();
        let playtest = listed
            .iter()
            .find(|prompt| prompt["name"] == "playtest_item")
            .unwrap();
        assert!(playtest["title"].is_string() && playtest["description"].is_string());
        let required: Vec<(&str, bool)> = playtest["arguments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|argument| {
                (
                    argument["name"].as_str().unwrap(),
                    argument["required"].as_bool().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            required,
            [
                ("item", true),
                ("world", true),
                ("target", false),
                ("distance", false),
                ("spot", false)
            ]
        );
    }

    #[test]
    fn get_substitutes_arguments_and_defaults() {
        let arguments = json!({ "item": "example:ruby_sword", "world": "Test World" });
        let result = get("playtest_item", arguments.as_object().unwrap()).unwrap();
        assert_eq!(result["messages"][0]["role"], "user");
        assert_eq!(result["messages"][0]["content"]["type"], "text");
        let text = text(&result);
        assert!(text.contains("world_open {name: \"Test World\"}"));
        assert!(text.contains("give: [{item: \"example:ruby_sword\", slot: 0}]"));
        assert!(text.contains("\"type\": \"minecraft:zombie\", \"ahead\": 3"));
        assert!(text.contains("scene_reset {spot: \"arena\""));
    }

    #[test]
    fn get_refuses_missing_unknown_and_misnamed() {
        let only_item = json!({ "item": "a:b" });
        assert!(get("playtest_item", only_item.as_object().unwrap()).is_err());
        let extra = json!({ "item": "a:b", "world": "w", "speed": "2" });
        assert!(get("playtest_item", extra.as_object().unwrap()).is_err());
        assert!(get("no_such_prompt", &Map::new()).is_err());
    }
}
