//! The game window and the key and text messages posted to it.

use std::{
    sync::{
        Mutex,
        atomic::{AtomicPtr, Ordering},
    },
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, RECT},
    System::Threading::GetCurrentProcessId,
    UI::WindowsAndMessaging::{
        EnumWindows, GW_OWNER, GetClientRect, GetForegroundWindow, GetWindow,
        GetWindowThreadProcessId, IsWindowVisible, PostMessageW, WM_CHAR, WM_KEYDOWN, WM_KEYUP,
        WM_MOUSEMOVE,
    },
};

static WINDOW: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
/// lParam of each key the client holds, by virtual key; 0 when up.
static HELD: Mutex<[isize; 256]> = Mutex::new([0; 256]);
const WINDOW_TIMEOUT: Duration = Duration::from_secs(30);

/// The game window found by [`find_game_window`].
pub fn window() -> HWND {
    WINDOW.load(Ordering::Acquire)
}

struct Search {
    best: HWND,
    area: i64,
}

unsafe extern "system" fn visit(hwnd: HWND, search: LPARAM) -> i32 {
    // SAFETY: `find_game_window` passes a live `Search` for the duration of EnumWindows.
    let search = unsafe { &mut *(search as *mut Search) };
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if pid != unsafe { GetCurrentProcessId() }
        || unsafe { IsWindowVisible(hwnd) } == 0
        || !unsafe { GetWindow(hwnd, GW_OWNER) }.is_null()
    {
        return 1;
    }
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    unsafe { GetClientRect(hwnd, &mut rect) };
    let area = i64::from(rect.right - rect.left) * i64::from(rect.bottom - rect.top);
    if area > search.area {
        search.area = area;
        search.best = hwnd;
    }
    1
}

/// Remember the process's largest visible top-level window, waiting up to 30 s.
pub fn find_game_window() -> bool {
    let start = Instant::now();
    while start.elapsed() < WINDOW_TIMEOUT {
        let mut search = Search {
            best: std::ptr::null_mut(),
            area: 0,
        };
        unsafe { EnumWindows(Some(visit), &mut search as *mut Search as LPARAM) };
        if !search.best.is_null() {
            WINDOW.store(search.best, Ordering::Release);
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Client size in physical pixels and whether the window has focus.
pub fn window_state() -> (i32, i32, bool) {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    unsafe { GetClientRect(window(), &mut rect) };
    (
        rect.right,
        rect.bottom,
        unsafe { GetForegroundWindow() } == window(),
    )
}

fn key_lparam(scan: u8, extended: bool) -> isize {
    1 | (isize::from(scan) << 16) | (isize::from(extended) << 24)
}

/// Post a message tagged as the agent's, so the input lock lets it through.
fn post(message: u32, wparam: usize, lparam: isize) -> bool {
    unsafe { PostMessageW(window(), message, wparam, crate::lock::tag(lparam)) != 0 }
}

/// The screen position of client position `x`, `y` in the game window.
pub fn client_to_screen(x: i64, y: i64) -> (i64, i64) {
    let mut point = windows_sys::Win32::Foundation::POINT {
        x: x as i32,
        y: y as i32,
    };
    // SAFETY: the window comes from EnumWindows; the point outlives the call.
    if unsafe { windows_sys::Win32::Graphics::Gdi::ClientToScreen(window(), &mut point) } == 0 {
        return (x, y);
    }
    (i64::from(point.x), i64::from(point.y))
}

pub fn key_down(vk: u8, scan: u8, extended: bool) -> bool {
    let mut held = HELD.lock().unwrap_or_else(|poison| poison.into_inner());
    let mut lparam = key_lparam(scan, extended);
    if held[usize::from(vk)] != 0 {
        lparam |= 1 << 30; // auto-repeat
    }
    held[usize::from(vk)] = key_lparam(scan, extended);
    post(WM_KEYDOWN, usize::from(vk), lparam)
}

pub fn key_up(vk: u8) -> bool {
    let mut held = HELD.lock().unwrap_or_else(|poison| poison.into_inner());
    let lparam = std::mem::take(&mut held[usize::from(vk)]);
    lparam == 0 || post(WM_KEYUP, usize::from(vk), lparam | (0b11 << 30))
}

/// Post one Unicode scalar value, as a surrogate pair when needed.
pub fn text(code: u32) -> bool {
    let Some(character) = char::from_u32(code) else {
        return false;
    };
    let mut units = [0u16; 2];
    character
        .encode_utf16(&mut units)
        .iter()
        .all(|&unit| post(WM_CHAR, usize::from(unit), 1))
}

/// Tell the window the mouse is in use, at client position `x`, `y`.
///
/// The agent assumes the game switches to mouse input (pointer, hover,
/// wheel) only after it sees window mouse messages, and that GameInput
/// readings alone leave it in keyboard navigation.
/// Posting the message does not move the system cursor.
pub fn mouse_activity(x: i64, y: i64) -> bool {
    let lparam = (y.clamp(0, 0x7fff) << 16 | x.clamp(0, 0x7fff)) as isize;
    post(WM_MOUSEMOVE, 0, lparam)
}

pub fn release_all() {
    for vk in 0..=255u8 {
        key_up(vk);
    }
}
