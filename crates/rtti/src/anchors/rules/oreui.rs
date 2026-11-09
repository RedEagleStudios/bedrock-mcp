//! Anchor rules for the `oreui` capability's classes.

use super::super::{
    Anchor::{Slot, Stored},
    Rule,
    Slots::Exact,
    Source::Import,
};

pub const RULES: &[Rule] = &[
    // The Ore UI library's log handler (`OreUI::Library::mLogHandler`, a
    // 2-slot `cohtml::Logging::ILogHandler`; LeviLamina:
    // `OreUI::EmptyLogHandler`). Its constructor calls Gameface's exported
    // base constructor, then stores the vtable; its deleting destructor in
    // slot 0 calls the exported base destructor.
    Rule {
        class: "OreUI::EmptyLogHandler",
        offset: 0,
        slots: Exact(2),
        shared: None,
        anchors: &[
            Stored {
                source: Import("??0ILogHandler@Logging@cohtml@@QEAA@XZ"),
                depth: 0,
            },
            Slot {
                slot: 0,
                source: Import("??1ILogHandler@Logging@cohtml@@UEAA@XZ"),
            },
        ],
    },
];
