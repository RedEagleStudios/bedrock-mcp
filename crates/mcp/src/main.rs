//! Bedrock MCP for Minecraft: a stdio MCP server for agentic AI add-on
//! development and testing. It drives Minecraft Bedrock (the GDK build, natively on Windows or
//! under Wine or Proton on Linux, started by any launcher) through an injected agent DLL for
//! input and frame copies, Bedrock's WebSocket automation protocol for commands and events, the
//! game's log files, and window captures.

mod agent;
mod analyze;
mod bds;
mod bridge;
mod camera;
mod clock;
mod content_log;
mod docs;
mod drivers;
mod embedded;
mod error;
mod events;
mod frame_file;
mod game_files;
mod gametest;
mod inject;
mod input;
mod install;
mod keys;
mod nbt;
mod oreui;
mod packs;
mod platform;
mod prompts;
mod recorder;
mod rpc;
mod screen_text;
mod session;
mod still;
#[cfg(test)]
mod testdir;
mod timeline;
mod tools;
mod ui_tree;
mod unstable;
mod world;
mod worlds;
mod ws;

use std::{io, sync::Arc};

fn main() {
    // `bedrock-mcp docs` prints the tool reference (docs/tools.md) and exits; `--version` prints
    // the version the installer compares against.
    match std::env::args().nth(1).as_deref() {
        Some("docs") => {
            print!("{}", tools::reference());
            return;
        }
        Some("--version") => {
            println!("bedrock-mcp {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        _ => {}
    }
    let toolsets = tools::Toolsets::from_env().unwrap_or_else(|error| {
        eprintln!("bedrock-mcp: {error}");
        std::process::exit(2);
    });
    let mut session = session::Session::new(
        install::Install::from_env(),
        Arc::new(clock::SystemClock),
        toolsets,
    );
    let input = io::BufReader::new(io::stdin());
    if let Err(error) = rpc::serve(input, io::stdout().lock(), &mut session) {
        eprintln!("bedrock-mcp: {error}");
        std::process::exit(1);
    }
}
