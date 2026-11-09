//! The camera: the player's view perspective, the field of view and the camera the world
//! is rendered with.
//!
//! Two hooks observe calls the game makes anyway:
//! - `VanillaCameraAPI::getPlayerViewPerspectiveOption` (slot 6 of 8) returns the
//!   perspective option as a `PlayerViewMode` in a register. Layout reference: LeviLamina's
//!   `src-client/mc/deps/vanilla_camera/{IVanillaCameraAPI,VanillaCameraAPI}.h` and
//!   `src/mc/deps/shared_types/v1_21_100/camera/PlayerViewMode.h`. The object's
//!   `mClientInstance` (+8) names the `ClientInstance`, kept once its vtable checks out.
//! - `CameraAPI::tryGetFOV` (slot 6 of 17) fills a caller-provided `std::optional<float>`
//!   (MSVC returns class types from member functions through a hidden pointer); the code
//!   accepts the value only within (0, π). `src-client/mc/deps/minecraft_camera/ICameraAPI.h`,
//!   `.../vanilla_camera/CameraAPI.h`.
//!
//! The rendered camera is the `mce::Camera` that `getCamera` returns
//! (`src-client/mc/deps/renderer/Camera.h`: three 64-byte matrix stacks and a 64-byte
//! matrix, then right, up, forward and position vectors, the aspect ratio and the field of
//! view). In `src-client/mc/client/game/IClientInstance.h`, `getCamera` is followed by
//! `getShaderColor`. The code assumes both are one-instruction getters,
//! `lea rax, [rcx + offset]; ret`, and that the shader colour member follows the camera.
//! Install decodes the vtable's slots at runtime and accepts only a unique pair of adjacent
//! getters whose second offset is exactly the camera's 512 bytes past the first; the first is
//! the camera's offset. The `screens` hook copies the camera on the game thread every few
//! frames and keeps it only when its basis is orthonormal and its numbers are plausible.

use std::{
    ffi::c_void,
    sync::{
        LazyLock, Mutex, OnceLock,
        atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
    time::Instant,
};

use protocol::unstable::{CameraState, CameraView, CapabilityState, Perspective};

use crate::{
    capability,
    memory::Memory,
    vtable::{Resolver, Table},
};

const NAME: &str = "camera";

const VANILLA_API: &str = "VanillaCameraAPI";
const VANILLA_API_SLOTS: usize = 8;
const GET_PERSPECTIVE: usize = 6;
const VANILLA_API_CLIENT: usize = 8;

const CAMERA_API: &str = "CameraAPI";
const CAMERA_API_SLOTS: usize = 17;
const TRY_GET_FOV: usize = 6;

/// `sizeof(mce::Camera)`, from the header.
const CAMERA_SIZE: usize = 512;
/// Frames between camera reads.
const CAMERA_FRAMES: u64 = 2;

const CLIENT_INSTANCE: &str = "ClientInstance";
/// The virtuals LeviLamina's `IClientInstance.h` declares; the code accepts more.
const CLIENT_INSTANCE_SLOTS: usize = 419;

/// `mce::Camera`: `mRight`, then `mUp`, `mForward`, `mPosition`, `mAspectRatio`, `mFov`.
const CAMERA_RIGHT: usize = 256;
const CAMERA_READ: usize = 312;

/// A world position farther out than this is not plausible.
const MAX_POSITION: f32 = 1.0e8;
/// Tolerance of the unit-length and orthogonality checks.
const BASIS_EPSILON: f32 = 1.0e-2;

type GetPerspective = unsafe extern "system" fn(*mut c_void) -> i32;
type TryGetFov = unsafe extern "system" fn(*mut c_void, *mut OptionalFloat) -> *mut OptionalFloat;
/// `lea rax, [rcx + disp32]` and `ret`.
const LEA_RAX_RCX: [u8; 3] = [0x48, 0x8D, 0x81];
const RET: u8 = 0xC3;

/// MSVC `std::optional<float>`: the value, then the engaged flag.
#[repr(C)]
struct OptionalFloat {
    value: f32,
    engaged: u8,
}

static ORIGINAL_PERSPECTIVE: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_FOV: AtomicUsize = AtomicUsize::new(0);
/// `mce::Camera`'s offset in the `ClientInstance`, zero when not found.
static CAMERA_OFFSET: AtomicUsize = AtomicUsize::new(0);
static FRAME: AtomicU64 = AtomicU64::new(0);
static CLIENT_TABLE: AtomicUsize = AtomicUsize::new(0);
/// The `ClientInstance` the camera API belongs to, once its vtable checked out.
static CLIENT: AtomicUsize = AtomicUsize::new(0);

/// Milliseconds since [`START`] of the last perspective and FOV reads, zero before one;
/// the values themselves.
static PERSPECTIVE: AtomicU32 = AtomicU32::new(u32::MAX);
static PERSPECTIVE_AT: AtomicU64 = AtomicU64::new(0);
static FOV: AtomicU32 = AtomicU32::new(0);
static FOV_AT: AtomicU64 = AtomicU64::new(0);

struct View {
    view: CameraView,
    at: u64,
}

static VIEW: Mutex<Option<View>> = Mutex::new(None);
/// Why the last camera read was rejected; cleared by a good one.
static VIEW_REJECTED: Mutex<Option<String>> = Mutex::new(None);
/// The parts that did not install.
static MISSING: OnceLock<String> = OnceLock::new();
static START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Milliseconds since install, at least 1 so zero means never.
fn now() -> u64 {
    START.elapsed().as_millis() as u64 + 1
}

fn age(at: u64) -> Option<u64> {
    (at != 0).then(|| now().saturating_sub(at))
}

unsafe extern "system" fn perspective_hook(this: *mut c_void) -> i32 {
    // SAFETY: stored from the slot this hook replaced.
    let original: GetPerspective =
        unsafe { std::mem::transmute(ORIGINAL_PERSPECTIVE.load(Ordering::Acquire)) };
    let mode = unsafe { original(this) };
    if (0..=2).contains(&mode) {
        if PERSPECTIVE.swap(mode as u32, Ordering::AcqRel) == u32::MAX {
            capability::set(
                NAME,
                CapabilityState::Active,
                "getPlayerViewPerspectiveOption confirmed with a valid view mode",
            );
        }
        PERSPECTIVE_AT.store(now(), Ordering::Release);
        if CLIENT.load(Ordering::Acquire) == 0 {
            publish_client(this as usize);
        }
    } else {
        capability::set(
            NAME,
            CapabilityState::Failed,
            format!("getPlayerViewPerspectiveOption returned {mode}, not a view mode"),
        );
    }
    mode
}

/// Publish the camera API's `mClientInstance` when it is a `ClientInstance`.
fn publish_client(api: usize) {
    let mut memory = Memory::default();
    let Some(client) = memory.read::<usize>(api + VANILLA_API_CLIENT) else {
        return;
    };
    if memory.object(client, 8)
        && memory.read::<usize>(client) == Some(CLIENT_TABLE.load(Ordering::Acquire))
    {
        CLIENT.store(client, Ordering::Release);
    }
}

/// The `ClientInstance` the game's camera uses, or 0 before the camera ran.
pub fn client() -> usize {
    CLIENT.load(Ordering::Acquire)
}

unsafe extern "system" fn fov_hook(
    this: *mut c_void,
    out: *mut OptionalFloat,
) -> *mut OptionalFloat {
    // SAFETY: stored from the slot this hook replaced.
    let original: TryGetFov = unsafe { std::mem::transmute(ORIGINAL_FOV.load(Ordering::Acquire)) };
    let result = unsafe { original(this, out) };
    if !result.is_null() {
        // SAFETY: the game just filled the optional it passed and returned.
        let OptionalFloat { value, engaged } = unsafe { std::ptr::read(result) };
        if engaged == 1 && value.is_finite() && value > 0.0 && value < std::f32::consts::PI {
            FOV.store(value.to_bits(), Ordering::Release);
            FOV_AT.store(now(), Ordering::Release);
        }
    }
    result
}

/// Called by the `screens` hook on the game thread after each `SceneStack::update`.
pub fn frame() {
    let offset = CAMERA_OFFSET.load(Ordering::Acquire);
    let client = CLIENT.load(Ordering::Acquire);
    if offset == 0
        || client == 0
        || !FRAME
            .fetch_add(1, Ordering::AcqRel)
            .is_multiple_of(CAMERA_FRAMES)
    {
        return;
    }
    let mut memory = Memory::default();
    if memory.read::<usize>(client) != Some(CLIENT_TABLE.load(Ordering::Acquire)) {
        CLIENT.store(0, Ordering::Release);
        return;
    }
    let result = read_view(&mut memory, client + offset);
    match result {
        Ok(view) => {
            if let Ok(mut slot) = VIEW.lock() {
                *slot = Some(View { view, at: now() });
            }
            if let Ok(mut rejected) = VIEW_REJECTED.lock() {
                *rejected = None;
            }
        }
        Err(reason) => {
            if let Ok(mut rejected) = VIEW_REJECTED.lock() {
                *rejected = Some(reason);
            }
        }
    }
}

/// The offset a `lea rax, [rcx + offset]; ret` getter at `function` returns.
fn getter_offset(function: usize) -> Option<usize> {
    let bytes = Memory::default().read::<[u8; 8]>(function)?;
    (bytes[..3] == LEA_RAX_RCX && bytes[7] == RET)
        .then(|| i32::from_le_bytes(bytes[3..7].try_into().unwrap()))
        .and_then(|offset| usize::try_from(offset).ok())
}

/// The camera's offset in the `ClientInstance`: the first offset of the one pair of adjacent
/// getters in `table` whose second returns the member right after a 512-byte first one
/// (`getCamera`, then `getShaderColor`). None when no pair or several pairs match.
fn camera_offset(table: &Table) -> Option<usize> {
    let offsets: Vec<Option<usize>> = (0..table.slots)
        .map(|slot| getter_offset(table.get(slot)))
        .collect();
    let mut pairs = offsets.windows(2).filter_map(|pair| match *pair {
        [Some(camera), Some(color)] if color == camera + CAMERA_SIZE => Some(camera),
        _ => None,
    });
    let camera = pairs.next()?;
    pairs.next().is_none().then_some(camera)
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// The camera at `address`, checked.
fn read_view(memory: &mut Memory, address: usize) -> Result<CameraView, String> {
    if !memory.object(address, CAMERA_READ) {
        return Err("layout: the camera is not readable".into());
    }
    let [right, up, forward, position]: [[f32; 3]; 4] = memory
        .read(address + CAMERA_RIGHT)
        .ok_or("layout: the camera is not readable")?;
    let [aspect_ratio, fov]: [f32; 2] = memory
        .read(address + CAMERA_RIGHT + 48)
        .ok_or("layout: the camera is not readable")?;
    let unit = |v: [f32; 3]| (dot(v, v) - 1.0).abs() < BASIS_EPSILON;
    let handed = cross(right, up);
    if !(unit(right) && unit(up) && unit(forward))
        || dot(right, up).abs() > BASIS_EPSILON
        || dot(up, forward).abs() > BASIS_EPSILON
        || (dot(handed, forward).abs() - 1.0).abs() > BASIS_EPSILON
    {
        return Err("layout: mce::Camera's basis is not orthonormal".into());
    }
    if !position
        .iter()
        .all(|value| value.is_finite() && value.abs() < MAX_POSITION)
    {
        return Err("layout: mce::Camera's position is not plausible".into());
    }
    // The code reports these fields only when they are plausible.
    let fov = (fov > 0.0 && fov < std::f32::consts::PI).then(|| fov.to_degrees());
    let aspect_ratio = (aspect_ratio.is_finite() && aspect_ratio > 0.05 && aspect_ratio < 20.0)
        .then_some(aspect_ratio);
    Ok(CameraView {
        position,
        forward,
        up,
        fov,
        aspect_ratio,
        age_ms: 0,
    })
}

pub fn install(resolver: &Resolver<'_>) {
    LazyLock::force(&START);
    let mut found = Vec::new();
    let client = resolver.table_at_least(CLIENT_INSTANCE, 0, CLIENT_INSTANCE_SLOTS);
    if let Ok(table) = &client {
        CLIENT_TABLE.store(table.address, Ordering::Release);
        found.push((CLIENT_INSTANCE, table.found));
    }
    let result = resolver
        .table(VANILLA_API, 0, VANILLA_API_SLOTS)
        .and_then(|table| {
            found.push((VANILLA_API, table.found));
            table.hook(
                GET_PERSPECTIVE,
                perspective_hook as GetPerspective as usize,
                &ORIGINAL_PERSPECTIVE,
            )
        });
    let mut missing = Vec::new();
    match resolver
        .table(CAMERA_API, 0, CAMERA_API_SLOTS)
        .and_then(|table| {
            found.push((CAMERA_API, table.found));
            table.hook(TRY_GET_FOV, fov_hook as TryGetFov as usize, &ORIGINAL_FOV)
        }) {
        Ok(()) => {}
        Err(reason) => missing.push(format!("no field of view: {reason}")),
    }
    match &client {
        Ok(table) => match camera_offset(table) {
            Some(camera) => CAMERA_OFFSET.store(camera, Ordering::Release),
            None => missing.push(
                "no rendered view: no single pair of adjacent ClientInstance getters returns a \
                 512-byte member and the one after it (getCamera, getShaderColor)"
                    .to_owned(),
            ),
        },
        Err(reason) => missing.push(format!("no rendered view: {reason}")),
    }
    if let Err(reason) = client {
        missing.push(format!("no client for hud_text: {reason}"));
    }
    let _ = MISSING.set(missing.join("; "));
    capability::found(NAME, &found);
    match result {
        Ok(()) => capability::set(
            NAME,
            CapabilityState::Pending,
            "hooked; waiting for the camera to read the perspective",
        ),
        Err(reason) => capability::set(NAME, CapabilityState::Unavailable, reason),
    }
}

pub fn read() -> CameraState {
    let perspective = match PERSPECTIVE.load(Ordering::Acquire) {
        0 => Some(Perspective::FirstPerson),
        1 => Some(Perspective::ThirdPersonBack),
        2 => Some(Perspective::ThirdPersonFront),
        _ => None,
    };
    let fov_at = FOV_AT.load(Ordering::Acquire);
    let view = VIEW.lock().ok().and_then(|view| {
        view.as_ref().map(|view| CameraView {
            age_ms: now().saturating_sub(view.at),
            ..view.view.clone()
        })
    });
    let rejected = VIEW_REJECTED
        .lock()
        .ok()
        .and_then(|rejected| rejected.clone());
    let detail = [MISSING.get().cloned(), rejected]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    let detail = (!detail.is_empty()).then_some(detail);
    CameraState {
        perspective,
        perspective_age_ms: age(PERSPECTIVE_AT.load(Ordering::Acquire)),
        fov: (fov_at != 0).then(|| f32::from_bits(FOV.load(Ordering::Acquire)).to_degrees()),
        fov_age_ms: age(fov_at),
        view,
        detail,
    }
}
