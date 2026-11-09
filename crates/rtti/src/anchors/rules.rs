//! The anchor rules for every class the unstable layer hooks.
//!
//! Slot numbers and exact counts come from LeviLamina's headers, and the
//! capabilities check the same ones. Classes that may gain virtuals between
//! game versions ask for at least the slots their anchors need, and their
//! capabilities find what they use inside the vtable instead of at a fixed
//! slot. The strings are literals the game shows or logs, and lambda scopes
//! name methods that LeviLamina's headers list.

use std::sync::LazyLock;

use super::{
    Anchor::{After, Beside, Sibling, Slot, Stored},
    Rule,
    Slots::{AtLeast, Exact},
    Source::{Lambda, Literal},
};

mod oreui;
mod player_render;
mod world_state;

/// Every rule, in the order `resolve` reports them.
pub static RULES: LazyLock<Vec<Rule>> =
    LazyLock::new(|| [CORE, oreui::RULES, player_render::RULES, world_state::RULES].concat());

/// The rules for the capabilities that predate the per-capability files.
const CORE: &[Rule] = &[
    // `IMinecraftGame` may gain virtuals between game versions; the anchors
    // need slots up to 3.
    Rule {
        class: "MinecraftGame",
        offset: 0,
        slots: AtLeast(4),
        shared: None,
        anchors: &[
            Slot {
                slot: 3,
                source: Lambda("MinecraftGame::stop"),
            },
            Slot {
                slot: 1,
                source: Lambda("MinecraftGame::initialize"),
            },
        ],
    },
    Rule {
        class: "SceneStack",
        offset: 0,
        slots: Exact(70),
        shared: None,
        anchors: &[
            Slot {
                slot: 17,
                source: Lambda("SceneStack::forEachVisibleScreen"),
            },
            Slot {
                slot: 2,
                source: Lambda("SceneStack::setOptions"),
            },
        ],
    },
    Rule {
        class: "UIScene",
        offset: 0,
        slots: Exact(93),
        shared: None,
        anchors: &[
            Slot {
                slot: 19,
                source: Literal("UIScene - render"),
            },
            Slot {
                slot: 1,
                source: Lambda("UIScene::init"),
            },
        ],
    },
    Rule {
        class: "VisualTree",
        offset: 0,
        slots: Exact(15),
        shared: None,
        anchors: &[Slot {
            slot: 5,
            source: Lambda("VisualTree::measureControls"),
        }],
    },
    Rule {
        class: "TextComponent",
        offset: 0,
        slots: Exact(18),
        shared: None,
        anchors: &[Slot {
            slot: 16,
            source: Literal("newline_refresh"),
        }],
    },
    Rule {
        class: "ToggleComponent",
        offset: 0,
        slots: Exact(16),
        shared: None,
        anchors: &[
            Slot {
                slot: 13,
                source: Literal("#toggle_state"),
            },
            Stored {
                source: Literal("#toggle_state"),
                depth: 0,
            },
        ],
    },
    // The rule expects the UI control factory to read each component's JSON
    // properties, then call the component's constructor, which stores its
    // vtable.
    Rule {
        class: "ButtonComponent",
        offset: 0,
        slots: Exact(16),
        shared: None,
        anchors: &[Stored {
            source: Literal("pressed_control"),
            depth: 1,
        }],
    },
    Rule {
        class: "ScrollViewComponent",
        offset: 0,
        slots: Exact(16),
        shared: None,
        anchors: &[
            Slot {
                slot: 12,
                source: Literal("#scroll_bar_visible"),
            },
            Stored {
                source: Literal("#scrollbar_hit_bottom"),
                depth: 0,
            },
        ],
    },
    Rule {
        class: "ContentLogFileEndPoint",
        offset: 0,
        slots: Exact(3),
        // Its wrapper templates can share `log`; the capability hooks them all.
        shared: Some(1),
        anchors: &[Slot {
            slot: 1,
            source: Literal(".txt"),
        }],
    },
    Rule {
        class: "ContentLogFileEndPoint",
        offset: 0x18,
        slots: Exact(5),
        shared: Some(2),
        anchors: &[Sibling {
            class: "ContentLogFileEndPoint",
            offset: 0,
        }],
    },
    Rule {
        class: "ToastManager",
        offset: 0,
        slots: Exact(5),
        shared: None,
        anchors: &[Slot {
            slot: 1,
            source: Literal("recipeToast.newUnlock.tts"),
        }],
    },
    Rule {
        class: "CameraAPI",
        offset: 0,
        slots: Exact(17),
        shared: None,
        anchors: &[Stored {
            source: Literal("follow_orbit"),
            depth: 2,
        }],
    },
    Rule {
        class: "VanillaCameraAPI",
        offset: 0,
        slots: Exact(8),
        shared: None,
        anchors: &[
            Stored {
                source: Literal("follow_orbit"),
                depth: 2,
            },
            After {
                class: "CameraAPI",
                offset: 0,
            },
        ],
    },
    // `IClientInstance` may gain virtuals between game versions; LeviLamina's
    // header declares 419.
    Rule {
        class: "ClientInstance",
        offset: 0,
        slots: AtLeast(419),
        shared: None,
        anchors: &[
            Slot {
                slot: 3,
                source: Lambda("ClientInstance::init"),
            },
            Slot {
                slot: 27,
                source: Lambda("ClientInstance::startSubClientLateJoin"),
            },
        ],
    },
    Rule {
        class: "GuiData",
        offset: 0,
        slots: Exact(2),
        shared: None,
        anchors: &[
            Stored {
                source: Literal("ui_invert_overlay"),
                depth: 0,
            },
            Beside {
                offset: 8,
                slots: 21,
            },
        ],
    },
    Rule {
        class: "GuiData",
        offset: 8,
        slots: Exact(21),
        shared: None,
        anchors: &[Sibling {
            class: "GuiData",
            offset: 0,
        }],
    },
    Rule {
        class: "ServerInstance",
        offset: 0x18,
        slots: Exact(21),
        shared: None,
        anchors: &[
            Slot {
                slot: 4,
                source: Lambda("ServerInstance::onAppSuspended"),
            },
            Slot {
                slot: 5,
                source: Lambda("ServerInstance::onAppResumed"),
            },
        ],
    },
    Rule {
        class: "ServerInstance",
        offset: 0x38,
        slots: Exact(10),
        shared: None,
        anchors: &[Sibling {
            class: "ServerInstance",
            offset: 0x18,
        }],
    },
    // The `cohtml::IViewListener` subobject: 26 slots in LeviLamina's header.
    Rule {
        class: "OreUI::View",
        offset: 0x18,
        slots: Exact(26),
        shared: None,
        anchors: &[Slot {
            slot: 7,
            source: Literal("core:routing:not-found"),
        }],
    },
    // `OreUI::IView`: 24 slots in LeviLamina's header; `View` may add its own.
    Rule {
        class: "OreUI::View",
        offset: 0,
        slots: AtLeast(24),
        shared: None,
        anchors: &[Sibling {
            class: "OreUI::View",
            offset: 0x18,
        }],
    },
];
