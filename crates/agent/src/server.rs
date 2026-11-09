//! The agent's requests, served through the shared [`endpoint`].

use std::sync::atomic::Ordering;

use endpoint::{Service, log};
use protocol::{CrashKind, PROTOCOL_VERSION, Reply, Request, Status};

use crate::{focus, frames, gameinput, keyboard, lock};

pub struct Agent;

fn status() -> Status {
    let (width, height, foreground) = keyboard::window_state();
    let synthetic = &gameinput::SYNTHETIC;
    Status {
        protocol: PROTOCOL_VERSION,
        pid: endpoint::pid(),
        width,
        height,
        foreground,
        game_active: focus::game_active(),
        mouse_readings: synthetic.readings.load(Ordering::Relaxed),
        gameinput_versions: gameinput::hooked_versions(),
        pointer: [
            synthetic.real_x.load(Ordering::Relaxed),
            synthetic.real_y.load(Ordering::Relaxed),
        ],
        pointer_override: synthetic.pointer_active.load(Ordering::Relaxed),
        input_locked: lock::locked(),
        takeovers: lock::takeovers(),
    }
}

fn crash(kind: CrashKind) -> ! {
    log!("crashing on request: {kind:?}");
    match kind {
        CrashKind::Panic => panic!("bedrock-agent: crash requested"),
        // SAFETY: none; this deliberately faults.
        CrashKind::AccessViolation => unsafe {
            std::ptr::write_volatile(std::ptr::null_mut::<u32>(), 1);
            unreachable!()
        },
        CrashKind::StackOverflow => {
            #[allow(unconditional_recursion, reason = "overflowing the stack is the point")]
            fn recurse(depth: u64) -> u64 {
                let frame = std::hint::black_box([depth; 64]);
                recurse(frame[0] + 1) + frame[63]
            }
            std::hint::black_box(recurse(0));
            unreachable!()
        }
        CrashKind::Abort => std::process::abort(),
    }
}

/// Post a mouse message where the game currently sees the pointer.
fn mouse_activity() {
    let synthetic = &gameinput::SYNTHETIC;
    let (x, y) = if synthetic.pointer_active.load(Ordering::Acquire) {
        (
            synthetic.pointer_x.load(Ordering::Relaxed),
            synthetic.pointer_y.load(Ordering::Relaxed),
        )
    } else {
        (
            synthetic.real_x.load(Ordering::Relaxed),
            synthetic.real_y.load(Ordering::Relaxed),
        )
    };
    keyboard::mouse_activity(x, y);
}

fn run(request: Request) -> Reply {
    let synthetic = &gameinput::SYNTHETIC;
    let ok = |done: bool, what: &str| {
        if done {
            Reply::Ok
        } else {
            Reply::Error {
                message: format!("could not {what}"),
            }
        }
    };
    match request {
        Request::Hello { .. } => Reply::Error {
            message: "already authenticated".into(),
        },
        Request::Status => Reply::Status(status()),
        Request::Key {
            down: true,
            vk,
            scan,
            extended,
        } => ok(keyboard::key_down(vk, scan, extended), "post the key"),
        Request::Key {
            down: false, vk, ..
        } => ok(keyboard::key_up(vk), "post the key"),
        Request::Char { code } => ok(keyboard::text(code), "post the character"),
        Request::Move { dx, dy } => {
            synthetic.motion_x.fetch_add(dx, Ordering::Relaxed);
            synthetic.motion_y.fetch_add(dy, Ordering::Relaxed);
            mouse_activity();
            Reply::Ok
        }
        Request::Wheel { dx, dy } => {
            synthetic.wheel_x.fetch_add(dx, Ordering::Relaxed);
            synthetic.wheel_y.fetch_add(dy, Ordering::Relaxed);
            mouse_activity();
            Reply::Ok
        }
        Request::Buttons { mask } if mask & !protocol::mouse::ALL == 0 => {
            synthetic.buttons.store(mask, Ordering::Relaxed);
            mouse_activity();
            Reply::Ok
        }
        Request::Buttons { mask } => Reply::Error {
            message: format!("unknown mouse buttons {mask:#x}"),
        },
        Request::Pointer { x, y } => {
            synthetic.pointer_x.store(x, Ordering::Relaxed);
            synthetic.pointer_y.store(y, Ordering::Relaxed);
            synthetic.pointer_active.store(true, Ordering::Release);
            mouse_activity();
            Reply::Ok
        }
        Request::PointerRelease => {
            synthetic.pointer_active.store(false, Ordering::Release);
            Reply::Ok
        }
        Request::Reset => {
            // Releases input only; a running recording and the focus hold stay as they are.
            keyboard::release_all();
            gameinput::release();
            Reply::Ok
        }
        Request::Crash { .. } => Reply::Ok,
        Request::FramesStart {
            path,
            fps,
            game_fps,
        } => match frames::start(path, fps, game_fps) {
            Ok(()) => Reply::Ok,
            Err(message) => Reply::Error { message },
        },
        Request::FramesStop => {
            frames::stop();
            Reply::Ok
        }
        Request::InputLock { locked } => {
            lock::set(locked);
            Reply::Status(status())
        }
    }
}

impl Service for Agent {
    type Request = Request;
    type Reply = Reply;
    const PROTOCOL: u32 = PROTOCOL_VERSION;

    fn hello(request: &Request) -> Option<(&str, u32)> {
        match request {
            Request::Hello { token, protocol } => Some((token, *protocol)),
            _ => None,
        }
    }

    fn ok(&self) -> Reply {
        Reply::Ok
    }

    fn error(&self, message: String) -> Reply {
        Reply::Error { message }
    }

    fn handle(&self, request: Request) -> (Reply, Option<Box<dyn FnOnce() + Send>>) {
        let after: Option<Box<dyn FnOnce() + Send>> = match request {
            Request::Crash { kind } => Some(Box::new(move || {
                std::thread::sleep(std::time::Duration::from_millis(100));
                crash(kind)
            })),
            _ => None,
        };
        (run(request), after)
    }

    fn release(&self) {
        // The player gets their input back whenever the client goes away.
        lock::set(false);
        frames::stop();
        keyboard::release_all();
        gameinput::release();
        focus::release();
    }

    fn attached(&self) {
        focus::hold();
    }
}
