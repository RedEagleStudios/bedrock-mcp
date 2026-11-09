//! What differs between the operating systems and desktops the server runs on.
//!
//! On Linux the game runs under Wine or Proton, found by its process whatever launcher started
//! it, and VAAPI encodes recordings while
//! PulseAudio records sound. The window is handled by a [`Desktop`] chosen at runtime: on
//! Hyprland, hyprctl places and sizes it and grim captures it; on any other desktop the agent
//! reports its size and copies its frames. On Windows the game runs natively: Toolhelp finds
//! it, the shell launches it, Win32 sizes its window, the agent's frame copy captures it,
//! ffmpeg's NVENC, AMF, Quick Sync, Media Foundation or x264 encoder records it and WASAPI
//! loopback records sound.
//!
//! Both OS modules export the same items: `impl Install` (paths, process discovery, launch and
//! stop), `alive`, `started_at`, `inject`, `agent_path`, `desktop`, `frame_file`,
//! `udp_port_bound`, `ProcessGroup`, `video_encoder`, `spawn_piped` and `AudioCapture`.

#[cfg(unix)]
mod desktop;
#[cfg(unix)]
mod linux;
#[cfg(unix)]
pub use desktop::current as desktop;
#[cfg(all(unix, test))]
pub use desktop::generic as generic_desktop;
#[cfg(unix)]
pub use linux::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(not(any(unix, windows)))]
compile_error!("bedrock-mcp runs on Linux or Windows.");

use std::path::Path;

use protocol::Status;
use serde_json::{Value, json};

use crate::error::Result;

/// How the server reaches the game window on this desktop. Callers turn the errors of
/// [`Desktop::resize`], [`Desktop::open_on_workspace`] and [`Desktop::move_to_workspace`]
/// into notes: a desktop that cannot do them still runs the game.
pub trait Desktop: Sync {
    /// The backend's name, reported as `window.desktop`.
    fn name(&self) -> &'static str;

    /// The game window, or `None` when this desktop exposes no windows; callers then take the
    /// size from the agent with [`Window::from_agent`].
    fn window(&self, pid: u32) -> Result<Option<Window>>;

    /// Give the window this logical size.
    fn resize(&self, pid: u32, width: u32, height: u32) -> Result<Window>;

    /// Keep the game rendering at full rate while its window is hidden or unfocused.
    fn keep_rendering(&self, pid: u32) -> Result<()>;

    /// Make the next game window open on `workspace` without switching to it.
    fn open_on_workspace(&self, workspace: &str) -> Result<()>;

    /// Move the running game window to `workspace` without following it.
    fn move_to_workspace(&self, pid: u32, workspace: &str) -> Result<()>;

    /// Capture the window to `path` at full size and return an inline PNG at most `max_width`
    /// physical pixels wide when asked. `None` when the desktop cannot capture windows; callers
    /// then use the agent's frame copy.
    fn screenshot(
        &self,
        window: &Window,
        path: &Path,
        max_width: Option<u32>,
    ) -> Option<Result<Option<Vec<u8>>>>;

    /// Raise the frame rate of a hidden window to at least `fps` for a recording. Returns the
    /// rate to restore afterwards, or a note when it could not be raised.
    fn raise_hidden_fps(&self, fps: i64) -> std::result::Result<Option<i64>, String>;

    /// Put back the rate [`Desktop::raise_hidden_fps`] replaced.
    fn restore_hidden_fps(&self, previous: i64);

    /// The highest refresh rate among the monitors, when the desktop reports one.
    fn refresh_rate(&self) -> Option<i64> {
        None
    }
}

/// The game window in logical pixels, and its monitor's scale.
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
    pub scale: f64,
    pub floating: bool,
    pub visible: bool,
    /// Hyprland's workspace name; other desktops have none.
    pub workspace: Option<String>,
    /// Hyprland's `stableId`, the foreign-toplevel identifier `grim -T` captures.
    pub toplevel: Option<String>,
    /// The [`Desktop::name`] that described the window.
    pub desktop: &'static str,
}

impl Window {
    /// The window as the agent sees it: its client size at scale 1, since the agent reports
    /// physical pixels and no monitor scale. Position and visibility are unknown; the window
    /// counts as visible because the agent only answers while the game runs.
    pub fn from_agent(status: &Status, desktop: &'static str) -> Self {
        Self {
            x: 0,
            y: 0,
            width: i64::from(status.width),
            height: i64::from(status.height),
            scale: 1.0,
            floating: false,
            visible: true,
            workspace: None,
            toplevel: None,
            desktop,
        }
    }

    /// Client size in physical pixels.
    pub fn physical(&self) -> [i64; 2] {
        [
            (self.width as f64 * self.scale).round() as i64,
            (self.height as f64 * self.scale).round() as i64,
        ]
    }

    pub fn to_json(&self) -> Value {
        json!({
            "at": [self.x, self.y],
            "size": [self.width, self.height],
            "physical_size": self.physical(),
            "scale": self.scale,
            "floating": self.floating,
            "visible": self.visible,
            "workspace": self.workspace,
            "desktop": self.desktop,
        })
    }
}

/// The scale, relative to logical pixels, of an inline copy at most `max_width` physical
/// pixels wide.
pub fn inline_scale(window: &Window, max_width: u32) -> f64 {
    let full = window.physical()[0].max(1) as f64;
    window.scale * (f64::from(max_width) / full).min(1.0)
}

/// How ffmpeg encodes the raw frames: arguments before the input (a hardware device), the
/// filter that converts frames for the encoder, and the encoder with its rate control.
#[derive(Clone, Debug, PartialEq)]
pub struct VideoEncoder {
    pub global: Vec<String>,
    pub upload: String,
    pub codec: Vec<String>,
}
