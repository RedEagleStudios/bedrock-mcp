//! Keeping the game active while a client drives it.
//!
//! The agent assumes Minecraft stops applying mouse look and attacks once its window is told
//! it lost activation (`WM_ACTIVATE`, `WM_ACTIVATEAPP`, `WM_KILLFOCUS`), as on alt-tab. While
//! a client is connected the agent withholds those notifications and answers the game's own
//! `GetForegroundWindow` and `GetFocus` with its window, so input keeps working while the
//! player uses other windows. The agent never moves the real foreground or focus. The game's
//! `ClipCursor` requests are dropped while its window is not really in front, so the player's
//! cursor is never confined to a window they left.
//!
//! While the client overrides the pointer, the game's own cursor queries answer with that
//! pointer: `GetCursorPos` returns it, and the real cursor's `WM_MOUSEMOVE` and
//! `WM_MOUSELEAVE` are dropped. The agent assumes menus read the pointer through these window
//! paths, where the real cursor (often outside the window) would cancel the hover and the
//! click would land nowhere. The window procedure also applies the input [`lock`].

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use endpoint::log;
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::{
        CallWindowProcW, ClipCursor, GWLP_WNDPROC, GetForegroundWindow, GetWindowLongPtrW,
        PostMessageW, SetWindowLongPtrW, WA_INACTIVE, WM_ACTIVATE, WM_ACTIVATEAPP, WM_KILLFOCUS,
        WM_MOUSEMOVE, WM_NCACTIVATE, WM_SETFOCUS, WNDPROC,
    },
};

use crate::{
    gameinput, iat, keyboard,
    lock::{self, WM_MOUSELEAVE},
};

/// Set while a client is connected.
static HELD: AtomicBool = AtomicBool::new(false);
/// Whether the game was last told it is active.
static ACTIVE: AtomicBool = AtomicBool::new(true);
/// A deactivation was withheld and the game has not been reactivated since.
static WITHHELD: AtomicBool = AtomicBool::new(false);
static ORIGINAL_PROC: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_FOREGROUND: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_FOCUS: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_CLIP: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_CURSOR_POS: AtomicUsize = AtomicUsize::new(0);

type WindowQuery = unsafe extern "system" fn() -> HWND;
type Clip = unsafe extern "system" fn(*const RECT) -> i32;
type CursorPos = unsafe extern "system" fn(*mut POINT) -> i32;

/// The client pointer while the client overrides it.
fn overridden_pointer() -> Option<(i64, i64)> {
    let synthetic = &gameinput::SYNTHETIC;
    synthetic.pointer_active.load(Ordering::Acquire).then(|| {
        (
            synthetic.pointer_x.load(Ordering::Relaxed),
            synthetic.pointer_y.load(Ordering::Relaxed),
        )
    })
}

fn really_in_front() -> bool {
    (unsafe { GetForegroundWindow() }) == keyboard::window()
}

/// Whether the game currently believes it is the active window.
pub fn game_active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

/// Whether a client is connected and the game is kept active.
pub fn held() -> bool {
    HELD.load(Ordering::Acquire)
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let (deactivation, activation) = match message {
        WM_ACTIVATE if (wparam & 0xFFFF) as u32 == WA_INACTIVE => (true, false),
        WM_ACTIVATE | WM_SETFOCUS => (false, true),
        WM_ACTIVATEAPP => (wparam == 0, wparam != 0),
        WM_KILLFOCUS => (true, false),
        _ => (false, false),
    };
    // The agent's own messages carry a tag; the game gets them without it.
    let (lparam, ours) = match lock::untag(lparam) {
        Some(lparam) => (lparam, true),
        None => (lparam, false),
    };
    if !ours {
        if overridden_pointer().is_some() && matches!(message, WM_MOUSEMOVE | WM_MOUSELEAVE) {
            return 0;
        }
        if let Some(answer) = lock::filter(hwnd, message, wparam, lparam) {
            return answer;
        }
    }
    if deactivation && HELD.load(Ordering::Acquire) {
        WITHHELD.store(true, Ordering::Release);
        // The game may hold a cursor clip from while it was in front.
        unsafe { ClipCursor(std::ptr::null()) };
        return 0;
    }
    if deactivation {
        ACTIVE.store(false, Ordering::Release);
    } else if activation {
        ACTIVE.store(true, Ordering::Release);
        WITHHELD.store(false, Ordering::Release);
    }
    // SAFETY: the window procedure this one replaced.
    let original: WNDPROC = unsafe { std::mem::transmute(ORIGINAL_PROC.load(Ordering::Acquire)) };
    unsafe { CallWindowProcW(original, hwnd, message, wparam, lparam) }
}

unsafe extern "system" fn foreground_hook() -> HWND {
    if HELD.load(Ordering::Acquire) {
        return keyboard::window();
    }
    // SAFETY: the import this hook replaced.
    let original: WindowQuery =
        unsafe { std::mem::transmute(ORIGINAL_FOREGROUND.load(Ordering::Acquire)) };
    unsafe { original() }
}

unsafe extern "system" fn focus_hook() -> HWND {
    // SAFETY: the import this hook replaced.
    let original: WindowQuery =
        unsafe { std::mem::transmute(ORIGINAL_FOCUS.load(Ordering::Acquire)) };
    let focus = unsafe { original() };
    if focus.is_null() && HELD.load(Ordering::Acquire) {
        return keyboard::window();
    }
    focus
}

unsafe extern "system" fn clip_hook(rect: *const RECT) -> i32 {
    if !rect.is_null() && HELD.load(Ordering::Acquire) && !really_in_front() {
        return 1;
    }
    // SAFETY: the import this hook replaced.
    let original: Clip = unsafe { std::mem::transmute(ORIGINAL_CLIP.load(Ordering::Acquire)) };
    unsafe { original(rect) }
}

unsafe extern "system" fn cursor_pos_hook(point: *mut POINT) -> i32 {
    if let Some((x, y)) = overridden_pointer()
        && !point.is_null()
    {
        let (x, y) = keyboard::client_to_screen(x, y);
        // SAFETY: the caller passed a writable POINT.
        unsafe {
            *point = POINT {
                x: x as i32,
                y: y as i32,
            }
        };
        return 1;
    }
    // SAFETY: the import this hook replaced.
    let original: CursorPos =
        unsafe { std::mem::transmute(ORIGINAL_CURSOR_POS.load(Ordering::Acquire)) };
    unsafe { original(point) }
}

/// Redirect the game executable's import of `function` to `hook`.
fn hook_import(function: &str, hook: usize, original: &AtomicUsize) -> bool {
    let module = unsafe { GetModuleHandleW(std::ptr::null()) } as *const u8;
    // SAFETY: the executable is a mapped PE32+ image.
    let Some(slot) = (unsafe { iat::slot(module, "user32.dll", function) }) else {
        log!("the game does not import {function}");
        return false;
    };
    // SAFETY: an import-address-table slot holds one function pointer.
    original.store(unsafe { *slot }, Ordering::Release);
    unsafe { gameinput::patch(slot, hook) }.is_some()
}

/// Subclass the game window and hook its focus queries; the hooks stay inert until a client
/// connects.
pub fn install() -> bool {
    let window = keyboard::window();
    ACTIVE.store(really_in_front(), Ordering::Release);
    let current = unsafe { GetWindowLongPtrW(window, GWLP_WNDPROC) };
    if current == 0 {
        log!("cannot read the game's window procedure");
        return false;
    }
    ORIGINAL_PROC.store(current as usize, Ordering::Release);
    type Procedure = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;
    let procedure = window_proc as Procedure as usize as isize;
    let replaced = unsafe { SetWindowLongPtrW(window, GWLP_WNDPROC, procedure) };
    if replaced == 0 {
        log!("cannot subclass the game window");
        return false;
    }
    ORIGINAL_PROC.store(replaced as usize, Ordering::Release);
    let foreground = hook_import(
        "GetForegroundWindow",
        foreground_hook as WindowQuery as usize,
        &ORIGINAL_FOREGROUND,
    );
    let focus = hook_import(
        "GetFocus",
        focus_hook as WindowQuery as usize,
        &ORIGINAL_FOCUS,
    );
    let clip = hook_import("ClipCursor", clip_hook as Clip as usize, &ORIGINAL_CLIP);
    let cursor = hook_import(
        "GetCursorPos",
        cursor_pos_hook as CursorPos as usize,
        &ORIGINAL_CURSOR_POS,
    );
    log!(
        "focus hold installed (GetForegroundWindow {foreground}, GetFocus {focus}, ClipCursor {clip}, GetCursorPos {cursor})"
    );
    true
}

fn post(message: u32, wparam: usize) {
    unsafe { PostMessageW(keyboard::window(), message, wparam, 0) };
}

/// Keep the game active while a client is connected. An inactive game is told it is active
/// again without touching the real focus.
pub fn hold() {
    HELD.store(true, Ordering::Release);
    if !ACTIVE.load(Ordering::Acquire) {
        log!("reactivating the game for the client");
        post(WM_ACTIVATEAPP, 1);
        post(WM_NCACTIVATE, 1);
        post(WM_SETFOCUS, 0);
    }
}

/// Stop holding focus; deliver a withheld deactivation if the window is still not in front.
pub fn release() {
    HELD.store(false, Ordering::Release);
    if WITHHELD.swap(false, Ordering::AcqRel) && !really_in_front() {
        log!("delivering the withheld deactivation");
        post(WM_NCACTIVATE, 0);
        post(WM_ACTIVATE, WA_INACTIVE as usize);
        post(WM_ACTIVATEAPP, 0);
        post(WM_KILLFOCUS, 0);
    }
}
