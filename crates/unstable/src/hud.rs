//! HUD text: toasts as the game queues them, and the title, subtitle, action bar, popup,
//! tip and chat lines the HUD draws from `GuiData`.
//!
//! Toasts: `ToastManager` (primary vtable of 5 slots: its own destructor in slot 0, since
//! `IToastManager` declares none, then `IToastManager`'s `pushToast`, `pushPersistentToast`,
//! `removePersistentToast`, `isEditorModeEnabled`) gets its slots 1 and 2 hooked. Each hook
//! copies the `ToastMessage&&` it is given before the game moves from it: `mType` at +16,
//! then `mTitle`, `mSubtitle` and `mMessageId` strings at +24, +56 and +88. Layout
//! reference: LeviLamina's `src-client/mc/client/social/{IToastManager,ToastManager,ToastMessage}.h`
//! and `.../social/ToastMessageType.h`.
//!
//! `GuiData` (`src-client/mc/client/gui/GuiData.h`, with `GuiMessage.h` and
//! `TitleMessage.h` beside it) has no per-frame virtual, so the capability finds it from
//! the `ClientInstance` the camera capability names: it scans that object for a pointer
//! whose target carries both `GuiData` vtables (the primary one and the
//! `AppPlatformListener` subobject's at +8) and whose `mClient` points back at the client.
//! The fields after `mClient` follow from the header's member sizes, relative to it:
//! `mLastPopupText` +40, `mTickCount` +168, `mGuiMessages` +184 (272-byte `GuiMessage`s),
//! `mTitleMessage` +696 (title +0, subtitle +72, fade in, stay, fade out +144, action bar
//! +160) and `mTipMessage` +1048. Reads run on the game thread from the `screens` hook:
//! the texts every few frames, to see when they change, and the chat lines on request.

use std::{
    collections::VecDeque,
    ffi::c_void,
    sync::{
        Condvar, LazyLock, Mutex, OnceLock,
        atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use endpoint::log;
use protocol::unstable::{CapabilityState, ChatLine, HudState, HudText, Toast};

use crate::{camera, capability, memory::Memory, vtable::Resolver};

const NAME: &str = "hud_text";

const TOAST_CLASS: &str = "ToastManager";
const TOAST_SLOTS: usize = 5;
const PUSH_TOAST: usize = 1;
const PUSH_PERSISTENT_TOAST: usize = 2;
const TOAST_TYPE: usize = 16;
const TOAST_TITLE: usize = 24;
const TOAST_SUBTITLE: usize = 56;
const TOAST_MESSAGE_ID: usize = 88;
/// `ToastMessageType` has 23 values; leave room for a few more.
const MAX_TOAST_TYPE: u32 = 64;
const MAX_TOASTS: usize = 32;

const GUI_CLASS: &str = "GuiData";
const GUI_PRIMARY_SLOTS: usize = 2;
const GUI_LISTENER_OFFSET: u32 = 8;
const GUI_LISTENER_SLOTS: usize = 21;

/// Where to look for `GuiData::mClient`, and for the `GuiData` pointer in the client.
const GUI_CLIENT_SCAN: std::ops::Range<usize> = 64..512;
const CLIENT_SCAN: usize = 0x4000;
/// Offsets from `GuiData::mClient`.
const POPUP: usize = 40;
const TICK_COUNT: usize = 168;
const MESSAGES: usize = 184;
const TITLE: usize = 696;
const TIP: usize = 1048;
/// Within `TitleMessage`.
const SUBTITLE: usize = 72;
const TITLE_TICKS: usize = 144;
const ACTIONBAR: usize = 160;
/// `GuiMessage`: size and fields.
const MESSAGE_SIZE: usize = 272;
const MESSAGE_TEXT: usize = 8;
const MESSAGE_USERNAME: usize = 112;
const MESSAGE_FULL: usize = 144;
const MESSAGE_DURATION: usize = 252;
const MESSAGE_SEEN: usize = 256;
/// `GuiMessage::MessageType` runs 0 to 9.
const MAX_MESSAGE_TYPE: u32 = 9;
const MAX_MESSAGES: usize = 4096;
const MAX_TEXT: usize = 1024;
const DEFAULT_CHAT: usize = 20;
const MAX_CHAT: usize = 100;
/// A fade time above this many ticks is not plausible.
const MAX_TITLE_TICKS: i32 = 1_000_000;

/// `SceneStack::update` calls, from every stack, between reads of the texts, and between
/// searches for `GuiData`. Each stack updates at least once per frame, so these are at most
/// that many frames.
const POLL_FRAMES: u64 = 2;
const SEARCH_FRAMES: u64 = 300;
/// How long a request waits for the game thread.
const WAIT: Duration = Duration::from_secs(1);

const CAMERA_API: &str = "VanillaCameraAPI";
const CAMERA_API_SLOTS: usize = 8;
const TRY_GET_DAMAGE_BOB: usize = 1;
/// A damage read older than this does not tell whether the player is dead now.
const DEATH_FRESH_MS: u64 = 1000;
/// More ticks since death than this is not plausible.
const MAX_DEATH_TICKS: f32 = 1.0e7;

type PushToast = unsafe extern "system" fn(*mut c_void, *mut c_void);
type TryGetDamageBob =
    unsafe extern "system" fn(*mut c_void, *mut DamageBob, *mut c_void, f32) -> *mut DamageBob;

/// `std::optional<IVanillaCameraAPI::DamageBobParameters>`: four floats, then the flag.
#[repr(C)]
struct DamageBob {
    ticks_since_death: f32,
    damage_yaw: f32,
    damage_countdown: f32,
    strength: f32,
    engaged: u8,
}

static ORIGINAL_PUSH: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_PERSISTENT: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_DAMAGE_BOB: AtomicUsize = AtomicUsize::new(0);
static TOAST_SEQ: AtomicU64 = AtomicU64::new(0);
static TOASTS: Mutex<VecDeque<Toast>> = Mutex::new(VecDeque::new());
/// The last ticks since death the camera read, and when (milliseconds since [`START`],
/// zero for never).
static DEATH_TICKS: AtomicU32 = AtomicU32::new(0);
static DEATH_AT: AtomicU64 = AtomicU64::new(0);
static START: LazyLock<Instant> = LazyLock::new(Instant::now);

fn since_start() -> u64 {
    START.elapsed().as_millis() as u64 + 1
}

/// `VanillaCameraAPI::tryGetDamageBobParameters(WeakRef<EntityContext>, float)`. LeviLamina's
/// `IVanillaCameraAPI.h` names the first field `mFractionalFramesSinceDeath`; the code treats a
/// value above zero as dead.
unsafe extern "system" fn damage_bob_hook(
    this: *mut c_void,
    out: *mut DamageBob,
    actor: *mut c_void,
    alpha: f32,
) -> *mut DamageBob {
    // SAFETY: stored from the slot this hook replaced.
    let original: TryGetDamageBob =
        unsafe { std::mem::transmute(ORIGINAL_DAMAGE_BOB.load(Ordering::Acquire)) };
    let result = unsafe { original(this, out, actor, alpha) };
    if !result.is_null() {
        // SAFETY: the game just filled the optional it was given and returned it.
        let bob = unsafe { std::ptr::read(result) };
        if bob.engaged == 1
            && bob.ticks_since_death.is_finite()
            && (0.0..MAX_DEATH_TICKS).contains(&bob.ticks_since_death)
        {
            DEATH_TICKS.store(bob.ticks_since_death.to_bits(), Ordering::Release);
            DEATH_AT.store(since_start(), Ordering::Release);
        }
    }
    result
}

/// The last ticks since the followed player died, when read within the last second.
fn death_ticks() -> Option<f32> {
    let at = DEATH_AT.load(Ordering::Acquire);
    (at != 0 && since_start().saturating_sub(at) <= DEATH_FRESH_MS)
        .then(|| f32::from_bits(DEATH_TICKS.load(Ordering::Acquire)))
}

struct Tables {
    primary: usize,
    listener: usize,
}

static TABLES: OnceLock<Tables> = OnceLock::new();

/// What each part last found; the capability state follows from both.
#[derive(Default)]
struct Parts {
    toasts: Option<Result<(), String>>,
    texts: Option<Result<(), String>>,
}

static PARTS: Mutex<Parts> = Mutex::new(Parts {
    toasts: None,
    texts: None,
});

/// `GuiData` and its `mClient` field, once found.
#[derive(Clone, Copy)]
struct Gui {
    address: usize,
    client: usize,
}

#[derive(Default)]
struct Texts {
    gui: Option<Gui>,
    last_search: Option<u64>,
    title: Option<HudText>,
    subtitle: Option<HudText>,
    actionbar: Option<HudText>,
    popup: Option<HudText>,
    tip: Option<HudText>,
    title_ticks: Option<[i32; 3]>,
    chat: Vec<ChatLine>,
    /// The request generation the chat lines answer.
    answered: u64,
    detail: Option<String>,
}

static FRAME: AtomicU64 = AtomicU64::new(0);
static TEXTS: Mutex<Option<Texts>> = Mutex::new(None);
static TEXTS_DONE: Condvar = Condvar::new();
/// Generation and chat limit of the latest request.
static WANTED: AtomicU64 = AtomicU64::new(0);
static CHAT_LIMIT: AtomicUsize = AtomicUsize::new(DEFAULT_CHAT);

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// Record one part's result and set the capability from both.
fn report(update: impl FnOnce(&mut Parts)) {
    let mut parts = PARTS.lock().unwrap_or_else(|poison| poison.into_inner());
    update(&mut parts);
    let describe = |part: &Option<Result<(), String>>, ok: &str, waiting: &str| match part {
        Some(Ok(())) => ok.to_owned(),
        Some(Err(reason)) => reason.clone(),
        None => waiting.to_owned(),
    };
    let detail = format!(
        "toasts: {}; texts: {}",
        describe(&parts.toasts, "pushToast confirmed", "waiting for a toast"),
        describe(
            &parts.texts,
            "GuiData found and read",
            "waiting for GuiData"
        ),
    );
    let state = match (&parts.toasts, &parts.texts) {
        (Some(Ok(())), _) | (_, Some(Ok(()))) => CapabilityState::Active,
        (Some(Err(_)), _) | (_, Some(Err(_))) => CapabilityState::Failed,
        _ => CapabilityState::Pending,
    };
    capability::set(NAME, state, detail);
}

/// Copy the toast at `message` into the ring; false when it fails its checks.
fn record_toast(message: usize, persistent: bool) -> bool {
    let mut memory = Memory::default();
    if !memory.object(message, TOAST_MESSAGE_ID + 32) {
        return false;
    }
    let Some(kind) = memory
        .read::<u32>(message + TOAST_TYPE)
        .filter(|kind| *kind < MAX_TOAST_TYPE)
    else {
        return false;
    };
    let (Some(title), Some(subtitle), Some(message_id)) = (
        memory.string(message + TOAST_TITLE, MAX_TEXT),
        memory.string(message + TOAST_SUBTITLE, MAX_TEXT),
        memory.string(message + TOAST_MESSAGE_ID, MAX_TEXT),
    ) else {
        return false;
    };
    let toast = Toast {
        seq: TOAST_SEQ.fetch_add(1, Ordering::AcqRel) + 1,
        at_ms: unix_ms(),
        kind,
        title,
        subtitle,
        message_id,
        persistent,
    };
    let mut toasts = TOASTS.lock().unwrap_or_else(|poison| poison.into_inner());
    toasts.push_back(toast);
    if toasts.len() > MAX_TOASTS {
        toasts.pop_front();
    }
    true
}

fn toast_seen(ok: bool) {
    let result = if ok {
        Ok(())
    } else {
        Err("layout: a pushed ToastMessage failed its checks".to_owned())
    };
    let changed = PARTS
        .lock()
        .map(|parts| parts.toasts.as_ref() != Some(&result))
        .unwrap_or(false);
    if changed {
        report(|parts| parts.toasts = Some(result));
    }
}

unsafe extern "system" fn push_hook(this: *mut c_void, message: *mut c_void) {
    toast_seen(record_toast(message as usize, false));
    // SAFETY: stored from the slot this hook replaced.
    let original: PushToast = unsafe { std::mem::transmute(ORIGINAL_PUSH.load(Ordering::Acquire)) };
    unsafe { original(this, message) };
}

unsafe extern "system" fn persistent_hook(this: *mut c_void, message: *mut c_void) {
    toast_seen(record_toast(message as usize, true));
    // SAFETY: stored from the slot this hook replaced.
    let original: PushToast =
        unsafe { std::mem::transmute(ORIGINAL_PERSISTENT.load(Ordering::Acquire)) };
    unsafe { original(this, message) };
}

pub fn install(resolver: &Resolver<'_>, hooked: &Result<(), String>) {
    let mut found = Vec::new();
    let toasts = resolver
        .table(TOAST_CLASS, 0, TOAST_SLOTS)
        .and_then(|table| {
            found.push((TOAST_CLASS, table.found));
            table.hook(PUSH_TOAST, push_hook as PushToast as usize, &ORIGINAL_PUSH)?;
            table.hook(
                PUSH_PERSISTENT_TOAST,
                persistent_hook as PushToast as usize,
                &ORIGINAL_PERSISTENT,
            )
        });
    LazyLock::force(&START);
    if let Err(reason) = resolver
        .table(CAMERA_API, 0, CAMERA_API_SLOTS)
        .and_then(|table| {
            found.push((CAMERA_API, table.found));
            table.hook(
                TRY_GET_DAMAGE_BOB,
                damage_bob_hook as TryGetDamageBob as usize,
                &ORIGINAL_DAMAGE_BOB,
            )
        })
    {
        log!("hud_text: no death state: {reason}");
    }
    let texts = hooked
        .clone()
        .map_err(|reason| format!("needs the SceneStack::update hook: {reason}"))
        .and_then(|()| {
            let primary = resolver.table(GUI_CLASS, 0, GUI_PRIMARY_SLOTS)?;
            found.push((GUI_CLASS, primary.found));
            let primary = primary.address;
            let listener = resolver
                .table(GUI_CLASS, GUI_LISTENER_OFFSET, GUI_LISTENER_SLOTS)?
                .address;
            TABLES
                .set(Tables { primary, listener })
                .map_err(|_| "installed twice".to_owned())
        });
    if let Ok(mut slot) = TEXTS.lock() {
        *slot = Some(Texts::default());
    }
    capability::found(NAME, &found);
    match (&toasts, &texts) {
        (Err(toasts), Err(texts)) => capability::set(
            NAME,
            CapabilityState::Unavailable,
            format!("toasts: {toasts}; texts: {texts}"),
        ),
        _ => report(|parts| {
            parts.toasts = toasts.err().map(Err);
            parts.texts = texts.err().map(Err);
        }),
    }
}

/// Find `GuiData` from the camera's `ClientInstance`.
fn find_gui(memory: &mut Memory, tables: &Tables) -> Result<Gui, String> {
    let client = camera::client();
    if client == 0 {
        return Err("waiting for the camera capability to name the ClientInstance".into());
    }
    for offset in (0..CLIENT_SCAN).step_by(8) {
        let Some(candidate) = memory.read::<usize>(client + offset) else {
            break;
        };
        if candidate == 0
            || !memory.object(candidate, GUI_CLIENT_SCAN.end)
            || memory.read::<usize>(candidate) != Some(tables.primary)
            || memory.read::<usize>(candidate + 8) != Some(tables.listener)
        {
            continue;
        }
        let back = GUI_CLIENT_SCAN
            .step_by(8)
            .find(|at| memory.read::<usize>(candidate + at) == Some(client));
        return match back {
            Some(at) => Ok(Gui {
                address: candidate,
                client: candidate + at,
            }),
            None => Err("layout: GuiData::mClient does not point at the ClientInstance".into()),
        };
    }
    Err(format!(
        "no GuiData within {CLIENT_SCAN:#x} bytes of the ClientInstance"
    ))
}

fn text(memory: &mut Memory, address: usize) -> Result<String, String> {
    memory
        .string(address, MAX_TEXT)
        .ok_or_else(|| format!("layout: no string at GuiData+{address:#x}"))
}

/// Keep `slot`'s change time unless the text changed.
fn track(slot: &mut Option<HudText>, text: String, first: bool) {
    if slot.as_ref().is_some_and(|known| known.text == text) {
        return;
    }
    *slot = Some(HudText {
        changed_ms: (!first).then(unix_ms),
        text,
    });
}

/// Read the texts, and the chat lines when `chat` asks for some.
fn read_texts(texts: &mut Texts, gui: Gui, chat: Option<usize>) -> Result<(), String> {
    let mut memory = Memory::default();
    let tables = TABLES.get().ok_or("not installed")?;
    if !memory.object(gui.address, 16)
        || memory.read::<usize>(gui.address) != Some(tables.primary)
        || memory.read::<usize>(gui.client) != Some(camera::client())
    {
        return Err("GuiData moved".into());
    }
    let base = gui.client;
    if memory
        .read::<i32>(base + TICK_COUNT)
        .is_none_or(|ticks| ticks < 0)
    {
        return Err("layout: GuiData::mTickCount is not plausible".into());
    }
    let title_ticks = memory
        .read::<[i32; 3]>(base + TITLE + TITLE_TICKS)
        .filter(|ticks| {
            ticks
                .iter()
                .all(|tick| (-1..MAX_TITLE_TICKS).contains(tick))
        })
        .ok_or("layout: TitleMessage fade times are not plausible")?;
    let title = text(&mut memory, base + TITLE)?;
    let subtitle = text(&mut memory, base + TITLE + SUBTITLE)?;
    let actionbar = text(&mut memory, base + TITLE + ACTIONBAR)?;
    let popup = text(&mut memory, base + POPUP)?;
    let tip = text(&mut memory, base + TIP)?;
    let (first, count) = memory
        .vector(base + MESSAGES, MESSAGE_SIZE, MAX_MESSAGES)
        .ok_or("layout: GuiData::mGuiMessages is not a vector of GuiMessage")?;
    let fresh = texts.title.is_none();
    track(&mut texts.title, title, fresh);
    track(&mut texts.subtitle, subtitle, fresh);
    track(&mut texts.actionbar, actionbar, fresh);
    track(&mut texts.popup, popup, fresh);
    track(&mut texts.tip, tip, fresh);
    texts.title_ticks = Some(title_ticks);
    let Some(limit) = chat else {
        return Ok(());
    };
    let mut lines = Vec::new();
    for index in count.saturating_sub(limit)..count {
        let message = first + index * MESSAGE_SIZE;
        let kind = memory
            .read::<u32>(message)
            .filter(|kind| *kind <= MAX_MESSAGE_TYPE)
            .ok_or("layout: a GuiMessage has no valid type")?;
        let duration = memory
            .read::<f32>(message + MESSAGE_DURATION)
            .filter(|duration| duration.is_finite())
            .ok_or("layout: a GuiMessage has no valid duration")?;
        lines.push(ChatLine {
            kind,
            username: text(&mut memory, message + MESSAGE_USERNAME)?,
            message: text(&mut memory, message + MESSAGE_TEXT)?,
            full: text(&mut memory, message + MESSAGE_FULL)?,
            duration,
            seen: memory.read::<u8>(message + MESSAGE_SEEN) == Some(1),
        });
    }
    texts.chat = lines;
    Ok(())
}

/// Called by the `screens` hook on the game thread after each `SceneStack::update`.
pub fn frame() {
    let frame = FRAME.fetch_add(1, Ordering::AcqRel);
    let wanted = WANTED.load(Ordering::Acquire);
    let Some(tables) = TABLES.get() else {
        return;
    };
    let Ok(mut slot) = TEXTS.lock() else {
        return;
    };
    let Some(texts) = slot.as_mut() else {
        return;
    };
    let requested = wanted > texts.answered;
    if !requested && !frame.is_multiple_of(POLL_FRAMES) {
        return;
    }
    let gui = match texts.gui {
        Some(gui) => Ok(gui),
        None if !requested
            && texts
                .last_search
                .is_some_and(|last| frame < last + SEARCH_FRAMES) =>
        {
            return;
        }
        None => {
            texts.last_search = Some(frame);
            find_gui(&mut Memory::default(), tables)
        }
    };
    let chat = requested.then(|| CHAT_LIMIT.load(Ordering::Acquire).min(MAX_CHAT));
    let result = gui.and_then(|gui| {
        texts.gui = Some(gui);
        read_texts(texts, gui, chat)
    });
    if requested {
        texts.answered = wanted;
    }
    let previous = texts.detail.clone();
    match &result {
        Ok(()) => texts.detail = None,
        Err(reason) => {
            texts.detail = Some(reason.clone());
            if reason == "GuiData moved" {
                texts.gui = None;
                texts.last_search = None;
            }
        }
    }
    let changed = previous != texts.detail || requested;
    drop(slot);
    if requested {
        TEXTS_DONE.notify_all();
    }
    if changed {
        match result {
            Ok(()) => report(|parts| parts.texts = Some(Ok(()))),
            Err(reason) if reason.starts_with("layout") => {
                report(|parts| parts.texts = Some(Err(reason)));
            }
            Err(_) => {}
        }
    }
}

/// The toasts so far and the texts, read on the next frame when one comes within a second.
pub fn read(chat_limit: Option<u32>) -> HudState {
    CHAT_LIMIT.store(
        chat_limit.map_or(DEFAULT_CHAT, |limit| limit as usize),
        Ordering::Release,
    );
    let generation = WANTED.fetch_add(1, Ordering::AcqRel) + 1;
    let toasts: Vec<Toast> = TOASTS
        .lock()
        .map(|toasts| toasts.iter().cloned().collect())
        .unwrap_or_default();
    let deadline = Instant::now() + WAIT;
    let Ok(mut slot) = TEXTS.lock() else {
        return empty(toasts, "the HUD state is unavailable");
    };
    loop {
        let answered = slot
            .as_ref()
            .is_some_and(|texts| texts.answered >= generation);
        let now = Instant::now();
        if answered || now >= deadline {
            break;
        }
        slot = match TEXTS_DONE.wait_timeout(slot, deadline - now) {
            Ok((slot, _)) => slot,
            Err(_) => return empty(toasts, "the HUD state is unavailable"),
        };
    }
    let Some(texts) = slot.as_ref() else {
        return empty(toasts, "not installed");
    };
    HudState {
        toasts,
        title: texts.title.clone(),
        subtitle: texts.subtitle.clone(),
        actionbar: texts.actionbar.clone(),
        title_ticks: texts.title_ticks,
        popup: texts.popup.clone(),
        tip: texts.tip.clone(),
        chat: texts.chat.clone(),
        dead: death_ticks().map(|ticks| ticks > 0.0),
        death_ticks: death_ticks(),
        detail: texts.detail.clone(),
        fresh: texts.answered >= generation,
    }
}

fn empty(toasts: Vec<Toast>, detail: &str) -> HudState {
    HudState {
        toasts,
        title: None,
        subtitle: None,
        actionbar: None,
        title_ticks: None,
        popup: None,
        tip: None,
        chat: Vec::new(),
        dead: death_ticks().map(|ticks| ticks > 0.0),
        death_ticks: death_ticks(),
        detail: Some(detail.into()),
        fresh: false,
    }
}
