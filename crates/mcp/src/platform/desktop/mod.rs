//! The Linux desktop backend, chosen once per process: Hyprland when the session runs inside
//! Hyprland and `hyprctl` answers, otherwise the generic backend that needs no compositor. A
//! headless game runs in gamescope, out of the desktop's reach, so it always gets the generic
//! backend, which takes the window from the agent.

mod generic;
mod hyprland;

use std::{ffi::OsString, sync::LazyLock};

use generic::Generic;
use hyprland::Hyprland;

use super::Desktop;

/// The variable Hyprland sets in every process it starts.
const HYPRLAND_SIGNATURE: &str = "HYPRLAND_INSTANCE_SIGNATURE";

/// The desktop backend for this session.
pub fn current() -> &'static dyn Desktop {
    static COMPOSITOR: LazyLock<&'static dyn Desktop> =
        LazyLock::new(|| select(std::env::var_os(HYPRLAND_SIGNATURE), hyprland::answers));
    if super::linux::headless_game().is_some() {
        &Generic
    } else {
        *COMPOSITOR
    }
}

/// The generic backend, for tests of callers on a desktop without window control.
#[cfg(test)]
pub fn generic() -> &'static dyn Desktop {
    &Generic
}

/// Hyprland when `signature` is set and non-empty and `answers` confirms hyprctl reaches it;
/// otherwise the generic backend.
fn select(signature: Option<OsString>, answers: impl FnOnce() -> bool) -> &'static dyn Desktop {
    if signature.is_some_and(|value| !value.is_empty()) && answers() {
        &Hyprland
    } else {
        &Generic
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn hyprland_is_chosen_only_when_its_session_answers() {
        let name = |signature: Option<&str>, answers: bool| {
            select(signature.map(OsString::from), || answers).name()
        };
        assert_eq!(name(Some("abc_123"), true), "hyprland");
        assert_eq!(name(Some("abc_123"), false), "generic");
        assert_eq!(name(Some(""), true), "generic");
        assert_eq!(name(None, true), "generic");
    }

    #[test]
    fn hyprctl_is_not_asked_without_a_hyprland_session() {
        let asked = std::cell::Cell::new(false);
        select(None, || {
            asked.set(true);
            true
        });
        assert!(!asked.get());
    }

    #[test]
    fn the_generic_backend_leaves_windows_to_the_agent() {
        let generic = select(None, || false);
        assert!(generic.window(1).unwrap().is_none());
        assert!(
            generic
                .screenshot(
                    &crate::platform::Window::from_agent(&test_status(), "generic"),
                    Path::new("/x.png"),
                    None
                )
                .is_none()
        );
        assert!(generic.keep_rendering(1).is_ok());
        assert_eq!(generic.raise_hidden_fps(240), Ok(None));
        let error = generic.resize(1, 800, 600).unwrap_err().to_string();
        assert!(error.contains("not supported on this desktop"), "{error}");
    }

    fn test_status() -> protocol::Status {
        protocol::Status {
            protocol: protocol::PROTOCOL_VERSION,
            pid: 1,
            width: 1280,
            height: 720,
            foreground: false,
            game_active: true,
            mouse_readings: 0,
            gameinput_versions: Vec::new(),
            pointer: [0, 0],
            pointer_override: false,
            input_locked: false,
            takeovers: 0,
        }
    }
}
