//! Agent loaded into `Minecraft.Windows.exe` to drive its input.
//!
//! The agent assumes Minecraft reads the keyboard from window messages and
//! the mouse from GameInput readings, and works however often the game polls
//! them. It posts key and text messages to the game window and merges
//! synthetic buttons, motion, wheel and pointer position into every GameInput
//! mouse reading the game takes. While a client is connected it also keeps
//! the game active in the background ([`focus`]). It hooks only public
//! GameInput interfaces, the window procedure and the executable's imports of
//! user32 focus functions, never game code.
//!
//! The agent serves the [`protocol`] on a loopback port through
//! [`endpoint`] and writes its endpoint and log next to its DLL.

#![cfg(windows)]

mod focus;
mod frames;
mod gameinput;
mod iat;
mod keyboard;
mod server;

use std::{ffi::c_void, path::PathBuf};

use endpoint::{log, wide};
use windows_sys::Win32::{
    Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HMODULE},
    System::{
        LibraryLoader::DisableThreadLibraryCalls, SystemServices::DLL_PROCESS_ATTACH,
        Threading::CreateMutexW,
    },
};

/// Windows `DllMain`: start the agent thread on first load.
///
/// # Safety
/// Called by the Windows loader with a valid module handle.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllMain(module: HMODULE, reason: u32, _reserved: *mut c_void) -> i32 {
    if reason != DLL_PROCESS_ATTACH {
        return 1;
    }
    // A second copy loaded from another path must not hook twice.
    let name = wide(&format!("Local\\bedrock-agent-{}", endpoint::pid()));
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
        .name("bedrock-agent".into())
        .spawn(move || run(dir));
    1
}

fn run(dir: PathBuf) {
    endpoint::log::open(&dir.join(protocol::LOG_FILE));
    log!(
        "bedrock-agent protocol {} in pid {}",
        protocol::PROTOCOL_VERSION,
        endpoint::pid()
    );
    if !keyboard::find_game_window() {
        log!("no game window found");
        return;
    }
    if gameinput::install().is_empty() {
        log!("GameInput is not loaded; mouse input is unavailable");
    }
    if !focus::install() {
        log!("the game cannot be kept active in the background");
    }
    if let Err(error) = endpoint::serve(&server::Agent, &dir, protocol::ENDPOINT_FILE) {
        log!("control endpoint stopped: {error}");
    }
}
