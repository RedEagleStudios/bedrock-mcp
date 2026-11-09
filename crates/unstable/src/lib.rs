//! Optional unstable layer loaded into `Minecraft.Windows.exe`.
//!
//! It reaches game internals through vtables found by class name in the
//! game's RTTI (see the `rtti` crate), or on a build without RTTI for a class,
//! by the string anchors in `rtti::anchors`; never by hard-coded addresses. Each
//! capability resolves, checks its vtable layout and installs itself on its
//! own; it reports `pending` until a live call confirms it and `active`
//! afterwards. A capability that cannot resolve on this build stays off
//! without affecting the others or the stable agent.

#![cfg(windows)]

mod camera;
mod capability;
mod content_log;
mod hud;
mod memory;
mod oreui;
mod perf;
mod player_render;
mod screens;
mod ui_tree;
mod vtable;
mod world_state;

use std::{ffi::c_void, path::PathBuf, sync::OnceLock};

use endpoint::{After, Service, log, wide};
use protocol::unstable::{Build, LogBatch, PROTOCOL_VERSION, Reply, Request, Status};
use windows_sys::Win32::{
    Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HMODULE},
    System::{
        LibraryLoader::{DisableThreadLibraryCalls, GetModuleHandleW},
        SystemServices::DLL_PROCESS_ATTACH,
        Threading::CreateMutexW,
    },
};

static BUILD: OnceLock<Build> = OnceLock::new();

/// Set to `1` in the game's environment to ignore RTTI and find every class
/// by string anchors, as on a build without RTTI.
const ANCHORS_ONLY: &str = "BEDROCK_MCP_UNSTABLE_ANCHORS_ONLY";

/// Windows `DllMain`: start the unstable layer on first load.
///
/// # Safety
/// Called by the Windows loader with a valid module handle.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllMain(module: HMODULE, reason: u32, _reserved: *mut c_void) -> i32 {
    if reason != DLL_PROCESS_ATTACH {
        return 1;
    }
    let name = wide(&format!("Local\\bedrock-unstable-{}", endpoint::pid()));
    let mutex = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if mutex.is_null() || unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return 1;
    }
    unsafe { DisableThreadLibraryCalls(module) };
    // SAFETY: the loader passed this module handle.
    let Some(dir) = (unsafe { endpoint::module_dir(module) }) else {
        return 1;
    };
    let _ = std::thread::Builder::new()
        .name("bedrock-unstable".into())
        .spawn(move || run(dir));
    1
}

/// PE timestamp and image size of the game executable.
fn identity(base: *const u8) -> (u32, u32) {
    // SAFETY: `base` is the game module's mapped headers.
    let header = unsafe { std::slice::from_raw_parts(base, 4096) };
    let read = |at: usize| u32::from_le_bytes(header[at..at + 4].try_into().unwrap());
    let pe = read(0x3c) as usize;
    (read(pe + 8), read(pe + 24 + 56))
}

fn run(dir: PathBuf) {
    endpoint::log::open(&dir.join(protocol::unstable::LOG_FILE));
    let base = unsafe { GetModuleHandleW(std::ptr::null()) } as *const u8;
    let (timestamp, image_size) = identity(base);
    log!(
        "bedrock-unstable protocol {PROTOCOL_VERSION} in pid {}, build {timestamp:#x}/{image_size:#x}",
        endpoint::pid()
    );
    // SAFETY: the game's own module stays mapped for the process lifetime.
    let image = match unsafe { rtti::Image::from_memory(base) } {
        Ok(image) => image,
        Err(error) => {
            log!("cannot read the game image: {error}");
            return;
        }
    };
    let start = std::time::Instant::now();
    let mut classes = rtti::index(&image);
    log!(
        "indexed {} RTTI classes in {:?}",
        classes.len(),
        start.elapsed()
    );
    if std::env::var_os(ANCHORS_ONLY).is_some_and(|value| value == "1") {
        log!("{ANCHORS_ONLY}=1: ignoring RTTI, finding classes by string anchors only");
        classes.clear();
    }
    let _ = BUILD.set(Build {
        version: None,
        timestamp,
        image_size,
        rtti_classes: classes.len(),
    });
    let resolver = vtable::Resolver::new(&image, &classes);
    content_log::install(&resolver);
    camera::install(&resolver);
    let hooked = screens::install(&resolver);
    hud::install(&resolver, &hooked);
    perf::install(&resolver, &hooked);
    ui_tree::install(&resolver, hooked);
    world_state::install(&resolver);
    oreui::install(&resolver);
    player_render::install(&resolver);
    if let Err(error) = endpoint::serve(&Unstable, &dir, protocol::unstable::ENDPOINT_FILE) {
        log!("unstable endpoint stopped: {error}");
    }
}

struct Unstable;

impl Service for Unstable {
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

    fn handle(&self, request: Request) -> (Reply, After) {
        let reply = match request {
            Request::Hello { .. } => Reply::Error {
                message: "already authenticated".into(),
            },
            Request::Status => Reply::Status(Status {
                protocol: PROTOCOL_VERSION,
                pid: endpoint::pid(),
                build: BUILD.get().cloned().unwrap_or(Build {
                    version: None,
                    timestamp: 0,
                    image_size: 0,
                    rtti_classes: 0,
                }),
                capabilities: capability::all(),
            }),
            Request::ContentLog { since, limit } => {
                let (entries, next, dropped) = content_log::read(since, limit as usize);
                Reply::ContentLog(LogBatch {
                    entries,
                    next,
                    dropped,
                })
            }
            Request::Screens => Reply::Screens {
                stacks: screens::read(),
            },
            Request::UiTree {
                stack,
                max_depth,
                visible_only,
            } => {
                let stacks: Vec<u64> = screens::read().iter().map(|stack| stack.id).collect();
                Reply::UiTree {
                    screens: ui_tree::read(&stacks, stack, max_depth, visible_only),
                }
            }
            Request::Camera => Reply::Camera(camera::read()),
            Request::Hud { chat_limit } => Reply::Hud(hud::read(chat_limit)),
            Request::WorldEntities {
                id,
                player,
                radius,
                entity_type,
                limit,
            } => {
                let query = world_state::Query {
                    id,
                    player,
                    radius,
                    entity_type,
                    limit,
                };
                world_state::entities(query)
                    .map_or_else(|message| Reply::Error { message }, Reply::WorldEntities)
            }
            Request::WorldPlayer { name } => world_state::player(name)
                .map_or_else(|message| Reply::Error { message }, Reply::WorldPlayer),
            Request::DynamicProperties { target, collection } => {
                world_state::dynamic_properties(target, collection)
                    .map_or_else(|message| Reply::Error { message }, Reply::DynamicProperties)
            }
            Request::OreUi { url } => match oreui::read(url) {
                Ok(views) => Reply::OreUi { views },
                Err(message) => Reply::Error { message },
            },
            Request::PlayerRender { wait_ms, actor } => {
                Reply::PlayerRender(player_render::read(wait_ms, actor))
            }
            Request::Perf => Reply::Perf(perf::read()),
        };
        (reply, None)
    }

    fn release(&self) {}
}
