//! Controls: W3C key codes, mouse buttons, and `key.*` bindings from options.txt.

use std::collections::BTreeMap;

use protocol::mouse;

use crate::error::{Result, bail};

/// A Windows key: virtual key, set-1 scan code, and whether it is an extended key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub vk: u8,
    pub scan: u8,
    pub extended: bool,
}

/// What a resolved control presses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Key(Key),
    /// A [`protocol::mouse`] bit.
    Button(u32),
}

/// A physical control, named the way the MCP client sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Control {
    pub name: String,
    pub kind: Kind,
}

/// Keyboard-and-mouse bindings by action (`key.jump`); positive codes are virtual keys,
/// negative ones mouse buttons.
pub type Bindings = BTreeMap<String, Vec<i32>>;

const fn k(name: &'static str, vk: u8, scan: u8, extended: bool) -> (&'static str, Key) {
    (name, Key { vk, scan, extended })
}

/// W3C UI Events `code` names. Order matters for [`key_for_vk`]: the first non-extended
/// entry for a virtual key names it.
#[rustfmt::skip]
const KEYS: &[(&str, Key)] = &[
    k("KeyA", 0x41, 0x1E, false), k("KeyB", 0x42, 0x30, false), k("KeyC", 0x43, 0x2E, false),
    k("KeyD", 0x44, 0x20, false), k("KeyE", 0x45, 0x12, false), k("KeyF", 0x46, 0x21, false),
    k("KeyG", 0x47, 0x22, false), k("KeyH", 0x48, 0x23, false), k("KeyI", 0x49, 0x17, false),
    k("KeyJ", 0x4A, 0x24, false), k("KeyK", 0x4B, 0x25, false), k("KeyL", 0x4C, 0x26, false),
    k("KeyM", 0x4D, 0x32, false), k("KeyN", 0x4E, 0x31, false), k("KeyO", 0x4F, 0x18, false),
    k("KeyP", 0x50, 0x19, false), k("KeyQ", 0x51, 0x10, false), k("KeyR", 0x52, 0x13, false),
    k("KeyS", 0x53, 0x1F, false), k("KeyT", 0x54, 0x14, false), k("KeyU", 0x55, 0x16, false),
    k("KeyV", 0x56, 0x2F, false), k("KeyW", 0x57, 0x11, false), k("KeyX", 0x58, 0x2D, false),
    k("KeyY", 0x59, 0x15, false), k("KeyZ", 0x5A, 0x2C, false),
    k("Digit0", 0x30, 0x0B, false), k("Digit1", 0x31, 0x02, false), k("Digit2", 0x32, 0x03, false),
    k("Digit3", 0x33, 0x04, false), k("Digit4", 0x34, 0x05, false), k("Digit5", 0x35, 0x06, false),
    k("Digit6", 0x36, 0x07, false), k("Digit7", 0x37, 0x08, false), k("Digit8", 0x38, 0x09, false),
    k("Digit9", 0x39, 0x0A, false),
    k("F1", 0x70, 0x3B, false), k("F2", 0x71, 0x3C, false), k("F3", 0x72, 0x3D, false),
    k("F4", 0x73, 0x3E, false), k("F5", 0x74, 0x3F, false), k("F6", 0x75, 0x40, false),
    k("F7", 0x76, 0x41, false), k("F8", 0x77, 0x42, false), k("F9", 0x78, 0x43, false),
    k("F10", 0x79, 0x44, false), k("F11", 0x7A, 0x57, false), k("F12", 0x7B, 0x58, false),
    k("Numpad0", 0x60, 0x52, false), k("Numpad1", 0x61, 0x4F, false), k("Numpad2", 0x62, 0x50, false),
    k("Numpad3", 0x63, 0x51, false), k("Numpad4", 0x64, 0x4B, false), k("Numpad5", 0x65, 0x4C, false),
    k("Numpad6", 0x66, 0x4D, false), k("Numpad7", 0x67, 0x47, false), k("Numpad8", 0x68, 0x48, false),
    k("Numpad9", 0x69, 0x49, false),
    k("NumpadAdd", 0x6B, 0x4E, false), k("NumpadSubtract", 0x6D, 0x4A, false),
    k("NumpadMultiply", 0x6A, 0x37, false), k("NumpadDivide", 0x6F, 0x35, true),
    k("NumpadDecimal", 0x6E, 0x53, false), k("NumpadEnter", 0x0D, 0x1C, true),
    k("Escape", 0x1B, 0x01, false), k("Backquote", 0xC0, 0x29, false),
    k("Minus", 0xBD, 0x0C, false), k("Equal", 0xBB, 0x0D, false),
    k("Backspace", 0x08, 0x0E, false), k("Tab", 0x09, 0x0F, false),
    k("BracketLeft", 0xDB, 0x1A, false), k("BracketRight", 0xDD, 0x1B, false),
    k("Backslash", 0xDC, 0x2B, false), k("Semicolon", 0xBA, 0x27, false),
    k("Quote", 0xDE, 0x28, false), k("Comma", 0xBC, 0x33, false),
    k("Period", 0xBE, 0x34, false), k("Slash", 0xBF, 0x35, false),
    k("Enter", 0x0D, 0x1C, false), k("Space", 0x20, 0x39, false),
    k("CapsLock", 0x14, 0x3A, false),
    k("ShiftLeft", 0x10, 0x2A, false), k("ShiftRight", 0x10, 0x36, false),
    k("ControlLeft", 0x11, 0x1D, false), k("ControlRight", 0x11, 0x1D, true),
    k("AltLeft", 0x12, 0x38, false), k("AltRight", 0x12, 0x38, true),
    k("MetaLeft", 0x5B, 0x5B, true), k("ContextMenu", 0x5D, 0x5D, true),
    k("ArrowUp", 0x26, 0x48, true), k("ArrowDown", 0x28, 0x50, true),
    k("ArrowLeft", 0x25, 0x4B, true), k("ArrowRight", 0x27, 0x4D, true),
    k("Insert", 0x2D, 0x52, true), k("Delete", 0x2E, 0x53, true),
    k("Home", 0x24, 0x47, true), k("End", 0x23, 0x4F, true),
    k("PageUp", 0x21, 0x49, true), k("PageDown", 0x22, 0x51, true),
];

const BUTTONS: &[(&str, u32)] = &[
    ("MouseLeft", mouse::LEFT),
    ("MouseRight", mouse::RIGHT),
    ("MouseMiddle", mouse::MIDDLE),
    ("MouseBack", mouse::BACK),
    ("MouseForward", mouse::FORWARD),
];

/// options.txt mouse codes, in [`BUTTONS`] order. Left -99, right -98 and middle -97 match
/// the defaults the public game writes to options.txt for key.attack, key.use and
/// key.pickItem. Back -96 and forward -95 continue the sequence; that is our assumption.
const MOUSE_CODES: [i32; 5] = [-99, -98, -97, -96, -95];

pub fn key(name: &str) -> Option<Key> {
    KEYS.iter()
        .find(|(known, _)| *known == name)
        .map(|(_, key)| *key)
}

fn button(name: &str) -> Option<u32> {
    BUTTONS
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, bit)| *bit)
}

/// The main key for a virtual key code: non-extended, left-hand first.
fn key_for_vk(vk: u8) -> Option<(&'static str, Key)> {
    let mut matching = KEYS.iter().filter(|(_, key)| key.vk == vk);
    let first = matching.clone().next().copied();
    matching.find(|(_, key)| !key.extended).copied().or(first)
}

/// Resolve a W3C key code, mouse button, or `key.*` binding to the control it presses.
pub fn resolve(name: &str, bindings: &Bindings) -> Result<Control> {
    if let Some(key) = key(name) {
        return Ok(Control {
            name: name.to_owned(),
            kind: Kind::Key(key),
        });
    }
    if let Some(bit) = button(name) {
        return Ok(Control {
            name: name.to_owned(),
            kind: Kind::Button(bit),
        });
    }
    if name.starts_with("key.") {
        let Some(&code) = bindings.get(name).and_then(|codes| codes.first()) else {
            bail!("{name} has no keyboard or mouse binding in options.txt.");
        };
        if code < 0 {
            let Some(index) = MOUSE_CODES.iter().position(|&known| known == code) else {
                bail!("{name} is bound to unknown mouse code {code}.");
            };
            let (button, bit) = BUTTONS[index];
            return Ok(Control {
                name: button.to_owned(),
                kind: Kind::Button(bit),
            });
        }
        let Ok(vk) = u8::try_from(code) else {
            bail!("{name} is bound to key code {code}, which is not a Windows virtual key.");
        };
        return Ok(match key_for_vk(vk) {
            Some((known, key)) => Control {
                name: known.to_owned(),
                kind: Kind::Key(key),
            },
            None => Control {
                name: format!("VK{vk}"),
                kind: Kind::Key(Key {
                    vk,
                    scan: 0,
                    extended: false,
                }),
            },
        });
    }
    bail!(
        "Unknown control {name:?}; use a binding such as key.jump, a W3C key code such as \
         KeyW, Digit1 or Escape, or MouseLeft/MouseRight/MouseMiddle/MouseBack/MouseForward."
    )
}

/// `keyboard_type_0_key.<action>:<code>[,<code>]` lines of options.txt, as `key.<action>`.
pub fn bindings(options: &BTreeMap<String, String>) -> Bindings {
    options
        .iter()
        .filter_map(|(key, value)| {
            let action = key.strip_prefix("keyboard_type_0_")?;
            action.starts_with("key.").then_some(())?;
            let codes = value
                .split(',')
                .filter_map(|code| code.trim().parse::<i32>().ok())
                .filter(|&code| code != 0)
                .collect();
            Some((action.to_owned(), codes))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(lines: &[(&str, &str)]) -> BTreeMap<String, String> {
        lines
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn bindings_keep_only_keyboard_codes_by_action() {
        let found = bindings(&options(&[
            ("keyboard_type_0_key.jump", "32"),
            ("keyboard_type_0_key.chat", "84,13"),
            ("keyboard_type_0_key.attack", "-99"),
            ("keyboard_type_0_key.unbound", "0"),
            ("keyboard_type_1_key.jump", "7"),
            ("gfx_fov", "70"),
        ]));
        assert_eq!(found["key.jump"], [32]);
        assert_eq!(found["key.chat"], [84, 13]);
        assert_eq!(found["key.attack"], [-99]);
        assert!(found["key.unbound"].is_empty());
        assert_eq!(found.len(), 4);
    }

    #[test]
    fn controls_resolve_codes_buttons_and_bindings() {
        let bindings = bindings(&options(&[
            ("keyboard_type_0_key.jump", "32"),
            ("keyboard_type_0_key.chat", "84,13"),
            ("keyboard_type_0_key.attack", "-99"),
            ("keyboard_type_0_key.use", "-98"),
            ("keyboard_type_0_key.sneak", "16"),
            ("keyboard_type_0_key.odd", "255"),
            ("keyboard_type_0_key.weird", "-5"),
        ]));
        let w = resolve("KeyW", &bindings).unwrap();
        assert_eq!(
            w.kind,
            Kind::Key(Key {
                vk: 0x57,
                scan: 0x11,
                extended: false
            })
        );
        assert_eq!(
            resolve("MouseBack", &bindings).unwrap().kind,
            Kind::Button(mouse::BACK)
        );
        assert_eq!(resolve("key.jump", &bindings).unwrap().name, "Space");
        assert_eq!(resolve("key.chat", &bindings).unwrap().name, "KeyT");
        assert_eq!(
            resolve("key.attack", &bindings).unwrap().kind,
            Kind::Button(mouse::LEFT)
        );
        assert_eq!(resolve("key.use", &bindings).unwrap().name, "MouseRight");
        assert_eq!(resolve("key.sneak", &bindings).unwrap().name, "ShiftLeft");
        let raw = resolve("key.odd", &bindings).unwrap();
        assert_eq!(raw.name, "VK255");
        assert_eq!(
            raw.kind,
            Kind::Key(Key {
                vk: 255,
                scan: 0,
                extended: false
            })
        );
        assert!(resolve("key.weird", &bindings).is_err());
        assert!(resolve("key.missing", &bindings).is_err());
        assert!(resolve("Banana", &bindings).is_err());
    }

    #[test]
    fn extended_keys_keep_their_flag() {
        let right_ctrl = key("ControlRight").unwrap();
        assert!(right_ctrl.extended);
        assert_eq!(key_for_vk(0x0D).unwrap().0, "Enter");
        assert_eq!(key_for_vk(0x26).unwrap().0, "ArrowUp");
    }
}
