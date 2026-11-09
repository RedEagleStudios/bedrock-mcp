//! Perf: the game's frame cadence and its own update timings.
//!
//! `MinecraftGame` implements `IMinecraftGame`, whose slot count may change between game
//! versions. Its `update()` (slot 2, right after `initialize()`; the destructor reuses the
//! base's slot 0) runs on the main thread, and the code treats each call as one frame. The
//! hook timestamps each call, times the original, and then calls
//! `getGameUpdateDurationInSeconds()`, the interface's last virtual and so the vtable's last
//! slot: a const getter that returns a double. Before it is ever called, install decodes its
//! body and takes it only when it is a plain load and return, `movsd xmm0, [rcx + offset];
//! ret`. Slot reference: LeviLamina's `src-client/mc/client/game/IMinecraftGame.h` and
//! `MinecraftGame.h`.
//!
//! The `screens` hook on `SceneStack::update` adds a second, independent cadence: the time
//! between consecutive updates of one stack. Every source keeps the last [`CAPACITY`] samples
//! in memory.

use std::{
    collections::VecDeque,
    ffi::c_void,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use protocol::unstable::{CapabilityState, Perf};

use crate::{capability, memory::Memory, vtable::Resolver};

const NAME: &str = "perf";
const CLASS: &str = "MinecraftGame";
/// The slots up to `stop()`, which the string anchors use.
const SLOTS: usize = 4;
const UPDATE: usize = 2;
const CAPACITY: usize = 1024;
/// A plausible game update, in seconds: anything outside fails the getter's check.
const MAX_UPDATE_SECONDS: f64 = 60.0;
/// The cadence follows one scene stack; another takes over after this long without it.
const STACK_TIMEOUT: Duration = Duration::from_secs(1);
/// `movsd xmm0, [rcx + disp8]` / `[rcx + disp32]`, then `ret`.
const MOVSD_XMM0_RCX_DISP8: [u8; 4] = [0xF2, 0x0F, 0x10, 0x41];
const MOVSD_XMM0_RCX_DISP32: [u8; 4] = [0xF2, 0x0F, 0x10, 0x81];
const RET: u8 = 0xC3;

type Update = unsafe extern "system" fn(*mut c_void);
type GetDuration = unsafe extern "system" fn(*mut c_void) -> f64;

static ORIGINAL_UPDATE: AtomicUsize = AtomicUsize::new(0);
static GET_DURATION_FN: AtomicUsize = AtomicUsize::new(0);
/// Whether the duration getter may be called: set once install decoded it, cleared when it
/// returns an implausible value. Whether the state was reported.
static GETTER_OK: AtomicBool = AtomicBool::new(false);
static REPORTED: AtomicBool = AtomicBool::new(false);
/// Why install did not take the duration getter.
static GETTER_REFUSED: OnceLock<String> = OnceLock::new();
/// Game frames started; zero while `MinecraftGame::update` is not hooked.
static FRAMES: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct Series(VecDeque<(Instant, f32)>);

impl Series {
    fn push(&mut self, at: Instant, value: Duration) {
        if self.0.len() == CAPACITY {
            self.0.pop_front();
        }
        self.0.push_back((at, value.as_secs_f32() * 1000.0));
    }

    fn samples(&self, now: Instant) -> Vec<[f32; 2]> {
        self.0
            .iter()
            .map(|(at, value)| [now.duration_since(*at).as_secs_f32() * 1000.0, *value])
            .collect()
    }
}

struct Samples {
    frame: Series,
    update: Series,
    game_update: Series,
    scene: Series,
    last_update: Option<Instant>,
    /// The stack the scene cadence follows, when it last updated, and the game frame of that
    /// update.
    stack: Option<(usize, Instant, u64)>,
}

static SAMPLES: Mutex<Samples> = Mutex::new(Samples {
    frame: Series(VecDeque::new()),
    update: Series(VecDeque::new()),
    game_update: Series(VecDeque::new()),
    scene: Series(VecDeque::new()),
    last_update: None,
    stack: None,
});

/// Whether the function at `function` only loads a double from its object and returns it.
fn plain_double_getter(function: usize) -> bool {
    let Some(bytes) = Memory::default().read::<[u8; 9]>(function) else {
        return false;
    };
    (bytes[..4] == MOVSD_XMM0_RCX_DISP8 && bytes[5] == RET)
        || (bytes[..4] == MOVSD_XMM0_RCX_DISP32 && bytes[8] == RET)
}

pub fn install(resolver: &Resolver<'_>, scenes: &Result<(), String>) {
    let result = resolver.table_at_least(CLASS, 0, SLOTS).and_then(|table| {
        capability::found(NAME, &[(CLASS, table.found)]);
        let last = table.slots - 1;
        let getter = table.get(last);
        if plain_double_getter(getter) {
            GET_DURATION_FN.store(getter, Ordering::Release);
            GETTER_OK.store(true, Ordering::Release);
        } else {
            let _ = GETTER_REFUSED.set(format!(
                "MinecraftGame's last slot ({last}) is not a plain double getter, so it is not \
                 getGameUpdateDurationInSeconds and is never called"
            ));
        }
        table.hook(UPDATE, update_hook as Update as usize, &ORIGINAL_UPDATE)
    });
    match (result, scenes) {
        (Ok(()), _) => capability::set(
            NAME,
            CapabilityState::Pending,
            "hooked MinecraftGame::update; waiting for the first frame",
        ),
        (Err(reason), Ok(())) => capability::set(
            NAME,
            CapabilityState::Pending,
            format!("scene cadence only, no game update timings: {reason}"),
        ),
        (Err(reason), Err(scenes)) => capability::set(
            NAME,
            CapabilityState::Unavailable,
            format!("{reason}; and no SceneStack::update hook: {scenes}"),
        ),
    }
}

unsafe extern "system" fn update_hook(this: *mut c_void) {
    // SAFETY: stored from the slot this hook replaced.
    let original: Update = unsafe { std::mem::transmute(ORIGINAL_UPDATE.load(Ordering::Acquire)) };
    let start = Instant::now();
    FRAMES.fetch_add(1, Ordering::Relaxed);
    unsafe { original(this) };
    let elapsed = start.elapsed();
    // SAFETY: the vtable's last slot, decoded at install as a plain load of a double from the
    // object; called on the object being updated, on the thread that updates it. It stops being
    // called once it returns an implausible value.
    let seconds = if GETTER_OK.load(Ordering::Acquire) {
        let get: GetDuration =
            unsafe { std::mem::transmute(GET_DURATION_FN.load(Ordering::Acquire)) };
        Some(unsafe { get(this) })
    } else {
        None
    };
    let Ok(mut samples) = SAMPLES.lock() else {
        return;
    };
    if let Some(last) = samples.last_update.replace(start) {
        samples.frame.push(start, start.duration_since(last));
    }
    samples.update.push(start, elapsed);
    match seconds {
        Some(seconds) if seconds.is_finite() && (0.0..MAX_UPDATE_SECONDS).contains(&seconds) => {
            samples
                .game_update
                .push(start, Duration::from_secs_f64(seconds));
            drop(samples);
            if !REPORTED.swap(true, Ordering::Relaxed) {
                capability::set(
                    NAME,
                    CapabilityState::Active,
                    "MinecraftGame::update is called and getGameUpdateDurationInSeconds is plausible",
                );
            }
        }
        Some(seconds) => {
            drop(samples);
            GETTER_OK.store(false, Ordering::Relaxed);
            REPORTED.store(true, Ordering::Relaxed);
            capability::set(
                NAME,
                CapabilityState::Active,
                format!(
                    "frame timings only: getGameUpdateDurationInSeconds returned {seconds}, so it is no longer called"
                ),
            );
        }
        None => {
            drop(samples);
            if let Some(reason) = GETTER_REFUSED.get()
                && !REPORTED.swap(true, Ordering::Relaxed)
            {
                capability::set(
                    NAME,
                    CapabilityState::Active,
                    format!("frame timings only: {reason}"),
                );
            }
        }
    }
}

/// Called by the `screens` hook after each `SceneStack::update` of stack `id`. Each stack
/// updates at least once per frame; the code counts only its first update of each frame.
pub fn scene_frame(id: usize) {
    let now = Instant::now();
    let frame = FRAMES.load(Ordering::Relaxed);
    let Ok(mut samples) = SAMPLES.lock() else {
        return;
    };
    match samples.stack {
        Some((stack, _, seen)) if stack == id && frame != 0 && frame == seen => {}
        Some((stack, last, _)) if stack == id => {
            samples.scene.push(now, now.duration_since(last));
            samples.stack = Some((id, now, frame));
        }
        Some((_, last, _)) if now.duration_since(last) < STACK_TIMEOUT => {}
        _ => samples.stack = Some((id, now, frame)),
    }
    let only_scenes = samples.last_update.is_none() && !samples.scene.0.is_empty();
    drop(samples);
    if only_scenes
        && ORIGINAL_UPDATE.load(Ordering::Relaxed) == 0
        && !REPORTED.swap(true, Ordering::Relaxed)
    {
        capability::set(
            NAME,
            CapabilityState::Active,
            "scene cadence only: MinecraftGame::update is not hooked on this build",
        );
    }
}

/// Every sample kept, oldest first.
pub fn read() -> Perf {
    let now = Instant::now();
    SAMPLES
        .lock()
        .map(|samples| Perf {
            frame: samples.frame.samples(now),
            update: samples.update.samples(now),
            game_update: samples.game_update.samples(now),
            scene: samples.scene.samples(now),
        })
        .unwrap_or_default()
}
