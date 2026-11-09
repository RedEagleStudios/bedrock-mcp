//! Locking the player's real input out of the game while a client plays.
//!
//! While locked, the window procedure drops the real keyboard messages (raw keyboard input
//! too) and the real mouse messages that reach the game window, and every GameInput mouse
//! reading carries only the client's motion, buttons and wheel. Window management messages
//! (activation, size, close) always pass. The agent tells its own posted messages apart by a
//! tag in the high half of `lParam`, which input messages from the system leave zero; the
//! window procedure strips the tag before the game sees the message, so the agent never
//! relies on timing. A real F9 press unlocks at once, is kept from the game, and counts a
//! take-over the client reads in the status. The agent unlocks when its client disconnects.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

use endpoint::log;
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    UI::{
        Input::{
            GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTHEADER, RID_INPUT, RIM_TYPEKEYBOARD,
        },
        WindowsAndMessaging::{
            DefWindowProcW, WM_INPUT, WM_KEYDOWN, WM_KEYFIRST, WM_KEYLAST, WM_MOUSEFIRST,
            WM_MOUSELAST, WM_SYSKEYDOWN,
        },
    },
};

use crate::{gameinput, keyboard};

/// `VK_F9`, kept here so the agent needs no keyboard feature of windows-sys.
const VK_F9: usize = 0x78;
/// `WM_MOUSEHOVER` and `WM_MOUSELEAVE`, which windows-sys keeps behind another feature.
const WM_MOUSEHOVER: u32 = 0x02A1;
pub const WM_MOUSELEAVE: u32 = 0x02A3;

/// "mcpg" in the high half of `lParam` marks a message the agent posted.
const TAG: isize = 0x6D63_7067 << 32;
const LOW_HALF: isize = 0xFFFF_FFFF;

static LOCKED: AtomicBool = AtomicBool::new(false);
static TAKEOVERS: AtomicU64 = AtomicU64::new(0);

/// Mark `lparam` (a 32-bit message parameter) as posted by the agent.
pub fn tag(lparam: LPARAM) -> LPARAM {
    (lparam & LOW_HALF) | TAG
}

/// The original `lParam` of a message the agent posted; None for any other message.
pub fn untag(lparam: LPARAM) -> Option<LPARAM> {
    (lparam & !LOW_HALF == TAG).then_some(lparam & LOW_HALF)
}

pub fn locked() -> bool {
    LOCKED.load(Ordering::Acquire)
}

pub fn takeovers() -> u64 {
    TAKEOVERS.load(Ordering::Acquire)
}

pub fn set(locked: bool) {
    if LOCKED.swap(locked, Ordering::AcqRel) != locked {
        log!("input {}", if locked { "locked" } else { "unlocked" });
    }
}

/// The player pressed F9: unlock, count it, and let go of what the client holds so the game
/// does not keep walking or attacking under the player.
fn take_over() {
    if LOCKED.swap(false, Ordering::AcqRel) {
        TAKEOVERS.fetch_add(1, Ordering::AcqRel);
        keyboard::release_all();
        gameinput::release();
        log!("the player took over with F9");
    }
}

/// What the locked window procedure does with a real (untagged) message.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Pass,
    Drop,
    TakeOver,
    /// `WM_INPUT`: only raw keyboard input is dropped, so the device must be read first.
    RawInput,
}

fn classify(message: u32, wparam: WPARAM) -> Verdict {
    match message {
        WM_KEYDOWN | WM_SYSKEYDOWN if wparam == VK_F9 => Verdict::TakeOver,
        WM_KEYFIRST..=WM_KEYLAST | WM_MOUSEFIRST..=WM_MOUSELAST | WM_MOUSEHOVER | WM_MOUSELEAVE => {
            Verdict::Drop
        }
        WM_INPUT => Verdict::RawInput,
        _ => Verdict::Pass,
    }
}

/// For raw input from a keyboard, whether it is an F9 press; None for other devices or when
/// the input cannot be read.
fn raw_keyboard(handle: LPARAM) -> Option<bool> {
    // SAFETY: an all-zero RAWINPUT is valid plain data.
    let mut raw: RAWINPUT = unsafe { std::mem::zeroed() };
    let mut size = size_of::<RAWINPUT>() as u32;
    // SAFETY: `raw` is writable for `size` bytes; a larger (HID) input fails instead of
    // writing past it.
    let read = unsafe {
        GetRawInputData(
            handle as HRAWINPUT,
            RID_INPUT,
            (&raw mut raw).cast(),
            &mut size,
            size_of::<RAWINPUTHEADER>() as u32,
        )
    };
    if read == u32::MAX || raw.header.dwType != RIM_TYPEKEYBOARD {
        return None;
    }
    // SAFETY: the header says the union holds keyboard input.
    let keyboard = unsafe { raw.data.keyboard };
    Some(
        usize::from(keyboard.VKey) == VK_F9
            && matches!(keyboard.Message, WM_KEYDOWN | WM_SYSKEYDOWN),
    )
}

/// The answer to a real message while locked, or None to pass it to the game.
pub fn filter(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    if !locked() {
        return None;
    }
    match classify(message, wparam) {
        Verdict::Pass => None,
        Verdict::Drop => Some(0),
        Verdict::TakeOver => {
            take_over();
            Some(0)
        }
        Verdict::RawInput => {
            if raw_keyboard(lparam)? {
                take_over();
            }
            // Raw input the game never sees still needs DefWindowProc to free it.
            Some(unsafe { DefWindowProcW(hwnd, message, wparam, lparam) })
        }
    }
}

/// Real mouse motion and wheel kept from the game while locked. GameInput's counters are
/// cumulative and the game turns them into deltas, so per counter the real part the game sees
/// stops while locked and carries on from there after, without a jump.
pub struct Hidden {
    hidden: [AtomicI64; 4],
    shown: [AtomicI64; 4],
}

/// Counter indexes for [`Hidden::shown`].
pub const MOTION_X: usize = 0;
pub const MOTION_Y: usize = 1;
pub const WHEEL_X: usize = 2;
pub const WHEEL_Y: usize = 3;

impl Hidden {
    const fn new() -> Self {
        Self {
            hidden: [const { AtomicI64::new(0) }; 4],
            shown: [const { AtomicI64::new(0) }; 4],
        }
    }

    /// The part of real counter `index` (now `real`) the game may see.
    pub fn shown(&self, index: usize, real: i64, locked: bool) -> i64 {
        if locked {
            let shown = self.shown[index].load(Ordering::Relaxed);
            self.hidden[index].store(real.wrapping_sub(shown), Ordering::Relaxed);
            shown
        } else {
            let shown = real.wrapping_sub(self.hidden[index].load(Ordering::Relaxed));
            self.shown[index].store(shown, Ordering::Relaxed);
            shown
        }
    }
}

pub static HIDDEN: Hidden = Hidden::new();

#[cfg(test)]
mod tests {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        WM_ACTIVATE, WM_CHAR, WM_CLOSE, WM_KEYUP, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_MOUSEWHEEL,
        WM_SIZE, WM_SYSKEYUP,
    };

    use super::*;

    #[test]
    fn tags_round_trip_and_real_parameters_are_not_tagged() {
        for lparam in [0, 1, 0x001E_0001, 0xC01E_0001, 0x7FFF_7FFF] {
            assert_eq!(untag(tag(lparam)), Some(lparam));
            assert_eq!(untag(lparam), None);
        }
        // Some systems sign-extend 32-bit key parameters.
        assert_eq!(untag(0xC01E_0001_u32 as i32 as isize), None);
    }

    #[test]
    fn real_input_is_dropped_and_window_management_passes() {
        for message in [
            WM_KEYDOWN,
            WM_KEYUP,
            WM_CHAR,
            WM_SYSKEYDOWN,
            WM_SYSKEYUP,
            WM_MOUSEMOVE,
            WM_LBUTTONDOWN,
            WM_MOUSEWHEEL,
            WM_MOUSELEAVE,
        ] {
            assert_eq!(classify(message, 0x41), Verdict::Drop, "{message:#x}");
        }
        for message in [WM_ACTIVATE, WM_SIZE, WM_CLOSE] {
            assert_eq!(classify(message, 0), Verdict::Pass, "{message:#x}");
        }
        assert_eq!(classify(WM_INPUT, 0), Verdict::RawInput);
    }

    #[test]
    fn f9_down_takes_over_and_its_release_is_dropped() {
        assert_eq!(classify(WM_KEYDOWN, VK_F9), Verdict::TakeOver);
        assert_eq!(classify(WM_SYSKEYDOWN, VK_F9), Verdict::TakeOver);
        assert_eq!(classify(WM_KEYUP, VK_F9), Verdict::Drop);
    }

    #[test]
    fn hidden_motion_stops_while_locked_and_resumes_without_a_jump() {
        let hidden = Hidden::new();
        assert_eq!(hidden.shown(MOTION_X, 10, false), 10);
        // The player moves 50 counts while locked: the game sees none of it.
        assert_eq!(hidden.shown(MOTION_X, 30, true), 10);
        assert_eq!(hidden.shown(MOTION_X, 60, true), 10);
        // After the lock the game sees later motion only.
        assert_eq!(hidden.shown(MOTION_X, 60, false), 10);
        assert_eq!(hidden.shown(MOTION_X, 65, false), 15);
        // Other counters are independent.
        assert_eq!(hidden.shown(WHEEL_Y, -120, false), -120);
    }
}
