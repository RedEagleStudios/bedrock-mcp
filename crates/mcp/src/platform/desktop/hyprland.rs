//! Hyprland: hyprctl finds, sizes and places the game window and keeps it rendering while
//! hidden; grim captures the window's own buffer. Configs written in Lua take `hl.*`
//! evaluations; older configs take keywords and plain dispatches, tried second.

use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};

use serde_json::Value;

use crate::{
    error::{Error, Result, bail},
    platform::{Desktop, Window, inline_scale},
};

pub const NAME: &str = "hyprland";
const UNFOCUSED_FPS: &str = "misc:render_unfocused_fps";

pub struct Hyprland;

impl Desktop for Hyprland {
    fn name(&self) -> &'static str {
        NAME
    }

    fn window(&self, pid: u32) -> Result<Option<Window>> {
        game_window(pid).map(Some)
    }

    fn resize(&self, pid: u32, width: u32, height: u32) -> Result<Window> {
        resize(pid, width, height)
    }

    fn keep_rendering(&self, pid: u32) -> Result<()> {
        keep_rendering(pid)
    }

    fn open_on_workspace(&self, workspace: &str) -> Result<()> {
        open_on_workspace(workspace)
    }

    fn move_to_workspace(&self, pid: u32, workspace: &str) -> Result<()> {
        move_to_workspace(pid, workspace)
    }

    fn screenshot(
        &self,
        window: &Window,
        path: &Path,
        max_width: Option<u32>,
    ) -> Option<Result<Option<Vec<u8>>>> {
        Some(screenshot(window, path, max_width))
    }

    fn raise_hidden_fps(&self, fps: i64) -> std::result::Result<Option<i64>, String> {
        raise_hidden_fps(fps)
    }

    fn restore_hidden_fps(&self, previous: i64) {
        restore_hidden_fps(previous);
    }

    fn refresh_rate(&self) -> Option<i64> {
        hyprctl(&["monitors", "-j"])
            .ok()?
            .as_array()?
            .iter()
            .filter_map(|monitor| monitor["refreshRate"].as_f64())
            .map(|rate| rate.round() as i64)
            .max()
    }
}

/// Whether `hyprctl version` answers, so the compositor really is reachable.
pub fn answers() -> bool {
    run("hyprctl", &["version"]).is_ok_and(|output| output.status.success())
}

fn run(program: &str, args: &[&str]) -> Result<std::process::Output> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| Error::msg(format!("Cannot run {program}: {error}")))
}

fn hyprctl(args: &[&str]) -> Result<Value> {
    let output = run("hyprctl", args)?;
    serde_json::from_slice(&output.stdout).map_err(|_| {
        let detail = String::from_utf8_lossy(if output.stdout.is_empty() {
            &output.stderr
        } else {
            &output.stdout
        });
        Error::msg(format!(
            "hyprctl {} failed: {}",
            args.join(" "),
            detail.trim()
        ))
    })
}

/// The window of process `pid` among `clients`, on its monitor from `monitors`.
fn find_window(clients: &Value, monitors: &Value, pid: u32) -> Option<Window> {
    let client = clients
        .as_array()?
        .iter()
        .find(|client| client["pid"].as_u64() == Some(u64::from(pid)))?;
    let monitor = monitors
        .as_array()?
        .iter()
        .find(|monitor| monitor["id"] == client["monitor"]);
    let pair = |key: &str, index: usize| client[key][index].as_i64().unwrap_or_default();
    let workspace = &client["workspace"];
    let visible = !client["hidden"].as_bool().unwrap_or(false)
        && monitor.is_some_and(|monitor| monitor["activeWorkspace"]["id"] == workspace["id"]);
    Some(Window {
        x: pair("at", 0),
        y: pair("at", 1),
        width: pair("size", 0),
        height: pair("size", 1),
        scale: monitor
            .and_then(|monitor| monitor["scale"].as_f64())
            .filter(|scale| *scale > 0.0)
            .unwrap_or(1.0),
        floating: client["floating"].as_bool().unwrap_or(false),
        visible,
        workspace: Some(workspace["name"].as_str().unwrap_or_default().to_owned()),
        toplevel: client["stableId"].as_str().map(str::to_owned),
        desktop: NAME,
    })
}

/// `grim -g` geometry.
fn geometry(window: &Window) -> String {
    format!(
        "{},{} {}x{}",
        window.x, window.y, window.width, window.height
    )
}

fn game_window(pid: u32) -> Result<Window> {
    let clients = hyprctl(&["clients", "-j"])?;
    let monitors = hyprctl(&["monitors", "-j"])?;
    find_window(&clients, &monitors, pid).ok_or_else(|| Error::msg("Minecraft has no window yet."))
}

/// Make the window floating and give it this logical size. Lua configs take `hl.dsp`
/// dispatches; older configs `setfloating` and `resizewindowpixel`.
fn resize(pid: u32, width: u32, height: u32) -> Result<Window> {
    let window = game_window(pid)?;
    let target = format!("pid:{pid}");
    if !window.floating {
        let toggle =
            format!("hl.dsp.window.float({{ window = \"{target}\", action = \"toggle\" }})");
        dispatch(&[&toggle]).or_else(|_| dispatch(&["setfloating", &target]))?;
    }
    let lua = format!(
        "hl.dsp.window.resize({{ window = \"{target}\", x = {width}, y = {height}, relative = false }})"
    );
    dispatch(&[&lua]).or_else(|_| {
        dispatch(&[
            "resizewindowpixel",
            &format!("exact {width} {height},{target}"),
        ])
    })?;
    game_window(pid)
}

/// Keep the game rendering while its workspace is hidden, at Hyprland's
/// `misc:render_unfocused_fps`; otherwise a hidden game drops to about one frame a second and
/// held input barely acts. Also turn off `focus_on_activate` for the window, so the game
/// activating itself never switches the user to its workspace. Lua configs take `hl.dsp`
/// dispatches; older configs `setprop`.
///
/// A window that opened on a hidden workspace stays at one frame a second until Hyprland lays
/// it out once, so a hidden window is floated and tiled back, which leaves it where it was.
fn keep_rendering(pid: u32) -> Result<()> {
    let target = format!("pid:{pid}");
    let set = |prop: &str, value: &str| {
        let lua = format!(
            "hl.dsp.window.set_prop({{ window = \"{target}\", prop = \"{prop}\", value = \"{value}\" }})"
        );
        dispatch(&[&lua]).or_else(|_| dispatch(&["setprop", &target, prop, value]))
    };
    set("focus_on_activate", "0")?;
    set("render_unfocused", "1")?;
    if game_window(pid).is_ok_and(|window| !window.visible) {
        let toggle =
            format!("hl.dsp.window.float({{ window = \"{target}\", action = \"toggle\" }})");
        for _ in 0..2 {
            dispatch(&[&toggle]).or_else(|_| dispatch(&["togglefloating", &target]))?;
        }
    }
    Ok(())
}

/// Name of the window rule [`open_on_workspace`] adds, so a later launch replaces it.
const WORKSPACE_RULE: &str = "bedrock-mcp-workspace";

/// Make the game window open on `workspace` without switching to it, through a window rule
/// on its class. The rule only places the window: `no_initial_focus` or `focus_on_activate`
/// in it leave the hidden game at one frame a second despite `render_unfocused`, so
/// [`keep_rendering`] turns `focus_on_activate` off on the window once it exists. Lua configs
/// take `hl.window_rule`; older configs a `windowrule` keyword. The rule lasts until Hyprland
/// reloads its config.
fn open_on_workspace(workspace: &str) -> Result<()> {
    let lua = format!(
        "hl.window_rule({{ name = \"{WORKSPACE_RULE}\", enabled = true, match = {{ class = \"^(minecraft\\\\.windows\\\\.exe)$\" }}, workspace = \"{workspace} silent\" }})"
    );
    let evaluated = run("hyprctl", &["eval", &lua])
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "ok");
    if !evaluated {
        let rule =
            format!("match:class ^(minecraft\\.windows\\.exe)$, workspace {workspace} silent");
        let output = run("hyprctl", &["keyword", "windowrule", &rule])?;
        if String::from_utf8_lossy(&output.stdout).trim() != "ok" {
            bail!("hyprctl could not add a window rule for workspace {workspace}");
        }
    }
    Ok(())
}

/// Move the game window to `workspace` without following it.
fn move_to_workspace(pid: u32, workspace: &str) -> Result<()> {
    let target = format!("pid:{pid}");
    let lua = format!(
        "hl.dsp.window.move({{ window = \"{target}\", workspace = \"{workspace}\", follow = false }})"
    );
    dispatch(&[&lua])
        .or_else(|_| dispatch(&["movetoworkspacesilent", &format!("{workspace},{target}")]))
}

fn dispatch(args: &[&str]) -> Result<()> {
    let mut full = vec!["dispatch"];
    full.extend_from_slice(args);
    let output = run("hyprctl", &full)?;
    let text = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || !text.trim().eq_ignore_ascii_case("ok") {
        bail!("hyprctl {} failed: {}", full.join(" "), text.trim());
    }
    Ok(())
}

/// Capture the window to `path` at full size; return an inline PNG at most `max_width`
/// physical pixels wide when asked.
fn screenshot(window: &Window, path: &Path, max_width: Option<u32>) -> Result<Option<Vec<u8>>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    grim(window, window.scale, path)?;
    let Some(max_width) = max_width else {
        return Ok(None);
    };
    let scale = inline_scale(window, max_width);
    if (scale - window.scale).abs() < f64::EPSILON {
        return Ok(Some(fs::read(path)?));
    }
    let name = path.file_stem().unwrap_or_default().to_string_lossy();
    let scaled = path.with_file_name(format!(".{name}.inline.png"));
    let result = grim(window, scale, &scaled).and_then(|()| Ok(fs::read(&scaled)?));
    let _ = fs::remove_file(&scaled);
    result.map(Some)
}

/// Capture the window at `scale` times its logical size. Its own buffer is captured when
/// Hyprland names the toplevel, so a window on a hidden workspace or under other windows
/// still captures; without a name only an on-screen window can be captured.
fn grim(window: &Window, scale: f64, path: &Path) -> Result<()> {
    let path = path.to_string_lossy();
    let output = match &window.toplevel {
        // Toplevel captures scale the window's physical buffer.
        Some(toplevel) => {
            let scale = format!("{}", scale / window.scale);
            run("grim", &["-s", &scale, "-T", toplevel, &path])?
        }
        None if window.visible => {
            let scale = format!("{scale}");
            run("grim", &["-s", &scale, "-g", &geometry(window), &path])?
        }
        None => bail!(
            "Minecraft's window is not on screen and Hyprland names no toplevel to capture; switch to its workspace ({}).",
            window.workspace.as_deref().unwrap_or_default()
        ),
    };
    if !output.status.success() {
        bail!(
            "grim failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Hyprland's frame rate for hidden windows with `render_unfocused`.
fn unfocused_fps() -> Result<i64> {
    hyprctl(&["-j", "getoption", UNFOCUSED_FPS])?["int"]
        .as_i64()
        .ok_or_else(|| Error::msg(format!("hyprctl did not report {UNFOCUSED_FPS}")))
}

/// Set Hyprland's frame rate for hidden windows with `render_unfocused`. Lua configs take an
/// `hl.config` evaluation; older configs a keyword.
fn set_unfocused_fps(fps: i64) -> Result<()> {
    let lua = format!("hl.config({{ misc = {{ render_unfocused_fps = {fps} }} }})");
    let evaluated = run("hyprctl", &["eval", &lua])
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "ok");
    if !evaluated {
        let output = run("hyprctl", &["keyword", UNFOCUSED_FPS, &fps.to_string()])?;
        if String::from_utf8_lossy(&output.stdout).trim() != "ok" {
            bail!("hyprctl could not set {UNFOCUSED_FPS}");
        }
    }
    Ok(())
}

/// Raise Hyprland's hidden-window frame rate to at least `fps` for a recording. Returns the
/// rate to restore afterwards, or a note when it could not be raised.
fn raise_hidden_fps(fps: i64) -> std::result::Result<Option<i64>, String> {
    match unfocused_fps() {
        Ok(current) if current < fps => match set_unfocused_fps(fps) {
            Ok(()) => Ok(Some(current)),
            Err(error) => Err(format!(
                "While hidden the game renders at {current} fps: {error}"
            )),
        },
        _ => Ok(None),
    }
}

/// Put back the hidden-window frame rate [`raise_hidden_fps`] replaced.
fn restore_hidden_fps(previous: i64) {
    let _ = set_unfocused_fps(previous);
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn hypr() -> (Value, Value) {
        let clients = json!([
            {"pid": 10, "at": [0, 0], "size": [5, 5], "monitor": 0, "workspace": {"id": 1, "name": "1"}},
            {"pid": 42, "at": [100, 50], "size": [1280, 720], "monitor": 1, "floating": true,
             "hidden": false, "workspace": {"id": 3, "name": "3"}},
        ]);
        let monitors = json!([
            {"id": 0, "scale": 1.0, "activeWorkspace": {"id": 1}},
            {"id": 1, "scale": 1.5, "activeWorkspace": {"id": 3}},
        ]);
        (clients, monitors)
    }

    #[test]
    fn windows_are_found_by_pid_with_their_monitor_scale() {
        let (clients, mut monitors) = hypr();
        let window = find_window(&clients, &monitors, 42).unwrap();
        assert_eq!(geometry(&window), "100,50 1280x720");
        assert_eq!(window.physical(), [1920, 1080]);
        assert!(window.visible && window.floating);
        assert_eq!(window.to_json()["workspace"], "3");
        monitors[1]["activeWorkspace"]["id"] = json!(4);
        assert!(!find_window(&clients, &monitors, 42).unwrap().visible);
        assert!(find_window(&clients, &monitors, 7).is_none());
    }

    #[test]
    fn inline_copies_are_scaled_to_the_maximum_width() {
        let (clients, monitors) = hypr();
        let window = find_window(&clients, &monitors, 42).unwrap();
        assert_eq!(inline_scale(&window, 960), 0.75);
        assert_eq!(inline_scale(&window, 4000), 1.5);
    }
}
