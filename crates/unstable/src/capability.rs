//! Each capability's state, as reported by `Status`.

use std::sync::Mutex;

use endpoint::log;
use protocol::unstable::{Capability, CapabilityState};

use crate::vtable::Found;

static CAPABILITIES: Mutex<Vec<Capability>> = Mutex::new(Vec::new());
/// How each capability's classes were found, appended to its detail.
static FOUND: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// Record how `name` found its classes: "found by RTTI" when all agree,
/// else each class with its way. Every later detail of `name` ends with it.
pub fn found(name: &str, classes: &[(&str, Found)]) {
    let Some((_, first)) = classes.first() else {
        return;
    };
    let note = if classes.iter().all(|(_, way)| way == first) {
        first.to_string()
    } else {
        classes
            .iter()
            .map(|(class, way)| format!("{class} {way}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut notes = FOUND.lock().unwrap_or_else(|poison| poison.into_inner());
    notes.retain(|(capability, _)| capability != name);
    notes.push((name.to_owned(), note));
}

/// Record `name`'s state; logs every change.
pub fn set(name: &str, state: CapabilityState, detail: impl Into<String>) {
    let mut detail = detail.into();
    if let Some((_, note)) = FOUND
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .iter()
        .find(|(capability, _)| capability == name)
    {
        detail = format!("{detail} ({note})");
    }
    let mut all = CAPABILITIES
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    match all.iter_mut().find(|capability| capability.name == name) {
        Some(capability) if capability.state == state && capability.detail == detail => return,
        Some(capability) => {
            capability.state = state;
            capability.detail = detail.clone();
        }
        None => all.push(Capability {
            name: name.into(),
            state,
            detail: detail.clone(),
        }),
    }
    log!("capability {name}: {state:?} ({detail})");
}

pub fn all() -> Vec<Capability> {
    CAPABILITIES
        .lock()
        .map(|all| all.clone())
        .unwrap_or_default()
}
