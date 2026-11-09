//! Anchor rules for the `player_render` capability's classes.
//!
//! `AnimationComponent.h` declares 10 virtuals and `ActorAnimationPlayer.h`
//! 11; the subclasses add none. The players' log lines come from
//! `applyToPose` (slot 1), which can leave them to a helper it calls.

use super::super::{
    Anchor::{Calls, Slot},
    Rule,
    Slots::Exact,
    Source::{Lambda, Literal},
};

pub const RULES: &[Rule] = &[
    Rule {
        class: "ClientAnimationComponent",
        offset: 0,
        slots: Exact(10),
        shared: None,
        anchors: &[Slot {
            slot: 4,
            source: Lambda("ClientAnimationComponent::ensureClientAnimationComponentIsInitialized"),
        }],
    },
    Rule {
        class: "ActorAnimationControllerPlayer",
        offset: 0,
        slots: Exact(11),
        shared: None,
        anchors: &[
            Slot {
                slot: 1,
                source: Literal(
                    "transitioning to a null animation - check your log to see if errors were found in that animation",
                ),
            },
            Calls {
                slot: 1,
                source: Literal(
                    "error: null/out-of-bounds animation controller state for index %d of %s with %d animations",
                ),
                depth: 1,
            },
        ],
    },
    Rule {
        class: "ActorAnimationControllerStatePlayer",
        offset: 0,
        slots: Exact(11),
        shared: None,
        anchors: &[Calls {
            slot: 1,
            source: Literal("error: null animation for animation %d of state %s of %s"),
            depth: 1,
        }],
    },
    // `buildBoneToPartMapping`.
    Rule {
        class: "ActorSkeletalAnimationPlayer",
        offset: 0,
        slots: Exact(11),
        shared: None,
        anchors: &[Slot {
            slot: 3,
            source: Literal(
                "Animation component's bone orientations missing expected skeletal hierarchy index!?",
            ),
        }],
    },
];
