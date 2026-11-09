//! Player render: the local player's animation state and its attachables.
//!
//! The game applies each client animation component's animations to its pose through
//! `visitApplyContext`, slot 2 of `ClientAnimationComponent`'s 10 (the order of
//! `AnimationComponent.h`). The hook calls the original first, then looks at the component it
//! was called for.
//!
//! They track every client component of the local player and the child components
//! (attachables) of those. A component's `mOwnerUUID` is an `AnimationComponentID` whose low 54
//! bits hold the owning actor's unique id. Which actor is the local player is our inference, not
//! something the headers state: the owner of a client HUD or first-person component (the
//! `AnimationComponentGroupType` values), on the assumption that no other actor is drawn in those
//! groups, or the id the request names. A tracked component is read only when a request waits
//! for it, on the thread that is evaluating it, and only for that call; no game pointer is kept
//! past the call except as an identity.
//!
//! Layouts, from LeviLamina's headers under `src/mc/world/actor/animation/`:
//! - `AnimationComponent.h`: `mComponentAnimationPlayers` (shared pointers to
//!   `ActorAnimationPlayer`), `mChildAnimationComponents` (80-byte `ChildAnimationComponentInfo`
//!   entries holding a `shared_ptr<AnimationComponent>` at 64), `mBoneOrientations` (an
//!   unordered map), then `mAnimationComponentInitialized`, `mAnimationComponentGroupType`,
//!   `mOwnerUUID`, `mLastUpdateFrame`, `mLastReloadInitTimeStampClient` and `mApplyAnimations`.
//!   `AnimationComponentGroupType.h` gives the group values.
//! - `ActorAnimationPlayer.h`: `mFriendlyName`, a `HashedString`
//!   (`src/mc/deps/core/string/HashedString.h`: hash, then `std::string`).
//! - `ActorAnimationControllerPlayer.h`: `mCurrStateIndex`, `mLastStateIndex`, then
//!   `mAnimationControllerStatePlayers`.
//! - `ActorAnimationControllerStatePlayer.h`: `mOwner` (its controller), `mAnimationPlayers`
//!   and `mStateTime`.
//!
//! Members may move between game versions, so the DLL does not trust the header offsets. It finds
//! each block on live objects by anchors: the class vtable behind every pointer, a valid
//! `std::vector` and unordered map, group values in range, a friendly name whose stored hash
//! matches its text, and a state player whose `mOwner` points back at its controller. A block
//! that matches no candidate, or more than one, leaves the capability failed with the reason.
//! The vtables come from RTTI, or from the string anchors on a build without it.

use std::{
    collections::HashMap,
    ffi::c_void,
    sync::{
        Condvar, Mutex, OnceLock,
        atomic::{AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use endpoint::log;
use protocol::unstable::{
    AnimationComponent, AnimationPlayer, CapabilityState, PlayerRender, Visits,
};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;

use crate::{capability, memory::Memory, vtable::Resolver};

const NAME: &str = "player_render";
const CLASS: &str = "ClientAnimationComponent";
const SLOTS: usize = 10;
const VISIT_APPLY_CONTEXT: usize = 2;
const CONTROLLER_CLASS: &str = "ActorAnimationControllerPlayer";
const STATE_CLASS: &str = "ActorAnimationControllerStatePlayer";
const SKELETAL_CLASS: &str = "ActorSkeletalAnimationPlayer";
/// `ActorAnimationPlayer.h` declares 11 virtuals; the three players add none.
const PLAYER_SLOTS: usize = 11;

/// `AnimationComponentGroupType` values.
const CLIENT_HUD: u32 = 1;
const SERVER: u32 = 2;
const FIRST_PERSON: u32 = 5;
/// `AnimationComponentID`: the owner's actor unique id in the low 54 bits; the attachable
/// index and sub-client id above it (`AnimationComponentID.h`).
const ACTOR_BITS: u64 = (1 << 54) - 1;
const GROUPS: usize = 6;

/// Search windows for each calibrated block, in bytes from the object start.
const BACK_BLOCK: (usize, usize) = (768, 1024);
const PLAYERS_BLOCK: (usize, usize) = (16, 64);
const NAME_BLOCK: (usize, usize) = (8, 56);
const STATES_BLOCK: (usize, usize) = (80, 232);
const STATE_OWNER_BLOCK: (usize, usize) = (80, 200);
const STATE_ANIMATIONS_SPAN: usize = 96;
/// From `mAnimationComponentInitialized` back to `mChildAnimationComponents` and
/// `mBoneOrientations`, and on to the fields after it.
const CHILDREN_BEFORE: usize = 88;
const BONES_BEFORE: usize = 64;
const GROUP_AFTER: usize = 4;
const OWNER_AFTER: usize = 8;
const FRAME_AFTER: usize = 16;
const APPLY_AFTER: usize = 32;
/// `mCurrStateIndex` and `mLastStateIndex` before the state player vector.
const STATE_INDEX_BEFORE: usize = 24;
const LAST_STATE_INDEX_BEFORE: usize = 20;
/// `mStateTime` after the state's animation vector.
const STATE_TIME_AFTER: usize = 24;
/// `ChildAnimationComponentInfo`: size, and its `shared_ptr<AnimationComponent>`.
const CHILD_INFO: usize = 80;
const CHILD_POINTER: usize = 64;

/// Bounds.
const MAX_PLAYERS: usize = 128;
const MAX_CHILDREN: usize = 16;
const MAX_STATES: usize = 128;
const MAX_STATE_ANIMATIONS: usize = 32;
const MAX_NAME: usize = 256;
const MAX_DEPTH: usize = 2;
const MAX_TRACKED: usize = 32;
const MAX_FRAME: i64 = 1 << 40;
const MAX_STATE_TIME: f32 = 1.0e7;
/// Calls that may try to calibrate before the capability gives up.
const MAX_CALIBRATIONS: u32 = 5000;
/// A tracked component unseen this long is dropped.
const FORGET: Duration = Duration::from_secs(10);
/// How often a tracked parent's child list is refreshed.
const CHILD_REFRESH: Duration = Duration::from_millis(200);
const DEFAULT_WAIT: Duration = Duration::from_millis(250);
const MAX_WAIT: Duration = Duration::from_secs(2);

type VisitApplyContext =
    unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void);

static ORIGINAL_APPLY: AtomicUsize = AtomicUsize::new(0);

/// Primary vtable addresses the reads recognize.
struct Classes {
    component: usize,
    controller: Option<usize>,
    state: Option<usize>,
    skeletal: Option<usize>,
}

impl Classes {
    fn player(&self, table: usize) -> bool {
        [self.controller, self.state, self.skeletal].contains(&Some(table))
    }
}

static CLASSES: OnceLock<Classes> = OnceLock::new();

/// Component and player offsets found on live objects.
#[derive(Clone, Copy, Debug)]
struct Layout {
    players: usize,
    initialized: usize,
    name: usize,
}

impl Layout {
    fn children(&self) -> usize {
        self.initialized - CHILDREN_BEFORE
    }
}

static LAYOUT: OnceLock<Layout> = OnceLock::new();
/// `mAnimationComponentGroupType` once the layout is known; zero until then. The hot path
/// reads only this.
static GROUP_AT: AtomicUsize = AtomicUsize::new(0);
static CALIBRATIONS: AtomicU32 = AtomicU32::new(0);
static CALIBRATING: Mutex<()> = Mutex::new(());

/// Controller offsets, found on the first controller read: the state player vector, and
/// `mOwner` inside a state player.
static CONTROLLER: OnceLock<(usize, usize)> = OnceLock::new();
/// `mAnimationPlayers` inside a state player, found on the first state that plays one.
static STATE_ANIMATIONS: OnceLock<usize> = OnceLock::new();

static VISITS: [AtomicU64; GROUPS] = [const { AtomicU64::new(0) }; GROUPS];
static FRAME: AtomicI64 = AtomicI64::new(0);
/// The local player's actor id, from the last client HUD or first-person component seen (our
/// inference that no other actor has those); zero before.
static LOCAL_ACTOR: AtomicU64 = AtomicU64::new(0);
/// Children of tracked components with their parent, so their own calls are tracked too.
static CHILDREN: [(AtomicUsize, AtomicUsize); MAX_TRACKED] =
    [const { (AtomicUsize::new(0), AtomicUsize::new(0)) }; MAX_TRACKED];

#[derive(Default, Clone, Copy)]
struct Stat {
    count: u64,
    last: Option<Instant>,
    thread: u32,
}

impl Stat {
    fn visits(&self, now: Instant) -> Option<Visits> {
        self.last.map(|last| Visits {
            count: self.count,
            age_ms: now.duration_since(last).as_millis() as u64,
            thread: self.thread,
        })
    }
}

struct Entry {
    group: u32,
    child: bool,
    apply: Stat,
    last_seen: Instant,
    children_read: Option<Instant>,
    /// The request generation it answered, when, and what it read.
    read: Option<(u64, Instant, AnimationComponent)>,
}

static WANTED: AtomicU64 = AtomicU64::new(0);
static ENTRIES: Mutex<Vec<(usize, Entry)>> = Mutex::new(Vec::new());
static READ_DONE: Condvar = Condvar::new();

pub fn install(resolver: &Resolver<'_>) {
    let result = resolver.table(CLASS, 0, SLOTS).and_then(|table| {
        let mut found = vec![(CLASS, table.found)];
        let mut missing = Vec::new();
        let mut player = |name| match resolver.table(name, 0, PLAYER_SLOTS) {
            Ok(player) => {
                found.push((name, player.found));
                Some(player.address)
            }
            Err(reason) => {
                missing.push(reason);
                None
            }
        };
        let classes = Classes {
            component: table.address,
            controller: player(CONTROLLER_CLASS),
            state: player(STATE_CLASS),
            skeletal: player(SKELETAL_CLASS),
        };
        capability::found(NAME, &found);
        if !missing.is_empty() {
            log!("player_render: {}", missing.join("; "));
        }
        if classes.controller.is_none() && classes.skeletal.is_none() {
            return Err(missing.join("; "));
        }
        CLASSES
            .set(classes)
            .map_err(|_| "installed twice".to_owned())?;
        table.hook(
            VISIT_APPLY_CONTEXT,
            apply_hook as VisitApplyContext as usize,
            &ORIGINAL_APPLY,
        )
    });
    match result {
        Ok(()) => capability::set(
            NAME,
            CapabilityState::Pending,
            "hooked; waiting for animated actors to find the layout",
        ),
        Err(reason) => capability::set(NAME, CapabilityState::Unavailable, reason),
    }
}

unsafe extern "system" fn apply_hook(
    this: *mut c_void,
    visitor: *mut c_void,
    r8: *mut c_void,
    r9: *mut c_void,
) {
    // SAFETY: stored from the slot this hook replaced.
    let original: VisitApplyContext =
        unsafe { std::mem::transmute(ORIGINAL_APPLY.load(Ordering::Acquire)) };
    unsafe { original(this, visitor, r8, r9) };
    observe(this as usize);
}

/// Runs on every hooked call: two field reads, then work only for tracked components.
fn observe(this: usize) {
    let group_at = GROUP_AT.load(Ordering::Acquire);
    if group_at == 0 {
        calibrate(this);
        return;
    }
    // SAFETY: `this` is the live component the game just called through its own vtable, and
    // the calibrated fields lie inside every `AnimationComponent`.
    let (group, owner, frame) = unsafe {
        (
            ((this + group_at) as *const u32).read(),
            ((this + group_at - GROUP_AFTER + OWNER_AFTER) as *const u64).read(),
            ((this + group_at - GROUP_AFTER + FRAME_AFTER) as *const i64).read(),
        )
    };
    if let Some(visits) = VISITS.get(group as usize) {
        visits.fetch_add(1, Ordering::Relaxed);
    }
    if (0..MAX_FRAME).contains(&frame) {
        FRAME.fetch_max(frame, Ordering::Relaxed);
    }
    let child = CHILDREN
        .iter()
        .any(|(known, _)| known.load(Ordering::Relaxed) == this);
    let actor = owner & ACTOR_BITS;
    if group == CLIENT_HUD || group == FIRST_PERSON {
        LOCAL_ACTOR.store(actor, Ordering::Relaxed);
    }
    let local = group != SERVER && actor != 0 && actor == LOCAL_ACTOR.load(Ordering::Relaxed);
    if local || child {
        track(this, group, child);
    }
}

/// Record a call of a tracked component and read it when a request waits for it.
fn track(this: usize, group: u32, child: bool) {
    let now = Instant::now();
    let wanted = WANTED.load(Ordering::Acquire);
    let (read, refresh) = {
        let Ok(mut entries) = ENTRIES.lock() else {
            return;
        };
        let before = entries.len();
        entries.retain(|(_, entry)| now.duration_since(entry.last_seen) < FORGET);
        if entries.len() != before {
            let live: Vec<usize> = entries.iter().map(|(id, _)| *id).collect();
            forget_children(|parent, _| !live.contains(&parent));
        }
        let index = match entries.iter().position(|(id, _)| *id == this) {
            Some(index) => index,
            None if entries.len() < MAX_TRACKED => {
                entries.push((
                    this,
                    Entry {
                        group,
                        child,
                        apply: Stat::default(),
                        last_seen: now,
                        children_read: None,
                        read: None,
                    },
                ));
                entries.len() - 1
            }
            None => return,
        };
        let entry = &mut entries[index].1;
        entry.group = group;
        entry.child = child;
        entry.last_seen = now;
        let stat = &mut entry.apply;
        stat.count += 1;
        stat.last = Some(now);
        // SAFETY: no preconditions.
        stat.thread = unsafe { GetCurrentThreadId() };
        let read = wanted != 0 && entry.read.as_ref().is_none_or(|(at, ..)| *at < wanted);
        let refresh = !child
            && entry
                .children_read
                .is_none_or(|at| now.duration_since(at) >= CHILD_REFRESH);
        if refresh {
            entry.children_read = Some(now);
        }
        (read, refresh)
    };
    let (Some(classes), Some(layout)) = (CLASSES.get(), LAYOUT.get()) else {
        return;
    };
    let mut memory = Memory::default();
    if refresh {
        remember_children(&mut memory, classes, layout, this);
    }
    if !read {
        return;
    }
    let start = Instant::now();
    let mut component = read_component(&mut memory, classes, layout, this, 0);
    component.read_us = start.elapsed().as_micros() as u64;
    let Ok(mut entries) = ENTRIES.lock() else {
        return;
    };
    if let Some((_, entry)) = entries.iter_mut().find(|(id, _)| *id == this) {
        entry.read = Some((wanted, Instant::now(), component));
    }
    drop(entries);
    READ_DONE.notify_all();
}

/// Make [`CHILDREN`] list the current children of `parent`.
fn remember_children(memory: &mut Memory, classes: &Classes, layout: &Layout, parent: usize) {
    let current = children(memory, classes, layout, parent);
    forget_children(|owner, child| owner == parent && !current.contains(&child));
    for child in current {
        if CHILDREN
            .iter()
            .any(|(known, _)| known.load(Ordering::Relaxed) == child)
        {
            continue;
        }
        if let Some((_, owner)) = CHILDREN.iter().find(|(slot, _)| {
            slot.compare_exchange(0, child, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        }) {
            owner.store(parent, Ordering::Release);
        }
    }
}

/// Free the [`CHILDREN`] slots whose (parent, child) `drop` accepts.
fn forget_children(drop: impl Fn(usize, usize) -> bool) {
    for (child, owner) in &CHILDREN {
        let known = child.load(Ordering::Acquire);
        if known != 0 && drop(owner.load(Ordering::Acquire), known) {
            owner.store(0, Ordering::Release);
            child.store(0, Ordering::Release);
        }
    }
}

/// The child component pointers of `component`.
fn children(
    memory: &mut Memory,
    classes: &Classes,
    layout: &Layout,
    component: usize,
) -> Vec<usize> {
    let Some((first, count)) =
        memory.vector(component + layout.children(), CHILD_INFO, MAX_CHILDREN)
    else {
        return Vec::new();
    };
    (0..count)
        .filter_map(|index| {
            let child = memory.read::<usize>(first + index * CHILD_INFO + CHILD_POINTER)?;
            (child != 0
                && memory.object(child, 8)
                && memory.read::<usize>(child) == Some(classes.component))
            .then_some(child)
        })
        .collect()
}

/// FNV-1 as `HashedString::computeHash` computes it: multiply, then xor the signed byte.
fn hashed(text: &[u8]) -> u64 {
    if text.is_empty() {
        return 0;
    }
    text.iter().fold(0xCBF2_9CE4_8422_2325, |hash: u64, &byte| {
        hash.wrapping_mul(0x0000_0100_0000_01B3) ^ (byte as i8 as i64 as u64)
    })
}

/// The raw bytes of the MSVC `std::string` at `address`, at most `max`.
fn raw_string(memory: &mut Memory, address: usize, max: usize) -> Option<Vec<u8>> {
    let (buffer, size, capacity) = memory.read::<([u8; 16], usize, usize)>(address)?;
    if capacity < size || capacity > 1 << 30 || size > max {
        return None;
    }
    let data = if capacity >= 16 {
        usize::from_le_bytes(buffer[..8].try_into().ok()?)
    } else {
        address
    };
    if !memory.readable(data, size) {
        return None;
    }
    // SAFETY: checked readable above.
    Some(unsafe { std::slice::from_raw_parts(data as *const u8, size) }.to_vec())
}

/// The `HashedString` at `address`, when its stored hash matches its text.
fn hashed_string(memory: &mut Memory, address: usize) -> Option<String> {
    let hash = memory.read::<u64>(address)?;
    let text = raw_string(memory, address + 8, MAX_NAME)?;
    (hashed(&text) == hash && text.iter().all(|byte| byte.is_ascii_graphic()))
        .then(|| String::from_utf8_lossy(&text).into_owned())
}

/// The shared pointers' targets in the vector at `address` whose vtable passes `known`.
fn pointers(
    memory: &mut Memory,
    address: usize,
    max: usize,
    known: impl Fn(usize) -> bool,
) -> Option<Vec<(usize, usize)>> {
    let (first, count) = memory.vector(address, 16, max)?;
    (0..count)
        .map(|index| {
            let target = memory.read::<usize>(first + index * 16)?;
            if !memory.object(target, 16) {
                return None;
            }
            let table = memory.read::<usize>(target)?;
            known(table).then_some((target, table))
        })
        .collect()
}

/// The single offset in `range` (8-byte steps) that `test` accepts.
fn unique(range: (usize, usize), mut test: impl FnMut(usize) -> bool) -> Result<Option<usize>, ()> {
    let mut found = None;
    for offset in (range.0..=range.1).step_by(8) {
        if test(offset) {
            if found.is_some() {
                return Err(());
            }
            found = Some(offset);
        }
    }
    Ok(found)
}

/// Find the component layout on `this`. Many components get a try; one with players and a
/// readable field block settles it.
fn calibrate(this: usize) {
    let Some(classes) = CLASSES.get() else {
        return;
    };
    let Ok(_guard) = CALIBRATING.try_lock() else {
        return;
    };
    if GROUP_AT.load(Ordering::Acquire) != 0 {
        return;
    }
    let attempt = CALIBRATIONS.fetch_add(1, Ordering::Relaxed);
    if attempt == MAX_CALIBRATIONS {
        capability::set(
            NAME,
            CapabilityState::Failed,
            format!(
                "layout: no AnimationComponent matched the header's field order in {MAX_CALIBRATIONS} tries"
            ),
        );
    }
    if attempt >= MAX_CALIBRATIONS {
        return;
    }
    let mut memory = Memory::default();
    if !memory.object(this, BACK_BLOCK.1 + 40) {
        return;
    }
    let back = unique(BACK_BLOCK, |offset| {
        let at = this + offset;
        let flags = |memory: &mut Memory, offset| {
            memory.read::<u8>(at + offset).is_some_and(|byte| byte <= 1)
        };
        if !flags(&mut memory, 0) || !flags(&mut memory, APPLY_AFTER) {
            return false;
        }
        if memory
            .read::<u32>(at + GROUP_AFTER)
            .is_none_or(|group| group as usize >= GROUPS)
            || memory
                .read::<i64>(at + FRAME_AFTER)
                .is_none_or(|frame| !(0..MAX_FRAME).contains(&frame))
        {
            return false;
        }
        // MSVC `std::unordered_map`: the max load factor, then the node list's head and size.
        let Some([factor, sentinel, size]) = memory.read::<[usize; 3]>(at - BONES_BEFORE) else {
            return false;
        };
        let factor = f32::from_bits(factor as u32);
        if !(factor > 0.0 && factor <= 64.0) || size > 1 << 16 || !memory.object(sentinel, 16) {
            return false;
        }
        let Some((first, count)) = memory.vector(at - CHILDREN_BEFORE, CHILD_INFO, MAX_CHILDREN)
        else {
            return false;
        };
        (0..count).all(|index| {
            memory
                .read::<usize>(first + index * CHILD_INFO + CHILD_POINTER)
                .is_some_and(|child| {
                    child == 0
                        || (memory.object(child, 8)
                            && memory.read::<usize>(child) == Some(classes.component))
                })
        })
    });
    let front = unique(PLAYERS_BLOCK, |at| {
        pointers(&mut memory, this + at, MAX_PLAYERS, |table| {
            classes.player(table)
        })
        .is_some_and(|players| !players.is_empty())
    });
    if [0, 9, 99, 999].contains(&attempt) {
        log!(
            "player_render calibration try {}: field block {back:?}, players {front:?}",
            attempt + 1
        );
    }
    let (Ok(Some(back)), Ok(Some(front))) = (back, front) else {
        return;
    };
    let Some(players) = pointers(&mut memory, this + front, MAX_PLAYERS, |table| {
        classes.player(table)
    }) else {
        return;
    };
    // The friendly name: the offset whose hash matches its text on every player.
    let name = unique(NAME_BLOCK, |at| {
        players.iter().all(|(player, _)| {
            memory.object(*player, at + 40) && hashed_string(&mut memory, player + at).is_some()
        })
    });
    let Ok(Some(name)) = name else {
        log!(
            "player_render calibration try {}: names {name:?}",
            attempt + 1
        );
        return;
    };
    let layout = Layout {
        players: front,
        initialized: back,
        name,
    };
    log!(
        "player_render layout {layout:?} from {} players",
        players.len()
    );
    let _ = LAYOUT.set(layout);
    GROUP_AT.store(back + GROUP_AFTER, Ordering::Release);
    capability::set(
        NAME,
        CapabilityState::Active,
        "found the AnimationComponent fields on live components, with names that match their hashes",
    );
}

/// Find the controller layout on `controller`, whose first state player points back at it.
fn controller_layout(
    memory: &mut Memory,
    classes: &Classes,
    controller: usize,
) -> Option<(usize, usize)> {
    if let Some(layout) = CONTROLLER.get() {
        return Some(*layout);
    }
    let state = classes.state?;
    if !memory.object(controller, STATES_BLOCK.1 + 24) {
        return None;
    }
    let mut owner = None;
    let states = unique(STATES_BLOCK, |at| {
        let Some(states) = pointers(memory, controller + at, MAX_STATES, |table| table == state)
        else {
            return false;
        };
        let Some((first, _)) = states.first() else {
            return false;
        };
        if !memory.object(*first, STATE_OWNER_BLOCK.1 + 8) {
            return false;
        }
        let back = unique(STATE_OWNER_BLOCK, |offset| {
            memory.read::<usize>(first + offset) == Some(controller)
        });
        let index = memory.read::<i32>(controller + at - STATE_INDEX_BEFORE);
        match (back, index) {
            (Ok(Some(back)), Some(index)) if (0..states.len() as i32).contains(&index) => {
                owner = Some(back);
                true
            }
            _ => false,
        }
    });
    let layout = (states.ok()??, owner?);
    log!("player_render controller layout {layout:?}");
    Some(*CONTROLLER.get_or_init(|| layout))
}

/// Find `mAnimationPlayers` on `state` whose controller owner sits at `owner`.
fn state_animations(
    memory: &mut Memory,
    classes: &Classes,
    state: usize,
    owner: usize,
) -> Option<usize> {
    if let Some(offset) = STATE_ANIMATIONS.get() {
        return Some(*offset);
    }
    let range = (owner + 8, owner + STATE_ANIMATIONS_SPAN);
    if !memory.object(state, range.1 + STATE_TIME_AFTER + 4) {
        return None;
    }
    let found = unique(range, |at| {
        let animations = pointers(memory, state + at, MAX_STATE_ANIMATIONS, |table| {
            classes.player(table) && Some(table) != classes.state
        });
        animations.is_some_and(|animations| !animations.is_empty())
            && memory
                .read::<f32>(state + at + STATE_TIME_AFTER)
                .is_some_and(|time| time.is_finite() && (0.0..MAX_STATE_TIME).contains(&time))
    })
    .ok()??;
    log!("player_render state animations at {found}");
    Some(*STATE_ANIMATIONS.get_or_init(|| found))
}

fn read_component(
    memory: &mut Memory,
    classes: &Classes,
    layout: &Layout,
    address: usize,
    depth: usize,
) -> AnimationComponent {
    let mut component = AnimationComponent {
        id: address as u64,
        group: 0,
        owner: 0,
        last_update_frame: 0,
        initialized: false,
        applied: None,
        read_age_ms: 0,
        read_us: 0,
        players: Vec::new(),
        children: Vec::new(),
        detail: None,
    };
    let at = layout.initialized;
    let fields = (|| {
        let initialized = memory.read::<u8>(address + at)?;
        let group = memory.read::<u32>(address + at + GROUP_AFTER)?;
        let owner = memory.read::<u64>(address + at + OWNER_AFTER)?;
        let frame = memory.read::<i64>(address + at + FRAME_AFTER)?;
        (initialized <= 1 && (group as usize) < GROUPS && (0..MAX_FRAME).contains(&frame))
            .then_some((initialized == 1, group, owner, frame))
    })();
    let Some((initialized, group, owner, frame)) = fields else {
        component.detail = Some("layout: the component's field block failed its checks".into());
        return component;
    };
    component.initialized = initialized;
    component.group = group;
    component.owner = owner;
    component.last_update_frame = frame;
    match pointers(memory, address + layout.players, MAX_PLAYERS, |_| true) {
        Some(players) => {
            component.players = players
                .into_iter()
                .map(|(player, table)| read_player(memory, classes, layout, player, table))
                .collect();
        }
        None => component.detail = Some("the player list is not readable".into()),
    }
    if depth < MAX_DEPTH {
        component.children = children(memory, classes, layout, address)
            .into_iter()
            .map(|child| read_component(memory, classes, layout, child, depth + 1))
            .collect();
    }
    component
}

fn read_player(
    memory: &mut Memory,
    classes: &Classes,
    layout: &Layout,
    player: usize,
    table: usize,
) -> AnimationPlayer {
    let name = |memory: &mut Memory, target: usize| {
        if memory.object(target, layout.name + 40) {
            hashed_string(memory, target + layout.name)
        } else {
            None
        }
    };
    let mut read = AnimationPlayer {
        kind: if Some(table) == classes.controller {
            "controller"
        } else if Some(table) == classes.skeletal {
            "animation"
        } else {
            "unknown"
        }
        .into(),
        name: name(memory, player).unwrap_or_default(),
        state: None,
        state_index: None,
        last_state_index: None,
        state_time: None,
        animations: Vec::new(),
    };
    if Some(table) != classes.controller {
        return read;
    }
    let Some((states_at, owner)) = controller_layout(memory, classes, player) else {
        return read;
    };
    let Some(states) = pointers(memory, player + states_at, MAX_STATES, |table| {
        Some(table) == classes.state
    }) else {
        return read;
    };
    // The code also accepts -1 as an unset index.
    let in_range = |index: &i32| (-1..states.len() as i32).contains(index);
    read.state_index = memory
        .read::<i32>(player + states_at - STATE_INDEX_BEFORE)
        .filter(in_range);
    read.last_state_index = memory
        .read::<i32>(player + states_at - LAST_STATE_INDEX_BEFORE)
        .filter(in_range);
    let Some(index) = read.state_index.filter(|index| *index >= 0) else {
        return read;
    };
    let (state, _) = states[index as usize];
    if !memory.object(state, owner + 8) || memory.read::<usize>(state + owner) != Some(player) {
        return read;
    }
    read.state = name(memory, state);
    let Some(animations_at) = state_animations(memory, classes, state, owner) else {
        return read;
    };
    read.state_time = memory
        .read::<f32>(state + animations_at + STATE_TIME_AFTER)
        .filter(|time| time.is_finite() && (0.0..MAX_STATE_TIME).contains(time));
    if let Some(animations) = pointers(
        memory,
        state + animations_at,
        MAX_STATE_ANIMATIONS,
        |table| classes.player(table),
    ) {
        read.animations = animations
            .into_iter()
            .filter_map(|(animation, _)| name(memory, animation))
            .collect();
    }
    read
}

/// Ask every tracked component for a read on its next evaluation, wait up to `wait`, and
/// return the local player's components, each with its attachables. `actor`, the local
/// player's unique id, replaces the one learned from HUD and first-person components.
pub fn read(wait: Option<u32>, actor: Option<i64>) -> PlayerRender {
    if let Some(actor) = actor {
        LOCAL_ACTOR.store(actor as u64 & ACTOR_BITS, Ordering::Relaxed);
    }
    let wait = wait
        .map_or(DEFAULT_WAIT, |ms| Duration::from_millis(ms.into()))
        .min(MAX_WAIT);
    let generation = WANTED.fetch_add(1, Ordering::AcqRel) + 1;
    let deadline = Instant::now() + wait;
    let Ok(mut entries) = ENTRIES.lock() else {
        return empty("the component table is poisoned");
    };
    loop {
        let now = Instant::now();
        // Nothing tracked yet: the first evaluations may still come within the wait.
        let answered = !entries.is_empty()
            && entries.iter().all(|(_, entry)| {
                entry
                    .read
                    .as_ref()
                    .is_some_and(|(at, ..)| *at >= generation)
                    || now.duration_since(entry.last_seen) > wait
            });
        if answered || now >= deadline {
            break;
        }
        entries = match READ_DONE.wait_timeout(entries, deadline - now) {
            Ok((entries, _)) => entries,
            Err(_) => return empty("the component table is poisoned"),
        };
    }
    let now = Instant::now();
    let stats: HashMap<u64, Option<Visits>> = entries
        .iter()
        .map(|(id, entry)| (*id as u64, entry.apply.visits(now)))
        .collect();
    let mut components: Vec<AnimationComponent> = entries
        .iter()
        .filter(|(_, entry)| !entry.child)
        .filter_map(|(_, entry)| {
            let (_, at, component) = entry.read.as_ref()?;
            let mut component = component.clone();
            annotate(
                &mut component,
                &stats,
                now.duration_since(*at).as_millis() as u64,
            );
            Some(component)
        })
        .collect();
    drop(entries);
    let nested: Vec<u64> = components
        .iter()
        .flat_map(|component| component.children.iter().map(|child| child.id))
        .collect();
    components.retain(|component| !nested.contains(&component.id));
    components.sort_by_key(|component| {
        (
            component.group != FIRST_PERSON,
            component.group,
            component.id,
        )
    });
    let detail = match (LAYOUT.get(), LOCAL_ACTOR.load(Ordering::Relaxed), components.is_empty()) {
        (None, ..) => Some("the layout is not known yet: no animated actor has been drawn".into()),
        (Some(_), 0, _) => Some(
            "the local player is not known yet: no HUD or first-person animation component has been evaluated since this layer loaded (join a world)"
                .into(),
        ),
        (Some(_), _, true) => Some(
            "no animation component of the local player has been evaluated within wait_ms".into(),
        ),
        _ => None,
    };
    PlayerRender {
        frame: FRAME.load(Ordering::Relaxed),
        components,
        visits_by_group: VISITS
            .iter()
            .map(|visits| visits.load(Ordering::Relaxed))
            .collect(),
        detail,
    }
}

fn annotate(component: &mut AnimationComponent, stats: &HashMap<u64, Option<Visits>>, age_ms: u64) {
    component.read_age_ms = age_ms;
    if let Some(applied) = stats.get(&component.id) {
        component.applied = applied.clone();
    }
    for child in &mut component.children {
        annotate(child, stats, age_ms);
    }
}

fn empty(detail: &str) -> PlayerRender {
    PlayerRender {
        frame: 0,
        components: Vec::new(),
        visits_by_group: Vec::new(),
        detail: Some(detail.into()),
    }
}
