//! Screen stacks: the names of the screens on every `SceneStack`.
//!
//! `SceneStack` implements `ISceneStack` (70 slots). Its `update()` (slot 34) runs on the
//! game thread, so the hook does all reads there and never touches a stack from another
//! thread. Each stack updates at least once per frame; on each update the hook compares
//! `getSize()` (38) and `getTopScene()` (49), which allocate nothing, and only when either
//! changes does it call `getScreenNames()` (42). The game's allocator owns that vector and
//! its strings, and the agent has no safe way to free them, so each screen change leaks one
//! small vector (a read the code rejects is retried on the next update, leaking one more).
//! Slot reference: LeviLamina's `src-client/mc/client/gui/screens/SceneStack.h` and
//! `interfaces/ISceneStack.h`.

use std::{
    ffi::c_void,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use endpoint::log;
use protocol::unstable::{CapabilityState, SceneStack};

use crate::{camera, capability, hud, ui_tree, vtable::Resolver};

const NAME: &str = "screens";
const CLASS: &str = "SceneStack";
const SLOTS: usize = 70;
const UPDATE: usize = 34;
const GET_SCREEN_NAMES: usize = 42;
const GET_SIZE: usize = 38;
const GET_TOP_SCENE: usize = 49;
const MAX_STACKS: usize = 8;
const MAX_SCREENS: usize = 64;
const MAX_NAME: usize = 256;
/// Bad reads in a row before the capability fails: at most half a second at 60 frames a
/// second, since each stack updates at least once per frame.
const MAX_BAD_READS: u32 = 30;

/// MSVC `std::string`: 16 bytes of inline buffer or pointer, size, capacity.
#[repr(C)]
struct RawString {
    buffer: [u8; 16],
    size: usize,
    capacity: usize,
}

/// MSVC `std::vector<T>`: first, last, end of storage.
#[repr(C)]
struct RawVector {
    first: *const RawString,
    last: *const RawString,
    end: *const RawString,
}

type Update = unsafe extern "system" fn(*mut c_void) -> bool;
type GetSize = unsafe extern "system" fn(*mut c_void) -> u64;
type GetTopScene = unsafe extern "system" fn(*mut c_void) -> *mut c_void;
type GetScreenNames = unsafe extern "system" fn(*mut c_void, *mut RawVector) -> *mut RawVector;

static ORIGINAL_UPDATE: AtomicUsize = AtomicUsize::new(0);
static GET_SIZE_FN: AtomicUsize = AtomicUsize::new(0);
static GET_TOP_SCENE_FN: AtomicUsize = AtomicUsize::new(0);
static GET_SCREEN_NAMES_FN: AtomicUsize = AtomicUsize::new(0);

struct Stack {
    id: usize,
    size: u64,
    top: usize,
    screens: Vec<String>,
    frames_since_change: u64,
}

static STACKS: Mutex<Vec<Stack>> = Mutex::new(Vec::new());
/// Consecutive bad reads of each stack whose last change is not read yet.
static BAD_READS: Mutex<Vec<(usize, u32)>> = Mutex::new(Vec::new());

/// A screen name: printable UTF-8 without control characters (names can hold spaces).
///
/// # Safety
/// `string` must be a live MSVC `std::string`.
unsafe fn read_string(string: &RawString) -> Result<String, String> {
    if string.size > MAX_NAME || string.capacity < string.size {
        return Err(format!(
            "a name of {} bytes with capacity {}",
            string.size, string.capacity
        ));
    }
    let data = if string.capacity >= 16 {
        // SAFETY: a heap string stores its pointer in the buffer.
        unsafe { *(string.buffer.as_ptr() as *const *const u8) }
    } else {
        string.buffer.as_ptr()
    };
    // SAFETY: `size` bytes are valid at `data` for a live string.
    let bytes = unsafe { std::slice::from_raw_parts(data, string.size) };
    match std::str::from_utf8(bytes) {
        Ok(text) if !text.chars().any(char::is_control) => Ok(text.to_owned()),
        _ => Err(format!(
            "an unprintable name {:?}",
            String::from_utf8_lossy(&bytes[..bytes.len().min(48)])
        )),
    }
}

/// # Safety
/// Must run on the game thread, inside `SceneStack::update` for `this`.
unsafe fn screen_names(this: *mut c_void) -> Result<Vec<String>, String> {
    // SAFETY: stored from slot 42 of the checked SceneStack vtable.
    let get: GetScreenNames =
        unsafe { std::mem::transmute(GET_SCREEN_NAMES_FN.load(Ordering::Acquire)) };
    let mut vector = RawVector {
        first: std::ptr::null(),
        last: std::ptr::null(),
        end: std::ptr::null(),
    };
    unsafe { get(this, &mut vector) };
    if vector.first.is_null() {
        return Ok(Vec::new());
    }
    let count = (vector.last as usize - vector.first as usize) / size_of::<RawString>();
    if count > MAX_SCREENS {
        return Err(format!("{count} names"));
    }
    // SAFETY: the returned vector holds `count` live strings.
    let strings = unsafe { std::slice::from_raw_parts(vector.first, count) };
    strings
        .iter()
        .map(|string| unsafe { read_string(string) })
        .collect()
}

unsafe extern "system" fn update_hook(this: *mut c_void) -> bool {
    // SAFETY: stored from the slot this hook replaced.
    let original: Update = unsafe { std::mem::transmute(ORIGINAL_UPDATE.load(Ordering::Acquire)) };
    let result = unsafe { original(this) };
    let get_size: GetSize = unsafe { std::mem::transmute(GET_SIZE_FN.load(Ordering::Acquire)) };
    let get_top: GetTopScene =
        unsafe { std::mem::transmute(GET_TOP_SCENE_FN.load(Ordering::Acquire)) };
    let size = unsafe { get_size(this) };
    let top = unsafe { get_top(this) } as usize;
    let changed = unsafe { track(this, size, top) };
    ui_tree::frame(this as usize, top, changed, || top_name(this as usize));
    hud::frame();
    camera::frame();
    crate::perf::scene_frame(this as usize);
    result
}

/// Record `this` stack's size and top scene; true when either changed and the new names
/// were read.
///
/// If the names disagree with `getSize`, the code keeps the last good read and retries on
/// the next update; only [`MAX_BAD_READS`] bad reads in a row fail the capability.
///
/// # Safety
/// Must run on the game thread, inside `SceneStack::update` for `this`.
unsafe fn track(this: *mut c_void, size: u64, top: usize) -> bool {
    let Ok(mut stacks) = STACKS.lock() else {
        return false;
    };
    let id = this as usize;
    let known = stacks.iter().position(|stack| stack.id == id);
    let changed = known.is_none_or(|index| stacks[index].size != size || stacks[index].top != top);
    if let Some(index) = known {
        stacks[index].frames_since_change += 1;
    }
    if !changed {
        return false;
    }
    let screens = if size as usize > MAX_SCREENS {
        Err(format!("implausible stack size {size}"))
    } else {
        match unsafe { screen_names(this) } {
            Err(reason) => Err(format!("getScreenNames returned {reason}")),
            Ok(screens) if screens.len() as u64 != size => {
                Err(format!("{} names for a stack of {size}", screens.len()))
            }
            Ok(screens) => Ok(screens),
        }
    };
    let mut bad = BAD_READS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let screens = match screens {
        Ok(screens) => {
            bad.retain(|(stack, _)| *stack != id);
            screens
        }
        Err(reason) => {
            let count = match bad.iter_mut().find(|(stack, _)| *stack == id) {
                Some((_, count)) => {
                    *count += 1;
                    *count
                }
                None => {
                    bad.push((id, 1));
                    1
                }
            };
            if count == 1 {
                log!("screens: stack {id:#x} read mid-change ({reason}); retrying");
            }
            if count == MAX_BAD_READS {
                capability::set(
                    NAME,
                    CapabilityState::Failed,
                    format!("{reason} for {count} frames in a row"),
                );
            }
            return false;
        }
    };
    drop(bad);
    let stack = Stack {
        id,
        size,
        top,
        screens,
        frames_since_change: 0,
    };
    match known {
        Some(index) => stacks[index] = stack,
        None if stacks.len() < MAX_STACKS => stacks.push(stack),
        None => return true,
    }
    capability::set(
        NAME,
        CapabilityState::Active,
        "SceneStack::update confirmed with readable screen names",
    );
    true
}

/// The name of the screen on top of stack `id`, as last recorded.
fn top_name(id: usize) -> String {
    STACKS
        .lock()
        .ok()
        .and_then(|stacks| {
            stacks
                .iter()
                .find(|stack| stack.id == id)
                .and_then(|stack| stack.screens.last().cloned())
        })
        .unwrap_or_default()
}

/// Hook `SceneStack::update`; the result says whether it is in place.
pub fn install(resolver: &Resolver<'_>) -> Result<(), String> {
    let result = resolver.table(CLASS, 0, SLOTS).and_then(|table| {
        capability::found(NAME, &[(CLASS, table.found)]);
        GET_SIZE_FN.store(table.get(GET_SIZE), Ordering::Release);
        GET_TOP_SCENE_FN.store(table.get(GET_TOP_SCENE), Ordering::Release);
        GET_SCREEN_NAMES_FN.store(table.get(GET_SCREEN_NAMES), Ordering::Release);
        table.hook(UPDATE, update_hook as Update as usize, &ORIGINAL_UPDATE)
    });
    match &result {
        Ok(()) => capability::set(
            NAME,
            CapabilityState::Pending,
            "hooked; waiting for the first frame",
        ),
        Err(reason) => capability::set(NAME, CapabilityState::Unavailable, reason.clone()),
    }
    result
}

/// Every stack seen so far, bottom-first screen names.
pub fn read() -> Vec<SceneStack> {
    STACKS
        .lock()
        .map(|stacks| {
            stacks
                .iter()
                .map(|stack| SceneStack {
                    id: stack.id as u64,
                    screens: stack.screens.clone(),
                    frames_since_change: stack.frames_since_change,
                })
                .collect()
        })
        .unwrap_or_default()
}
