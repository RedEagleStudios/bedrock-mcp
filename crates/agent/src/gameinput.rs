//! Synthetic mouse input merged into the game's GameInput readings.
//!
//! The agent handles the game's mouse reads through
//! `IGameInput::GetCurrentReading` and `IGameInputReading::GetMouseState`,
//! however often the game calls them. It assumes interface and reading
//! vtables are static tables inside GameInput, shared by every object, so
//! patching their slots also covers the game's own objects.
//! Slot numbers follow the v0/v1/v2 interfaces in Wine's
//! `include/gameinput.idl`; the layouts are fixed per interface version.

use std::{
    ffi::c_void,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
};

use windows_sys::{
    Win32::System::{
        LibraryLoader::{GetModuleHandleW, GetProcAddress},
        Memory::{PAGE_PROTECTION_FLAGS, PAGE_READWRITE, VirtualProtect},
    },
    core::{GUID, HRESULT},
};

use endpoint::{log, wide};

const IIDS: [GUID; 3] = [
    GUID::from_u128(0x11be2a7e_4254_445a_9c09_ffc40f006918),
    GUID::from_u128(0x40ffb7e4_6150_407a_b439_132badc08d2d),
    GUID::from_u128(0xbbaa66d2_837a_40f7_a303_917d500955f4),
];
const SLOT_CURRENT_READING: usize = 4;
const SLOT_NEXT_READING: usize = 5;
const SLOT_MOUSE_STATE: [usize; 3] = [16, 14, 14];
const MODULES: [&str; 2] = ["GameInputRedist.dll", "gameinput.dll"];
const MAX_READING_CLASSES: usize = 8;
/// `GameInputMouseAbsolutePosition`.
const ABSOLUTE_POSITION: u32 = 1;
/// `GameInputKindMouse`.
const KIND_MOUSE: u32 = 0x20;

#[repr(C)]
struct MouseStateV0 {
    buttons: u32,
    x: i64,
    y: i64,
    wheel_x: i64,
    wheel_y: i64,
}

/// v1 and v2.
#[repr(C)]
struct MouseStateV1 {
    buttons: u32,
    positions: u32,
    x: i64,
    y: i64,
    absolute_x: i64,
    absolute_y: i64,
    wheel_x: i64,
    wheel_y: i64,
}

type CurrentReading =
    unsafe extern "system" fn(*mut c_void, u32, *mut c_void, *mut *mut c_void) -> HRESULT;
type NextReading = unsafe extern "system" fn(
    *mut c_void,
    *mut c_void,
    u32,
    *mut c_void,
    *mut *mut c_void,
) -> HRESULT;
type MouseState = unsafe extern "system" fn(*mut c_void, *mut c_void) -> bool;
type QueryInterface =
    unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT;
type Release = unsafe extern "system" fn(*mut c_void) -> u32;
type AddRef = unsafe extern "system" fn(*mut c_void) -> u32;
type Create = unsafe extern "system" fn(*mut *mut c_void) -> HRESULT;

/// Synthetic input added to every mouse reading. Motion and wheel are
/// cumulative, like GameInput's own counters.
pub struct Synthetic {
    pub motion_x: AtomicI64,
    pub motion_y: AtomicI64,
    pub wheel_x: AtomicI64,
    pub wheel_y: AtomicI64,
    pub buttons: AtomicU32,
    pub pointer_active: AtomicBool,
    pub pointer_x: AtomicI64,
    pub pointer_y: AtomicI64,
    pub readings: AtomicU64,
    pub real_x: AtomicI64,
    pub real_y: AtomicI64,
}

pub static SYNTHETIC: Synthetic = Synthetic {
    motion_x: AtomicI64::new(0),
    motion_y: AtomicI64::new(0),
    wheel_x: AtomicI64::new(0),
    wheel_y: AtomicI64::new(0),
    buttons: AtomicU32::new(0),
    pointer_active: AtomicBool::new(false),
    pointer_x: AtomicI64::new(0),
    pointer_y: AtomicI64::new(0),
    readings: AtomicU64::new(0),
    real_x: AtomicI64::new(0),
    real_y: AtomicI64::new(0),
};

static ORIGINAL_CURRENT: [AtomicUsize; 3] = [const { AtomicUsize::new(0) }; 3];
static ORIGINAL_NEXT: [AtomicUsize; 3] = [const { AtomicUsize::new(0) }; 3];
static HOOKED_VERSIONS: Mutex<Vec<u32>> = Mutex::new(Vec::new());
/// The last mouse reading GameInput returned for each interface version, with a reference held,
/// and the device filter the game asked it with.
static LAST_MOUSE: [AtomicUsize; 3] = [const { AtomicUsize::new(0) }; 3];
static LAST_MOUSE_DEVICE: [AtomicUsize; 3] = [const { AtomicUsize::new(0) }; 3];

/// A reading class seen at runtime: its vtable, interface version and the
/// original `GetMouseState`. `vtable` is published last, with Release.
struct ReadingClass {
    vtable: AtomicUsize,
    version: AtomicUsize,
    mouse_state: AtomicUsize,
}

static CLASSES: [ReadingClass; MAX_READING_CLASSES] = [const {
    ReadingClass {
        vtable: AtomicUsize::new(0),
        version: AtomicUsize::new(0),
        mouse_state: AtomicUsize::new(0),
    }
}; MAX_READING_CLASSES];
static CLASSES_LOCK: Mutex<()> = Mutex::new(());

/// Interface versions whose reading retrieval is hooked.
pub fn hooked_versions() -> Vec<u32> {
    HOOKED_VERSIONS
        .lock()
        .map(|versions| versions.clone())
        .unwrap_or_default()
}

/// Release the buttons and pointer override; motion and wheel stay cumulative.
pub fn release() {
    SYNTHETIC.buttons.store(0, Ordering::Relaxed);
    SYNTHETIC.pointer_active.store(false, Ordering::Relaxed);
}

/// Replace a function-pointer slot (a vtable or import-table entry), returning the previous
/// function.
///
/// # Safety
/// `slot` must point at a live function-pointer slot.
pub(crate) unsafe fn patch(slot: *mut usize, value: usize) -> Option<usize> {
    let mut old: PAGE_PROTECTION_FLAGS = 0;
    if unsafe { VirtualProtect(slot.cast(), size_of::<usize>(), PAGE_READWRITE, &mut old) } == 0 {
        return None;
    }
    // SAFETY: the slot is writable now and a single aligned word.
    let previous = unsafe { AtomicUsize::from_ptr(slot) }.swap(value, Ordering::AcqRel);
    unsafe { VirtualProtect(slot.cast(), size_of::<usize>(), old, &mut old) };
    Some(previous)
}

/// # Safety
/// `object` must be a live COM object.
unsafe fn vtable(object: *mut c_void) -> *mut usize {
    unsafe { *object.cast::<*mut usize>() }
}

fn class_of(reading: *mut c_void) -> Option<&'static ReadingClass> {
    // SAFETY: called only with readings GameInput just returned.
    let table = unsafe { vtable(reading) } as usize;
    CLASSES
        .iter()
        .find(|class| class.vtable.load(Ordering::Acquire) == table)
}

unsafe extern "system" fn mouse_state_hook(reading: *mut c_void, state: *mut c_void) -> bool {
    let Some(class) = class_of(reading) else {
        return false;
    };
    // SAFETY: the class stored GameInput's original GetMouseState for this vtable.
    let original: MouseState =
        unsafe { std::mem::transmute(class.mouse_state.load(Ordering::Acquire)) };
    let result = unsafe { original(reading, state) };
    if !result || state.is_null() {
        return result;
    }
    let s = &SYNTHETIC;
    s.readings.fetch_add(1, Ordering::Relaxed);
    if class.version.load(Ordering::Relaxed) == 0 {
        // SAFETY: v0 readings fill a v0 mouse state.
        let mouse = unsafe { &mut *state.cast::<MouseStateV0>() };
        mouse.buttons |= s.buttons.load(Ordering::Relaxed);
        mouse.x += s.motion_x.load(Ordering::Relaxed);
        mouse.y += s.motion_y.load(Ordering::Relaxed);
        mouse.wheel_x += s.wheel_x.load(Ordering::Relaxed);
        mouse.wheel_y += s.wheel_y.load(Ordering::Relaxed);
        return result;
    }
    // SAFETY: v1 and v2 readings fill a v1 mouse state.
    let mouse = unsafe { &mut *state.cast::<MouseStateV1>() };
    s.real_x.store(mouse.absolute_x, Ordering::Relaxed);
    s.real_y.store(mouse.absolute_y, Ordering::Relaxed);
    mouse.buttons |= s.buttons.load(Ordering::Relaxed);
    mouse.x += s.motion_x.load(Ordering::Relaxed);
    mouse.y += s.motion_y.load(Ordering::Relaxed);
    mouse.wheel_x += s.wheel_x.load(Ordering::Relaxed);
    mouse.wheel_y += s.wheel_y.load(Ordering::Relaxed);
    if s.pointer_active.load(Ordering::Relaxed) {
        // GameInput reports the absolute position in screen coordinates; OreUI screens hit
        // test with it, so a client position here lands their clicks off target.
        let (x, y) = crate::keyboard::client_to_screen(
            s.pointer_x.load(Ordering::Relaxed),
            s.pointer_y.load(Ordering::Relaxed),
        );
        mouse.positions |= ABSOLUTE_POSITION;
        mouse.absolute_x = x;
        mouse.absolute_y = y;
    }
    result
}

/// Patch `GetMouseState` on the class of `reading` the first time it is seen.
fn hook_reading_class(reading: *mut c_void, version: usize) {
    if reading.is_null() || class_of(reading).is_some() {
        return;
    }
    let _guard = CLASSES_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    // SAFETY: GameInput just returned this reading.
    let table = unsafe { vtable(reading) };
    if CLASSES
        .iter()
        .any(|class| class.vtable.load(Ordering::Acquire) == table as usize)
    {
        return;
    }
    let Some(free) = CLASSES
        .iter()
        .find(|class| class.vtable.load(Ordering::Acquire) == 0)
    else {
        return;
    };
    let slot = unsafe { table.add(SLOT_MOUSE_STATE[version]) };
    free.version.store(version, Ordering::Relaxed);
    // SAFETY: the slot exists in every reading vtable of this interface version.
    free.mouse_state.store(unsafe { *slot }, Ordering::Release);
    free.vtable.store(table as usize, Ordering::Release);
    if unsafe { patch(slot, mouse_state_hook as MouseState as usize) }.is_some() {
        log!("hooked GameInput v{version} reading class {table:p}");
    }
}

/// # Safety
/// `object` must be a live COM object.
unsafe fn add_ref(object: *mut c_void) {
    // SAFETY: slot 1 of every COM vtable is AddRef.
    let add_ref: AddRef = unsafe { std::mem::transmute(*vtable(object).add(1)) };
    unsafe { add_ref(object) };
}

/// # Safety
/// `object` must be a live COM object the caller holds a reference to.
unsafe fn release_object(object: *mut c_void) {
    // SAFETY: slot 2 of every COM vtable is Release.
    let release: Release = unsafe { std::mem::transmute(*vtable(object).add(2)) };
    unsafe { release(object) };
}

/// Keep a reference to the newest mouse reading of interface version `V`, taken for `device`.
fn remember_mouse<const V: usize>(reading: *mut c_void, device: *mut c_void) {
    unsafe { add_ref(reading) };
    LAST_MOUSE_DEVICE[V].store(device as usize, Ordering::Release);
    let previous = LAST_MOUSE[V].swap(reading as usize, Ordering::AcqRel);
    if previous != 0 {
        unsafe { release_object(previous as *mut c_void) };
    } else {
        log!("GameInput v{V}: keeping the last mouse reading for background replay");
    }
}

unsafe extern "system" fn current_hook<const V: usize>(
    this: *mut c_void,
    kind: u32,
    device: *mut c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    // SAFETY: stored from the slot this hook replaced.
    let original: CurrentReading =
        unsafe { std::mem::transmute(ORIGINAL_CURRENT[V].load(Ordering::Acquire)) };
    let result = unsafe { original(this, kind, device, out) };
    if out.is_null() {
        return result;
    }
    let mouse = kind & KIND_MOUSE != 0;
    if result >= 0 {
        let reading = unsafe { *out };
        if !reading.is_null() {
            hook_reading_class(reading, V);
            if mouse {
                remember_mouse::<V>(reading, device);
            }
        }
        return result;
    }
    // GameInput withholds input from a process in the background. While a client keeps the
    // game active, hand back the last reading so synthetic input still reaches the game; the
    // real mouse's part of it does not change, so the player's own mouse stays out.
    let last = LAST_MOUSE[V].load(Ordering::Acquire) as *mut c_void;
    let same_device = LAST_MOUSE_DEVICE[V].load(Ordering::Acquire) == device as usize;
    if mouse && same_device && !last.is_null() && crate::focus::held() {
        unsafe { add_ref(last) };
        unsafe { *out = last };
        return 0;
    }
    result
}

unsafe extern "system" fn next_hook<const V: usize>(
    this: *mut c_void,
    reference: *mut c_void,
    kind: u32,
    device: *mut c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    // SAFETY: stored from the slot this hook replaced.
    let original: NextReading =
        unsafe { std::mem::transmute(ORIGINAL_NEXT[V].load(Ordering::Acquire)) };
    let result = unsafe { original(this, reference, kind, device, out) };
    if result >= 0 && !out.is_null() {
        hook_reading_class(unsafe { *out }, V);
    }
    result
}

/// Addresses of the reading-retrieval hooks for interface `version`.
fn hooks_for(version: usize) -> (usize, usize) {
    let (current, next): (CurrentReading, NextReading) = match version {
        0 => (current_hook::<0>, next_hook::<0>),
        1 => (current_hook::<1>, next_hook::<1>),
        _ => (current_hook::<2>, next_hook::<2>),
    };
    (current as usize, next as usize)
}

/// Hook reading retrieval on every IGameInput version the loaded GameInput
/// implements; returns the versions hooked.
pub fn install() -> Vec<u32> {
    for module_name in MODULES {
        let name = wide(module_name);
        let module = unsafe { GetModuleHandleW(name.as_ptr()) };
        if module.is_null() {
            continue;
        }
        let Some(create) = (unsafe { GetProcAddress(module, c"GameInputCreate".as_ptr().cast()) })
        else {
            continue;
        };
        // SAFETY: GameInputCreate has this signature in every GameInput version.
        let create: Create = unsafe { std::mem::transmute(create) };
        let mut root = std::ptr::null_mut();
        if unsafe { create(&mut root) } < 0 || root.is_null() {
            continue;
        }
        for (version, iid) in IIDS.iter().enumerate() {
            let mut object = std::ptr::null_mut();
            // SAFETY: slot 0 of every COM vtable is QueryInterface.
            let query: QueryInterface = unsafe { std::mem::transmute(*vtable(root)) };
            if unsafe { query(root, iid, &mut object) } < 0 || object.is_null() {
                continue;
            }
            let table = unsafe { vtable(object) };
            if ORIGINAL_CURRENT[version].load(Ordering::Acquire) == 0 {
                // Store the originals before the hooks can run.
                let current = unsafe { table.add(SLOT_CURRENT_READING) };
                let next = unsafe { table.add(SLOT_NEXT_READING) };
                ORIGINAL_CURRENT[version].store(unsafe { *current }, Ordering::Release);
                ORIGINAL_NEXT[version].store(unsafe { *next }, Ordering::Release);
                let (current_hook, next_hook) = hooks_for(version);
                let patched = unsafe { patch(current, current_hook) }.is_some()
                    && unsafe { patch(next, next_hook) }.is_some();
                if patched {
                    log!("hooked {module_name} IGameInput v{version}");
                    if let Ok(mut versions) = HOOKED_VERSIONS.lock() {
                        versions.push(version as u32);
                    }
                }
            }
            // SAFETY: slot 2 of every COM vtable is Release.
            let release: Release = unsafe { std::mem::transmute(*vtable(object).add(2)) };
            unsafe { release(object) };
        }
        let release: Release = unsafe { std::mem::transmute(*vtable(root).add(2)) };
        unsafe { release(root) };
    }
    hooked_versions()
}
