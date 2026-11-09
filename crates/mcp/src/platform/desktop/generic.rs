//! Any other Linux desktop, Wayland or X11: the server uses no compositor API. The agent
//! reports the window size and copies the frames screenshots are made of; resizing and
//! workspaces are left to the user.

use std::path::Path;

use crate::{
    error::{Error, Result},
    platform::{Desktop, Window},
};

pub const NAME: &str = "generic";

pub struct Generic;

fn unsupported(what: &str) -> Error {
    Error::msg(format!(
        "{what} is not supported on this desktop: the server drives windows only on Hyprland"
    ))
}

impl Desktop for Generic {
    fn name(&self) -> &'static str {
        NAME
    }

    fn window(&self, _pid: u32) -> Result<Option<Window>> {
        Ok(None)
    }

    fn resize(&self, _pid: u32, _width: u32, _height: u32) -> Result<Window> {
        Err(unsupported("Resizing the game window"))
    }

    /// Nothing to do: the server cannot change how this desktop paces hidden windows.
    fn keep_rendering(&self, _pid: u32) -> Result<()> {
        Ok(())
    }

    fn open_on_workspace(&self, _workspace: &str) -> Result<()> {
        Err(unsupported("Placing the game window on a workspace"))
    }

    fn move_to_workspace(&self, _pid: u32, _workspace: &str) -> Result<()> {
        Err(unsupported("Moving the game window to a workspace"))
    }

    fn screenshot(
        &self,
        _window: &Window,
        _path: &Path,
        _max_width: Option<u32>,
    ) -> Option<Result<Option<Vec<u8>>>> {
        None
    }

    fn raise_hidden_fps(&self, _fps: i64) -> std::result::Result<Option<i64>, String> {
        Ok(None)
    }

    fn restore_hidden_fps(&self, _previous: i64) {}
}
