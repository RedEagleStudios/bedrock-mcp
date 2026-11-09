//! Finding a class's vtable and replacing its slots.
//!
//! A class is found by its RTTI name first. A build without RTTI for the
//! class falls back to the string anchors in `rtti::anchors`, which resolve
//! once, before any capability replaces a slot.

use std::{
    collections::HashMap,
    fmt,
    sync::atomic::{AtomicUsize, Ordering},
};

use endpoint::log;
use rtti::{
    Image, Vtable,
    anchors::{self, Outcome, Slots},
};
use windows_sys::Win32::System::Memory::{PAGE_PROTECTION_FLAGS, PAGE_READWRITE, VirtualProtect};

/// How a vtable was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Found {
    Rtti,
    Anchors,
}

impl fmt::Display for Found {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Found::Rtti => "found by RTTI",
            Found::Anchors => "found by string anchors",
        })
    }
}

pub struct Resolver<'a> {
    pub image: &'a Image<'a>,
    /// Classes with RTTI; empty on a build without it.
    pub classes: &'a HashMap<String, Vec<Vtable>>,
    /// What each rule in [`anchors::RULES`] found, when RTTI lacks one of
    /// their classes.
    anchors: Option<Vec<Outcome>>,
}

/// A located vtable in memory.
pub struct Table {
    /// Address of slot 0.
    pub address: usize,
    pub slots: usize,
    pub found: Found,
}

impl<'a> Resolver<'a> {
    /// Resolves the anchor rules now if RTTI lacks any of their classes:
    /// they read the vtables as the build shipped them, so they must run
    /// before a capability replaces a slot.
    pub fn new(image: &'a Image<'a>, classes: &'a HashMap<String, Vec<Vtable>>) -> Self {
        let lacking = anchors::RULES
            .iter()
            .filter(|rule| rtti_table(classes, rule.class, rule.offset).is_none())
            .count();
        let anchors = (lacking > 0).then(|| {
            let start = std::time::Instant::now();
            let outcomes = anchors::resolve(image, &anchors::RULES);
            let found = outcomes
                .iter()
                .filter(|outcome| outcome.vtable().is_some())
                .count();
            log!(
                "{lacking} anchored classes lack RTTI; anchors found {found} of {} vtables in {:?}",
                outcomes.len(),
                start.elapsed()
            );
            outcomes
        });
        Resolver {
            image,
            classes,
            anchors,
        }
    }

    /// The vtable of class `name` for the subobject at `offset`, checked to
    /// have exactly `slots` slots. The error says why it was rejected.
    pub fn table(&self, name: &str, offset: u32, slots: usize) -> Result<Table, String> {
        self.find(name, offset, Slots::Exact(slots))
    }

    /// Like [`Resolver::table`], for a class that may gain virtuals between game
    /// versions: the vtable must have at least `slots` slots.
    pub fn table_at_least(&self, name: &str, offset: u32, slots: usize) -> Result<Table, String> {
        self.find(name, offset, Slots::AtLeast(slots))
    }

    fn find(&self, name: &str, offset: u32, slots: Slots) -> Result<Table, String> {
        if self.classes.contains_key(&rtti::decorated(name, false)) {
            let table = rtti_table(self.classes, name, offset)
                .ok_or_else(|| format!("{name} has no vtable for the subobject at {offset:#x}"))?;
            return self.checked(name, offset, slots, table, Found::Rtti);
        }
        let index = anchors::RULES
            .iter()
            .position(|rule| rule.class == name && rule.offset == offset)
            .ok_or_else(|| format!("{name} has no RTTI on this build and no string anchors"))?;
        let outcome = self
            .anchors
            .as_ref()
            .map(|outcomes| &outcomes[index])
            .ok_or_else(|| format!("{name} has no RTTI on this build"))?;
        if let Some(table) = outcome.vtable() {
            return self.checked(name, offset, slots, table, Found::Anchors);
        }
        Err(match outcome {
            Outcome::Ambiguous(starts) => format!(
                "{name} has no RTTI on this build and string anchors matched {} vtables",
                starts.len()
            ),
            Outcome::Missing(reason) => format!(
                "{name} has no RTTI on this build and string anchors did not find it: {reason}"
            ),
            Outcome::Found(_) | Outcome::Family(_) => format!("{name} has no RTTI on this build"),
        })
    }

    fn checked(
        &self,
        name: &str,
        offset: u32,
        slots: Slots,
        table: &Vtable,
        found: Found,
    ) -> Result<Table, String> {
        if !slots.admits(table.slots.len()) {
            return Err(format!(
                "{name} vtable at {offset:#x} ({found}) has {} slots, expected {slots}",
                table.slots.len()
            ));
        }
        Ok(Table {
            address: self.image.base as usize + table.rva as usize,
            slots: table.slots.len(),
            found,
        })
    }

    /// Every primary vtable with `table`'s slot count whose `slot` holds the
    /// same function as `table`'s. Wrapper templates such as
    /// `SelfRegisteringContentLogEndPoint<T>` get their own vtables that
    /// reuse `T`'s functions; live objects may use either.
    pub fn sharing(&self, table: &Table, slot: usize) -> Vec<Table> {
        let function = table.get(slot) as u64;
        let Some(rva) = self.image.rva_of(function) else {
            return Vec::new();
        };
        let at = |rva: u32| self.image.base as usize + rva as usize;
        match table.found {
            Found::Rtti => self
                .classes
                .values()
                .flatten()
                .filter(|candidate| {
                    candidate.offset == 0
                        && candidate.slots.len() == table.slots
                        && candidate.slots[slot] == rva
                })
                .map(|candidate| Table {
                    address: at(candidate.rva),
                    slots: candidate.slots.len(),
                    found: Found::Rtti,
                })
                .collect(),
            Found::Anchors => anchors::sharing(self.image, rva, slot, table.slots)
                .into_iter()
                .map(|start| Table {
                    address: at(start),
                    slots: table.slots,
                    found: Found::Anchors,
                })
                .collect(),
        }
    }
}

/// The RTTI vtable of `name` for the subobject at `offset`.
fn rtti_table<'c>(
    classes: &'c HashMap<String, Vec<Vtable>>,
    name: &str,
    offset: u32,
) -> Option<&'c Vtable> {
    classes
        .get(&rtti::decorated(name, false))?
        .iter()
        .find(|table| table.offset == offset)
}

impl Table {
    /// The function currently in `slot`.
    pub fn get(&self, slot: usize) -> usize {
        assert!(slot < self.slots);
        // SAFETY: the vtable is mapped read-only data inside the game image.
        unsafe { *(self.address as *const usize).add(slot) }
    }

    /// Replace `slot` with `function`, storing the previous one in
    /// `original` first so the hook can call through as soon as it runs.
    pub fn hook(&self, slot: usize, function: usize, original: &AtomicUsize) -> Result<(), String> {
        assert!(slot < self.slots);
        let entry = (self.address + slot * size_of::<usize>()) as *mut usize;
        original.store(self.get(slot), Ordering::Release);
        let mut old: PAGE_PROTECTION_FLAGS = 0;
        if unsafe { VirtualProtect(entry.cast(), size_of::<usize>(), PAGE_READWRITE, &mut old) }
            == 0
        {
            return Err("VirtualProtect refused the vtable page".into());
        }
        // SAFETY: the slot is writable now and a single aligned word.
        unsafe { AtomicUsize::from_ptr(entry) }.store(function, Ordering::Release);
        unsafe { VirtualProtect(entry.cast(), size_of::<usize>(), old, &mut old) };
        Ok(())
    }
}
