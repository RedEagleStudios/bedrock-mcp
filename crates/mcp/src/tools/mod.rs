//! Every MCP tool, declared once: its toolset, behaviour hints, handler and the definition
//! `tools/list` shows (name, description, input schema). One module per toolset.

mod actions;
mod automation;
mod dev;
mod docs;
mod game;
mod gametest;
mod media;
mod player;
mod reference;
mod schema;
mod world;

pub use reference::markdown as reference;

use std::{env, sync::LazyLock};

use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::{
    error::{Error, Result},
    rpc::Content,
    session::Session,
};

/// Lists the toolsets to offer, comma-separated; unset or empty offers all of them.
pub const TOOLSETS_SETTING: &str = "BEDROCK_MCP_TOOLSETS";

/// Tools are offered by toolset, so a client can list only what its work needs; `summary`
/// says what each covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Toolset {
    Game,
    Player,
    World,
    Actions,
    Gametest,
    Automation,
    Media,
    Dev,
    Docs,
}

impl Toolset {
    pub const ALL: [Self; 9] = [
        Self::Game,
        Self::Player,
        Self::World,
        Self::Actions,
        Self::Gametest,
        Self::Automation,
        Self::Media,
        Self::Dev,
        Self::Docs,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Game => "game",
            Self::Player => "player",
            Self::World => "world",
            Self::Actions => "actions",
            Self::Gametest => "gametest",
            Self::Automation => "automation",
            Self::Media => "media",
            Self::Dev => "dev",
            Self::Docs => "docs",
        }
    }

    /// What the toolset covers, for the generated reference.
    pub fn summary(self) -> &'static str {
        match self {
            Self::Game => {
                "Launching, stopping and inspecting the game, and opening, leaving and joining \
                 worlds and servers."
            }
            Self::Player => "The player: state, inventory, looking and the hotbar.",
            Self::World => "Entities, blocks, containers, items, raycasts and world facts.",
            Self::Actions => "Player input, menu clicks and gameplay actions.",
            Self::Gametest => {
                "GameTest simulated players and test runs, in the game or on a dedicated server."
            }
            Self::Automation => "The WebSocket session: commands, chat, events and waits.",
            Self::Media => "Screenshots, recordings and scripted cameras.",
            Self::Dev => "Logs, packs, worlds, skins and the unstable layer.",
            Self::Docs => {
                "Pack validation, pack fixes, Bedrock JSON schemas and Molang; needs no game."
            }
        }
    }
}

/// The toolsets a session offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toolsets(Vec<Toolset>);

impl Toolsets {
    pub fn all() -> Self {
        Self(Toolset::ALL.to_vec())
    }

    /// Parse a comma-separated list of toolset names; empty means all.
    pub fn parse(spec: &str) -> Result<Self> {
        let mut chosen = Vec::new();
        for name in spec
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            let Some(toolset) = Toolset::ALL.into_iter().find(|set| set.name() == name) else {
                let known: Vec<_> = Toolset::ALL.iter().map(|set| set.name()).collect();
                return Err(Error::msg(format!(
                    "Unknown toolset {name:?} in {TOOLSETS_SETTING}; use any of {}.",
                    known.join(", ")
                )));
            };
            if !chosen.contains(&toolset) {
                chosen.push(toolset);
            }
        }
        Ok(if chosen.is_empty() {
            Self::all()
        } else {
            Self(chosen)
        })
    }

    pub fn from_env() -> Result<Self> {
        Self::parse(&env::var(TOOLSETS_SETTING).unwrap_or_default())
    }

    pub fn contains(&self, toolset: Toolset) -> bool {
        self.0.contains(&toolset)
    }
}

/// MCP tool annotations: what calling the tool does to the game and the files around it.
#[derive(Clone, Copy, Debug)]
pub struct Hints {
    read_only: bool,
    destructive: bool,
    idempotent: bool,
    open_world: bool,
}

impl Hints {
    /// Only reads.
    pub const READ: Self = Self {
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
    };
    /// Changes something; calling again acts again.
    pub const ACT: Self = Self {
        read_only: false,
        destructive: false,
        idempotent: false,
        open_world: false,
    };
    /// Changes something to a given state; calling again changes nothing more.
    pub const SET: Self = Self {
        idempotent: true,
        ..Self::ACT
    };
    /// Ends or replaces something that cannot be brought back by calling again.
    pub const DESTROY: Self = Self {
        destructive: true,
        ..Self::ACT
    };

    /// Also reaches beyond this machine (remote servers).
    pub const fn open_world(self) -> Self {
        Self {
            open_world: true,
            ..self
        }
    }
}

pub type Handler = fn(&mut Session, Value) -> Result<Vec<Content>>;

pub struct Tool {
    toolset: Toolset,
    hints: Hints,
    run: Handler,
    /// `name`, `title`, `description` and `inputSchema`.
    definition: Value,
}

impl Tool {
    /// A tool from its `name`, `description` and `inputSchema`; `title` is its display name.
    pub fn new(
        toolset: Toolset,
        hints: Hints,
        title: &'static str,
        run: Handler,
        mut definition: Value,
    ) -> Self {
        definition["title"] = title.into();
        Self {
            toolset,
            hints,
            run,
            definition,
        }
    }

    pub fn name(&self) -> &str {
        self.definition["name"].as_str().unwrap_or_default()
    }

    /// The `tools/list` entry, with the hints as annotations.
    fn listed(&self) -> Value {
        let mut entry = self.definition.clone();
        entry["annotations"] = json!({
            "title": self.definition["title"],
            "readOnlyHint": self.hints.read_only,
            "destructiveHint": self.hints.destructive,
            "idempotentHint": self.hints.idempotent,
            "openWorldHint": self.hints.open_world,
        });
        entry
    }
}

static REGISTRY: LazyLock<Vec<Tool>> = LazyLock::new(|| {
    [
        game::tools(),
        player::tools(),
        world::tools(),
        actions::tools(),
        gametest::tools(),
        automation::tools(),
        media::tools(),
        dev::tools(),
        docs::tools(),
    ]
    .into_iter()
    .flatten()
    .collect()
});

/// The `tools/list` entries of the offered toolsets.
pub fn definitions(offered: &Toolsets) -> Vec<Value> {
    REGISTRY
        .iter()
        .filter(|tool| offered.contains(tool.toolset))
        .map(Tool::listed)
        .collect()
}

fn find(offered: &Toolsets, name: &str) -> Option<&'static Tool> {
    REGISTRY
        .iter()
        .find(|tool| tool.name() == name && offered.contains(tool.toolset))
}

/// Whether an offered toolset holds `name`.
pub fn offers(offered: &Toolsets, name: &str) -> bool {
    find(offered, name).is_some()
}

/// Run an offered tool.
pub fn call(
    offered: &Toolsets,
    session: &mut Session,
    name: &str,
    arguments: Value,
) -> Result<Vec<Content>> {
    let Some(tool) = find(offered, name) else {
        return Err(Error::msg(format!("Unknown tool: {name}")));
    };
    (tool.run)(session, arguments)
}

/// Deserialize tool arguments, reporting schema mismatches as tool errors.
pub fn arguments<T: DeserializeOwned>(arguments: Value) -> Result<T> {
    serde_json::from_value(arguments)
        .map_err(|error| Error::msg(format!("Invalid arguments: {error}")))
}

/// A tool result that is one JSON value.
pub fn reply(value: Value) -> Result<Vec<Content>> {
    Ok(vec![Content::json(&value)])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed(name: &str) -> Value {
        definitions(&Toolsets::all())
            .into_iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("{name} is not listed"))
    }

    #[test]
    fn every_tool_has_a_unique_name_title_description_and_object_schema() {
        let tools = definitions(&Toolsets::all());
        let mut names: Vec<_> = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        for tool in &tools {
            assert!(
                tool["description"]
                    .as_str()
                    .is_some_and(|text| text.len() > 20),
                "{}",
                tool["name"]
            );
            assert!(
                tool["title"]
                    .as_str()
                    .is_some_and(|title| !title.is_empty())
            );
            assert_eq!(tool["inputSchema"]["type"], "object");
        }
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "duplicate tool names");
    }

    #[test]
    fn toolsets_limit_what_is_listed_and_callable() {
        let only_dev = Toolsets::parse(" dev ,dev").unwrap();
        assert_eq!(only_dev, Toolsets(vec![Toolset::Dev]));
        let names: Vec<_> = definitions(&only_dev)
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect();
        assert!(names.contains(&"content_log_read".to_owned()));
        assert!(!names.contains(&"game_launch".to_owned()));
        assert_eq!(Toolsets::parse("").unwrap(), Toolsets::all());
        let error = Toolsets::parse("game,cheats").unwrap_err().to_string();
        assert!(
            error.contains("\"cheats\"") && error.contains("automation"),
            "{error}"
        );
    }

    #[test]
    fn annotations_follow_the_hints() {
        let status = listed("game_status");
        assert_eq!(status["annotations"]["readOnlyHint"], true);
        let stop = listed("game_stop");
        assert_eq!(stop["annotations"]["readOnlyHint"], false);
        assert_eq!(stop["annotations"]["destructiveHint"], true);
        assert_eq!(listed("server_join")["annotations"]["openWorldHint"], true);
    }

    #[test]
    fn screenshot_offers_ocr_over_a_logical_region() {
        let schema = &listed("frame_capture")["inputSchema"]["properties"];
        assert_eq!(schema["ocr"]["type"], "boolean");
        assert_eq!(schema["region"]["type"], "object");
        assert_eq!(
            schema["region"]["required"],
            json!(["x", "y", "width", "height"])
        );
        assert_eq!(schema["region"]["additionalProperties"], false);
    }

    #[test]
    fn wait_for_documents_the_text_condition() {
        let condition = &listed("wait_until")["inputSchema"]["properties"]["condition"];
        assert!(
            condition["description"]
                .as_str()
                .unwrap()
                .contains("{\"text\": {\"contains\"")
        );
    }
}
