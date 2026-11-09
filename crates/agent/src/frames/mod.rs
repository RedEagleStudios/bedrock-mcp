//! Copies of the frames the game presents, for recordings and captures.
//!
//! The game draws with Direct3D 12 and presents through DXGI, on Windows and under
//! vkd3d-proton alike. The agent creates a throwaway device, queue and swap chain, and patches
//! `Present`, `Present1` and `ExecuteCommandLists` in their vtables, which the game's own
//! objects share. Nothing is hooked until a client first asks for frames.
//!
//! While frames are wanted, each due present queues a GPU copy of the back buffer into a
//! readback buffer on the presenting queue, right before the present. A worker thread waits
//! for the copy and writes it to the shared-memory file ([`shm`]). The game thread never waits:
//! a frame is skipped and counted when no readback buffer is free. Every present's time is
//! recorded, captured or not, so frame pacing is known exactly.

mod copier;
mod shm;

use std::{
    ffi::c_void,
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel},
    },
    time::Duration,
};

use endpoint::{log, wide};
use protocol::frames::{PRESENT_RING, state};
use windows::{
    Win32::{
        Foundation::HWND,
        Graphics::{
            Direct3D::D3D_FEATURE_LEVEL_11_0,
            Direct3D12::{
                D3D12_COMMAND_LIST_TYPE_DIRECT, D3D12_COMMAND_QUEUE_DESC, D3D12_RANGE,
                D3D12CreateDevice, ID3D12CommandQueue, ID3D12Device,
            },
            Dxgi::{
                Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
                CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, DXGI_PRESENT_TEST,
                DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_DISCARD,
                DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIFactory2, IDXGISwapChain,
            },
        },
    },
    core::Interface,
};
use windows_sys::Win32::{
    System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
    UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, WS_POPUP},
};

use crate::gameinput::patch;
use copier::{Copier, Queued, back_buffer, present_queue};
use shm::Shm;

const SLOT_PRESENT: usize = 8;
const SLOT_PRESENT1: usize = 22;
const SLOT_EXECUTE: usize = 10;

type Present = unsafe extern "system" fn(*mut c_void, u32, u32) -> i32;
type Present1 = unsafe extern "system" fn(*mut c_void, u32, u32, *const c_void) -> i32;
type Execute = unsafe extern "system" fn(*mut c_void, u32, *const *mut c_void);

static ORIGINAL_PRESENT: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_PRESENT1: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_EXECUTE: AtomicUsize = AtomicUsize::new(0);
/// Hooks are installed the first time frames are wanted.
static INSTALLED: LazyLock<Result<(), String>> = LazyLock::new(install);

static ACTIVE: AtomicBool = AtomicBool::new(false);
static PRESENTS: AtomicU64 = AtomicU64::new(0);
static PRESENT_TIMES: [AtomicU64; PRESENT_RING] = [const { AtomicU64::new(0) }; PRESENT_RING];
static DROPPED: AtomicU64 = AtomicU64::new(0);
/// Nanoseconds between presents while the game is capped; 0 when it is not.
static PACE_INTERVAL_NS: AtomicU64 = AtomicU64::new(0);
/// When the next capped present may go; only the presenting thread uses it.
static PACE_NEXT_NS: AtomicU64 = AtomicU64::new(0);
/// The pacer spins instead of sleeping for this long before a present.
const PACE_SPIN_NS: u64 = 1_500_000;
/// A direct queue seen in `ExecuteCommandLists`, for swap chains that do not report theirs.
static DIRECT_QUEUE: Mutex<Option<ID3D12CommandQueue>> = Mutex::new(None);
static DIRECT_QUEUE_RAW: AtomicUsize = AtomicUsize::new(0);
static STATE: Mutex<Option<Capture>> = Mutex::new(None);

/// Present-thread state of a running capture.
struct Capture {
    sender: SyncSender<Pending>,
    interval_ns: u64,
    next_due_ns: u64,
    copier: Option<Copier>,
    failed: bool,
}

// SAFETY: the copier's D3D12 objects are free-threaded; the capture is used under a mutex.
unsafe impl Send for Capture {}

struct Pending {
    queued: Queued,
    present: u64,
    present_ns: u64,
}

fn now_ns() -> u64 {
    let (mut counter, mut frequency) = (0i64, 0i64);
    unsafe {
        QueryPerformanceCounter(&mut counter);
        QueryPerformanceFrequency(&mut frequency);
    }
    (counter as u128 * 1_000_000_000 / frequency.max(1) as u128) as u64
}

/// Hold the presenting thread until the next slot of the capture's game frame cap. A present
/// more than a slot late (a stall, or the first one) restarts the schedule from now.
fn pace() {
    let interval = PACE_INTERVAL_NS.load(Ordering::Relaxed);
    if interval == 0 {
        return;
    }
    let now = now_ns();
    let mut target = PACE_NEXT_NS.load(Ordering::Relaxed);
    if target == 0 || now > target + interval {
        target = now;
    }
    loop {
        let remaining = target.saturating_sub(now_ns());
        if remaining == 0 {
            break;
        }
        // Sleeps overshoot; spin through the last stretch.
        if remaining > PACE_SPIN_NS {
            std::thread::sleep(Duration::from_nanos(remaining - PACE_SPIN_NS));
        } else {
            std::hint::spin_loop();
        }
    }
    PACE_NEXT_NS.store(target + interval, Ordering::Relaxed);
}

/// Record the present and copy the frame if one is due.
fn before_present(this: *mut c_void, flags: u32) {
    if !ACTIVE.load(Ordering::Acquire) || flags & DXGI_PRESENT_TEST.0 != 0 {
        return;
    }
    pace();
    let now = now_ns();
    // Only the presenting thread writes the ring.
    let present = PRESENTS.load(Ordering::Relaxed);
    PRESENT_TIMES[present as usize % PRESENT_RING].store(now, Ordering::Release);
    PRESENTS.store(present + 1, Ordering::Release);
    let Ok(mut guard) = STATE.try_lock() else {
        return;
    };
    let Some(capture) = guard.as_mut() else {
        return;
    };
    if capture.failed || now < capture.next_due_ns {
        return;
    }
    capture.next_due_ns = if capture.interval_ns == 0 {
        0
    } else if capture.next_due_ns + capture.interval_ns > now {
        capture.next_due_ns + capture.interval_ns
    } else {
        now + capture.interval_ns
    };
    // SAFETY: DXGI calls Present on a live swap chain.
    let Some(swapchain) = (unsafe { IDXGISwapChain::from_raw_borrowed(&this) }) else {
        return;
    };
    match copy_frame(capture, swapchain, this as usize) {
        Ok(Copied::Queued(queued)) => {
            let pending = Pending {
                queued,
                present,
                present_ns: now,
            };
            if let Err(TrySendError::Full(pending) | TrySendError::Disconnected(pending)) =
                capture.sender.try_send(pending)
            {
                pending.queued.readback.busy[pending.queued.buffer].store(false, Ordering::Release);
                DROPPED.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(Copied::Busy) => {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
        // The game submits work before its next present; the queue is known then.
        Ok(Copied::NoQueue) => {}
        Err(error) => {
            log!("frame capture stopped: {error}");
            capture.failed = true;
        }
    }
}

enum Copied {
    Queued(Queued),
    /// Every readback buffer is still in use.
    Busy,
    /// The presenting queue is not known yet.
    NoQueue,
}

fn copy_frame(
    capture: &mut Capture,
    swapchain: &IDXGISwapChain,
    raw: usize,
) -> Result<Copied, String> {
    let buffer = back_buffer(swapchain)?;
    let desc = unsafe { buffer.GetDesc() };
    if !capture
        .copier
        .as_ref()
        .is_some_and(|copier| copier.fits(raw, &desc))
    {
        capture.copier = None;
        let (queue, source) = match present_queue(swapchain) {
            Some(queue) => (queue, "the swap chain"),
            None => match DIRECT_QUEUE.lock().ok().and_then(|queue| queue.clone()) {
                Some(queue) => (queue, "ExecuteCommandLists"),
                None => return Ok(Copied::NoQueue),
            },
        };
        let copier = Copier::new(raw, queue, &buffer)?;
        log!(
            "copying {}x{} frames (format {}) on the queue from {source}",
            copier.readback.shape.width,
            copier.readback.shape.height,
            copier.readback.shape.format
        );
        capture.copier = Some(copier);
    }
    Ok(
        match capture.copier.as_mut().expect("set above").copy(&buffer)? {
            Some(queued) => Copied::Queued(queued),
            None => Copied::Busy,
        },
    )
}

unsafe extern "system" fn present_hook(this: *mut c_void, sync: u32, flags: u32) -> i32 {
    before_present(this, flags);
    // SAFETY: stored from the slot this hook replaced.
    let original: Present =
        unsafe { std::mem::transmute(ORIGINAL_PRESENT.load(Ordering::Acquire)) };
    unsafe { original(this, sync, flags) }
}

unsafe extern "system" fn present1_hook(
    this: *mut c_void,
    sync: u32,
    flags: u32,
    parameters: *const c_void,
) -> i32 {
    before_present(this, flags);
    // SAFETY: stored from the slot this hook replaced.
    let original: Present1 =
        unsafe { std::mem::transmute(ORIGINAL_PRESENT1.load(Ordering::Acquire)) };
    unsafe { original(this, sync, flags, parameters) }
}

unsafe extern "system" fn execute_hook(this: *mut c_void, count: u32, lists: *const *mut c_void) {
    if ACTIVE.load(Ordering::Relaxed) && DIRECT_QUEUE_RAW.load(Ordering::Relaxed) != this as usize {
        // SAFETY: D3D12 calls this on a live queue.
        if let Some(queue) = unsafe { ID3D12CommandQueue::from_raw_borrowed(&this) }
            && unsafe { queue.GetDesc() }.Type == D3D12_COMMAND_LIST_TYPE_DIRECT
            && let Ok(mut slot) = DIRECT_QUEUE.try_lock()
        {
            *slot = Some(queue.clone());
            DIRECT_QUEUE_RAW.store(this as usize, Ordering::Relaxed);
        }
    }
    // SAFETY: stored from the slot this hook replaced.
    let original: Execute =
        unsafe { std::mem::transmute(ORIGINAL_EXECUTE.load(Ordering::Acquire)) };
    unsafe { original(this, count, lists) }
}

/// Patch `slot` of `object`'s vtable, keeping the original in `original`.
fn hook(
    object: *mut c_void,
    slot: usize,
    hook: usize,
    original: &AtomicUsize,
) -> Result<(), String> {
    // SAFETY: `object` is a live COM object whose vtable has `slot`.
    unsafe {
        let entry = (*object.cast::<*mut usize>()).add(slot);
        original.store(*entry, Ordering::Release);
        patch(entry, hook).ok_or_else(|| format!("cannot patch vtable slot {slot}"))?;
    }
    Ok(())
}

fn install() -> Result<(), String> {
    let class = wide("STATIC");
    let window = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            std::ptr::null(),
            WS_POPUP,
            0,
            0,
            16,
            16,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
        )
    };
    if window.is_null() {
        return Err("cannot create a window for the probe swap chain".into());
    }
    let result = (|| unsafe {
        let mut device: Option<ID3D12Device> = None;
        D3D12CreateDevice(None, D3D_FEATURE_LEVEL_11_0, &mut device)
            .map_err(|e| format!("D3D12CreateDevice: {e}"))?;
        let device = device.ok_or("D3D12CreateDevice returned no device")?;
        let queue: ID3D12CommandQueue = device
            .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                ..Default::default()
            })
            .map_err(|e| format!("CreateCommandQueue: {e}"))?;
        let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))
            .map_err(|e| format!("CreateDXGIFactory2: {e}"))?;
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: 16,
            Height: 16,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            ..Default::default()
        };
        let swapchain = factory
            .CreateSwapChainForHwnd(&queue, HWND(window), &desc, None, None)
            .map_err(|e| format!("CreateSwapChainForHwnd: {e}"))?;
        hook(
            swapchain.as_raw(),
            SLOT_PRESENT,
            present_hook as Present as usize,
            &ORIGINAL_PRESENT,
        )?;
        hook(
            swapchain.as_raw(),
            SLOT_PRESENT1,
            present1_hook as Present1 as usize,
            &ORIGINAL_PRESENT1,
        )?;
        hook(
            queue.as_raw(),
            SLOT_EXECUTE,
            execute_hook as Execute as usize,
            &ORIGINAL_EXECUTE,
        )?;
        Ok(())
    })();
    unsafe { DestroyWindow(window) };
    match &result {
        Ok(()) => log!("hooked DXGI Present/Present1 and ExecuteCommandLists"),
        Err(error) => log!("cannot hook presents: {error}"),
    }
    result
}

/// Copy a finished frame into the file, creating the file for the first frame.
fn write(shm: &mut Option<Shm>, path: &str, pending: Pending) {
    let Pending {
        queued,
        present,
        present_ns,
    } = pending;
    let readback = &queued.readback;
    let release = || readback.busy[queued.buffer].store(false, Ordering::Release);
    if !readback.wait(queued.fence_value) {
        log!("a frame copy did not finish");
        DROPPED.fetch_add(1, Ordering::Relaxed);
        return release();
    }
    if shm.is_none() {
        match Shm::create(path, readback.shape) {
            Ok(created) => *shm = Some(created),
            Err(error) => {
                log!("{error}");
                ACTIVE.store(false, Ordering::Release);
                return release();
            }
        }
    }
    let file = shm.as_mut().expect("created above");
    if file.shape != readback.shape {
        file.set_state(state::RESIZED);
        DROPPED.fetch_add(1, Ordering::Relaxed);
        return release();
    }
    let buffer = &readback.buffers[queued.buffer];
    let mut data: *mut c_void = std::ptr::null_mut();
    let range = D3D12_RANGE {
        Begin: 0,
        End: readback.total_bytes as usize,
    };
    unsafe {
        if buffer.Map(0, Some(&range), Some(&mut data)).is_ok() && !data.is_null() {
            let source = data.cast::<u8>().add(readback.footprint.Offset as usize);
            file.publish(
                source,
                readback.footprint.Footprint.RowPitch as usize,
                present,
                present_ns,
            );
            buffer.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
        } else {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
    release();
}

fn worker(receiver: Receiver<Pending>, path: String) {
    let mut shm: Option<Shm> = None;
    loop {
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(pending) => write(&mut shm, &path, pending),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if let Some(file) = shm.as_mut() {
            file.sync_presents(
                &PRESENT_TIMES,
                PRESENTS.load(Ordering::Acquire),
                DROPPED.load(Ordering::Relaxed),
            );
        }
    }
    if let Some(file) = shm {
        file.set_state(state::STOPPED);
    }
    log!("frame capture finished");
}

/// Start copying frames into a new file at the Windows path `path`, at most `fps` a second,
/// holding the game to `game_fps` presents a second when it is above 0.
pub fn start(path: String, fps: u32, game_fps: u32) -> Result<(), String> {
    INSTALLED.clone()?;
    stop();
    PRESENTS.store(0, Ordering::Release);
    DROPPED.store(0, Ordering::Release);
    let (sender, receiver) = sync_channel(copier::BUFFERS);
    std::thread::Builder::new()
        .name("bedrock-frames".into())
        .spawn(move || worker(receiver, path))
        .map_err(|error| format!("cannot start the frame thread: {error}"))?;
    let mut guard = STATE.lock().unwrap_or_else(|poison| poison.into_inner());
    *guard = Some(Capture {
        sender,
        interval_ns: if fps == 0 {
            0
        } else {
            1_000_000_000 / u64::from(fps)
        },
        next_due_ns: 0,
        copier: None,
        failed: false,
    });
    PACE_NEXT_NS.store(0, Ordering::Relaxed);
    PACE_INTERVAL_NS.store(
        if game_fps == 0 {
            0
        } else {
            1_000_000_000 / u64::from(game_fps)
        },
        Ordering::Relaxed,
    );
    ACTIVE.store(true, Ordering::Release);
    log!(
        "frame capture started at {fps} fps (0 = every frame), game capped at {game_fps} (0 = uncapped)"
    );
    Ok(())
}

/// Stop copying frames; the worker finishes queued copies and marks the file stopped. The
/// presenting queue stays known for the next capture.
pub fn stop() {
    ACTIVE.store(false, Ordering::Release);
    PACE_INTERVAL_NS.store(0, Ordering::Relaxed);
    let capture = STATE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .take();
    drop(capture);
}
