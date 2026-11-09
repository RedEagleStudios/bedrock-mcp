//! Anchor rules for the `world_state` capability's classes.

use super::super::{Anchor::Holds, Rule, Slots::AtLeast, Source};

pub const RULES: &[Rule] = &[
    // `ServerLevel` may gain virtuals between game versions, so `_subTick` can
    // move: LeviLamina's headers give `ILevel` 405 virtuals and `Level` 5 more,
    // `_subTick` among them. The override builds a lambda of its own and
    // names its profiler scope; the rule resolves uniquely on the public game.
    Rule {
        class: "ServerLevel",
        offset: 0,
        slots: AtLeast(410),
        shared: None,
        anchors: &[
            Holds {
                source: Source::Lambda("ServerLevel::_subTick"),
            },
            Holds {
                source: Source::Literal("Server Level - tick"),
            },
        ],
    },
];
