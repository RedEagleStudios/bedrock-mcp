//! Windows: the game runs natively; Win32 finds, launches, sizes and stops it.

use std::{
    collections::BTreeMap,
    env,
    ffi::{OsStr, OsString},
    fs::File,
    io::Write,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::sync_channel,
    },
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime},
};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, FILETIME, HANDLE, HWND, INVALID_HANDLE_VALUE, LPARAM, POINT, RECT,
        STILL_ACTIVE, WAIT_TIMEOUT,
    },
    Graphics::Gdi::ClientToScreen,
    NetworkManagement::IpHelper::{GetExtendedUdpTable, MIB_UDP6ROW, MIB_UDPROW, UDP_TABLE_BASIC},
    Networking::WinSock::{AF_INET, AF_INET6},
    System::{
        Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize},
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, MODULEENTRY32W, Module32FirstW, Module32NextW,
            PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPMODULE,
            TH32CS_SNAPPROCESS,
        },
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject, TerminateJobObject,
        },
        Pipes::CreatePipe,
        Threading::{
            CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, GetExitCodeProcess, GetProcessTimes,
            OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
            PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess,
            WaitForSingleObject,
        },
    },
    UI::{
        HiDpi::{
            AdjustWindowRectExForDpi, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow,
            SetThreadDpiAwarenessContext,
        },
        Shell::ShellExecuteW,
        WindowsAndMessaging::{
            EnumWindows, GW_OWNER, GWL_EXSTYLE, GWL_STYLE, GetClientRect, GetWindow,
            GetWindowLongPtrW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, IsZoomed,
            PostMessageW, SW_SHOWNOACTIVATE, SW_SHOWNORMAL, SWP_NOACTIVATE, SWP_NOMOVE,
            SWP_NOZORDER, SetWindowPos, ShowWindow, WM_CLOSE,
        },
    },
};

use super::{Desktop, VideoEncoder, Window};
use crate::{
    clock::{Clock, poll},
    error::{Error, Result, bail},
    install::{GAME_EXE, Install},
};

/// What [`Install::crash_handler_running`] looks for, for messages.
pub const CRASH_HANDLER: &str = "Windows Error Reporting";
/// How to build the Windows artifacts the server injects.
pub const BUILD_HINT: &str = "build it with `cargo build --release -p bedrock-agent -p bedrock-unstable` (or scripts/build-windows.ps1)";
/// A native build puts the DLLs next to this executable; nothing else is searched.
pub const ARTIFACT_DIRS: &[&str] = &[];

/// The game's application user model ID: package family name and app ID.
const APP: &str = r"shell:AppsFolder\Microsoft.MinecraftUWP_8wekyb3d8bbwe!Game";
const CRASH_REPORTER: &str = "werfault.exe";
/// How long the game gets to close its window before it is terminated.
const CLOSE_GRACE_MS: u32 = 15_000;
/// ffmpeg's input pipe size: 1 MiB, so a frame is not split into hundreds of writes.
const PIPE_BYTES: u32 = 1 << 20;

fn wide(text: impl AsRef<OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().chain(Some(0)).collect()
}

/// Closes a handle when dropped.
struct Handle(HANDLE);

impl Handle {
    fn open(access: u32, pid: u32) -> Option<Self> {
        // SAFETY: OpenProcess has no memory preconditions.
        let handle = unsafe { OpenProcess(access, 0, pid) };
        (!handle.is_null()).then_some(Self(handle))
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by this value.
        unsafe { CloseHandle(self.0) };
    }
}

// SAFETY: kernel handles may be used from any thread.
unsafe impl Send for Handle {}

/// A running process from a Toolhelp snapshot.
struct Process {
    pid: u32,
    parent: u32,
    /// Lower-cased executable name.
    name: String,
}

fn processes() -> Vec<Process> {
    // SAFETY: a process snapshot has no preconditions.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    let snapshot = Handle(snapshot);
    let mut found = Vec::new();
    // SAFETY: PROCESSENTRY32W is plain data; dwSize is set as the API requires.
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    // SAFETY: `entry` is a valid, sized PROCESSENTRY32W for the snapshot's lifetime.
    let mut more = unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0;
    while more {
        let length = entry
            .szExeFile
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(entry.szExeFile.len());
        found.push(Process {
            pid: entry.th32ProcessID,
            parent: entry.th32ParentProcessID,
            name: String::from_utf16_lossy(&entry.szExeFile[..length]).to_lowercase(),
        });
        // SAFETY: as above.
        more = unsafe { Process32NextW(snapshot.0, &mut entry) } != 0;
    }
    found
}

/// The paths of the modules loaded in process `pid`.
fn modules(pid: u32) -> Vec<PathBuf> {
    // SAFETY: a module snapshot has no preconditions; it fails for processes we cannot read.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, pid) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    let snapshot = Handle(snapshot);
    let mut found = Vec::new();
    // SAFETY: MODULEENTRY32W is plain data; dwSize is set as the API requires.
    let mut entry: MODULEENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<MODULEENTRY32W>() as u32;
    // SAFETY: `entry` is a valid, sized MODULEENTRY32W for the snapshot's lifetime.
    let mut more = unsafe { Module32FirstW(snapshot.0, &mut entry) } != 0;
    while more {
        let length = entry
            .szExePath
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(entry.szExePath.len());
        found.push(PathBuf::from(OsString::from_wide(
            &entry.szExePath[..length],
        )));
        // SAFETY: as above.
        more = unsafe { Module32NextW(snapshot.0, &mut entry) } != 0;
    }
    found
}

fn game_pids(all: &[Process]) -> impl Iterator<Item = u32> + '_ {
    all.iter()
        .filter(|process| process.name == GAME_EXE)
        .map(|process| process.pid)
}

/// `profile` is `%LOCALAPPDATA%\bedrock-mcp`; `prefix` is the roaming AppData folder.
impl Install {
    /// `$BEDROCK_MCP_HOME`, else `%LOCALAPPDATA%\bedrock-mcp`; the game data under `%APPDATA%`.
    pub fn from_env() -> Self {
        let var = |name: &str| env::var_os(name).filter(|value| !value.is_empty());
        let profile = var("BEDROCK_MCP_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                var("LOCALAPPDATA")
                    .map(PathBuf::from)
                    .unwrap_or_else(env::temp_dir)
                    .join("bedrock-mcp")
            });
        let prefix = var("APPDATA").map(PathBuf::from).unwrap_or_default();
        Self { profile, prefix }
    }

    /// Where a failed start leaves details: the game's own log folder.
    pub fn logs_dir(&self) -> PathBuf {
        self.prefix.join("Minecraft Bedrock").join("logs")
    }

    /// The running `Minecraft.Windows.exe`.
    pub fn game_pid(&self) -> Option<u32> {
        game_pids(&processes()).next()
    }

    /// The install game_launch drives: this one, since Windows has one game install. A
    /// `pid` must name the running game.
    pub fn select(&self, pid: Option<u32>) -> Result<Self> {
        if let Some(pid) = pid
            && !game_pids(&processes()).any(|game| game == pid)
        {
            bail!("Process {pid} is not a running Minecraft.");
        }
        Ok(self.clone())
    }

    /// Whether Windows Error Reporting is handling a crash of the game: a `WerFault.exe`
    /// started by the game process.
    pub fn crash_handler_running(&self) -> bool {
        let all = processes();
        let games: Vec<u32> = game_pids(&all).collect();
        all.iter()
            .any(|process| process.name == CRASH_REPORTER && games.contains(&process.parent))
    }

    /// The executable image of `pid`.
    pub fn game_exe(&self, pid: u32) -> Option<PathBuf> {
        let process = Handle::open(PROCESS_QUERY_LIMITED_INFORMATION, pid)?;
        let mut buffer = vec![0u16; 32_768];
        let mut length = buffer.len() as u32;
        // SAFETY: `buffer` holds `length` UTF-16 units.
        let ok = unsafe {
            QueryFullProcessImageNameW(
                process.0,
                PROCESS_NAME_WIN32,
                buffer.as_mut_ptr(),
                &mut length,
            )
        };
        (ok != 0).then(|| PathBuf::from(OsString::from_wide(&buffer[..length as usize])))
    }

    /// File names of DLLs loaded in game `pid` that come from neither the game, Windows, the
    /// program folders, nor this server: other mods, which may hook the same functions as
    /// the agent.
    pub fn other_mods(&self, pid: u32) -> Vec<String> {
        let lower = |path: &Path| PathBuf::from(path.to_string_lossy().to_lowercase());
        let mut known: Vec<PathBuf> = [
            "SystemRoot",
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ProgramW6432",
        ]
        .iter()
        .filter_map(|name| env::var_os(name).filter(|value| !value.is_empty()))
        .map(PathBuf::from)
        .collect();
        known.push(self.agent_dir());
        known.extend(
            self.game_exe(pid)
                .and_then(|exe| exe.parent().map(Path::to_path_buf)),
        );
        let known: Vec<PathBuf> = known.iter().map(|dir| lower(dir)).collect();
        let mut names: Vec<String> = modules(pid)
            .into_iter()
            .filter(|path| {
                path.extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("dll"))
            })
            .filter(|path| {
                let path = lower(path);
                !known.iter().any(|dir| path.starts_with(dir))
            })
            .filter_map(|path| Some(path.file_name()?.to_string_lossy().into_owned()))
            .collect();
        names.sort_by_key(|name| name.to_lowercase());
        names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        names
    }

    /// Start the game through the shell: its app ID, or a `minecraft://` link when one is
    /// the only argument. The shell starts the game, so no environment can be passed.
    /// Returns the install to drive: this one.
    pub fn launch(
        &self,
        args: &[String],
        extra_env: &[(String, String)],
        _headless: Option<(u32, u32)>,
        _timeout: Duration,
        _clock: &dyn Clock,
    ) -> Result<Self> {
        if !extra_env.is_empty() {
            bail!("env cannot be passed on Windows: the shell starts the game, not this server.");
        }
        let target = match args {
            [] => APP.to_owned(),
            [link] if link.starts_with("minecraft:") => link.clone(),
            _ => bail!(
                "On Windows the game takes no command-line arguments; pass nothing or one minecraft:// link."
            ),
        };
        shell_open(&target)?;
        Ok(self.clone())
    }

    /// Ask the game to close its windows; terminate it when it is still running 15 s later.
    pub fn stop(&self) -> Result<String> {
        let Some(pid) = self.game_pid() else {
            return Ok("Minecraft is not running.".into());
        };
        let process = Handle::open(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, pid)
            .ok_or_else(|| Error::msg(format!("Cannot open Minecraft (pid {pid}).")))?;
        for window in windows_of(pid) {
            // SAFETY: posting a message has no memory preconditions.
            unsafe { PostMessageW(window, WM_CLOSE, 0, 0) };
        }
        // SAFETY: waiting on a process handle this value owns.
        if unsafe { WaitForSingleObject(process.0, CLOSE_GRACE_MS) } != WAIT_TIMEOUT {
            return Ok(format!("Minecraft (pid {pid}) closed."));
        }
        // SAFETY: as above.
        if unsafe { TerminateProcess(process.0, 1) } == 0 {
            bail!(
                "Minecraft (pid {pid}) did not close and could not be terminated: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(format!(
            "Minecraft (pid {pid}) did not close within 15 s, so it was terminated."
        ))
    }
}

/// Headless runs use gamescope, which exists only on Linux: refuse a request for one.
pub fn resolve_headless(argument: Option<&serde_json::Value>) -> Result<Option<(u32, u32)>> {
    let asked = match argument {
        Some(serde_json::Value::Bool(on)) => *on,
        Some(serde_json::Value::String(text)) => !matches!(text.trim(), "" | "0" | "false" | "no"),
        Some(serde_json::Value::Null) | None => false,
        Some(_) => true,
    };
    if asked {
        bail!(
            "Headless runs need Linux with gamescope; on Windows the game renders in its own \
             window."
        );
    }
    Ok(None)
}

/// Windows has no gamescope.
pub fn runs_in_gamescope(_pid: u32) -> bool {
    false
}

pub fn set_headless_game(_size: Option<(u32, u32)>) {}

pub fn headless_game() -> Option<(u32, u32)> {
    None
}

/// Open `target` with the shell on a thread of its own, which COM needs initialised.
fn shell_open(target: &str) -> Result<()> {
    let target = wide(target);
    let opened = std::thread::spawn(move || {
        // SAFETY: COM is initialised and uninitialised on this thread only.
        unsafe {
            let initialised = CoInitializeEx(
                std::ptr::null(),
                (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
            ) >= 0;
            let result = ShellExecuteW(
                std::ptr::null_mut(),
                wide("open").as_ptr(),
                target.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            );
            if initialised {
                CoUninitialize();
            }
            // ShellExecuteW returns a value above 32 on success.
            result as isize > 32
        }
    })
    .join()
    .unwrap_or(false);
    if !opened {
        bail!(
            "The shell could not start Minecraft: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

/// The folders that may hold `Minecraft Bedrock` data folders: the roaming AppData folder.
pub fn roaming_dirs(prefix: &Path) -> Vec<PathBuf> {
    vec![prefix.to_path_buf()]
}

pub fn alive(pid: u32) -> bool {
    let Some(process) = Handle::open(PROCESS_QUERY_LIMITED_INFORMATION, pid) else {
        return false;
    };
    let mut code = 0u32;
    // SAFETY: `code` outlives the call.
    unsafe { GetExitCodeProcess(process.0, &mut code) != 0 && code == STILL_ACTIVE as u32 }
}

/// When process `pid` started.
pub fn started_at(pid: u32) -> Option<SystemTime> {
    let process = Handle::open(PROCESS_QUERY_LIMITED_INFORMATION, pid)?;
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: the four FILETIMEs outlive the call.
    let ok =
        unsafe { GetProcessTimes(process.0, &mut created, &mut exited, &mut kernel, &mut user) };
    if ok == 0 {
        return None;
    }
    // 100 ns ticks since 1601-01-01.
    let ticks = u64::from(created.dwHighDateTime) << 32 | u64::from(created.dwLowDateTime);
    let unix_ticks = ticks.checked_sub(116_444_736_000_000_000)?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_nanos(unix_ticks * 100))
}

/// Load the staged `dll` into the game from this process.
pub fn inject(_install: &Install, pid: u32, dll: &Path, _clock: &dyn Clock) -> Result<()> {
    bedrock_inject::inject(&dll.to_string_lossy(), pid).map_err(|(_, message)| {
        let name = dll.file_name().unwrap_or_default().to_string_lossy();
        Error::msg(format!("Could not inject {name}: {message}."))
    })
}

/// The path the agent in the game opens for `path`: the same path.
pub fn agent_path(path: &Path) -> String {
    path.display().to_string()
}

/// Run `body` with this thread aware of per-monitor DPI, so window sizes are physical pixels.
fn dpi_aware<T>(body: impl FnOnce() -> T) -> T {
    // SAFETY: the previous context is restored before returning.
    let previous =
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let result = body();
    if !previous.is_null() {
        // SAFETY: restoring the context this thread had.
        unsafe { SetThreadDpiAwarenessContext(previous) };
    }
    result
}

/// Top-level, unowned windows of process `pid`.
fn windows_of(pid: u32) -> Vec<HWND> {
    struct Search {
        pid: u32,
        found: Vec<HWND>,
    }
    unsafe extern "system" fn visit(window: HWND, data: LPARAM) -> i32 {
        // SAFETY: `data` is the Search below, alive for the whole enumeration.
        let search = unsafe { &mut *(data as *mut Search) };
        let mut owner_pid = 0u32;
        // SAFETY: `window` comes from EnumWindows.
        unsafe { GetWindowThreadProcessId(window, &mut owner_pid) };
        // SAFETY: as above.
        if owner_pid == search.pid && unsafe { GetWindow(window, GW_OWNER) }.is_null() {
            search.found.push(window);
        }
        1
    }
    let mut search = Search {
        pid,
        found: Vec::new(),
    };
    // SAFETY: `visit` only touches `search`, which outlives the call.
    unsafe { EnumWindows(Some(visit), &mut search as *mut Search as LPARAM) };
    search.found
}

/// The game's main window: its largest visible top-level window.
fn main_window(pid: u32) -> Result<HWND> {
    windows_of(pid)
        .into_iter()
        // SAFETY: the windows come from EnumWindows.
        .filter(|&window| unsafe { IsWindowVisible(window) } != 0)
        .max_by_key(|&window| {
            let size = client_size(window);
            i64::from(size.0) * i64::from(size.1)
        })
        .ok_or_else(|| Error::msg("Minecraft has no window yet."))
}

fn client_size(window: HWND) -> (i32, i32) {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: `rect` outlives the call.
    unsafe { GetClientRect(window, &mut rect) };
    (rect.right - rect.left, rect.bottom - rect.top)
}

fn dpi_scale(window: HWND) -> (u32, f64) {
    // SAFETY: GetDpiForWindow has no memory preconditions.
    let dpi = match unsafe { GetDpiForWindow(window) } {
        0 => 96,
        dpi => dpi,
    };
    (dpi, f64::from(dpi) / 96.0)
}

fn describe(window: HWND) -> Window {
    let (width, height) = client_size(window);
    let mut origin = POINT { x: 0, y: 0 };
    // SAFETY: `origin` outlives the call.
    unsafe { ClientToScreen(window, &mut origin) };
    let (_, scale) = dpi_scale(window);
    let logical = |value: i32| (f64::from(value) / scale).round() as i64;
    // SAFETY: the window comes from EnumWindows.
    let (visible, iconic, zoomed) = unsafe {
        (
            IsWindowVisible(window) != 0,
            IsIconic(window) != 0,
            IsZoomed(window) != 0,
        )
    };
    Window {
        x: logical(origin.x),
        y: logical(origin.y),
        width: logical(width),
        height: logical(height),
        scale,
        floating: !zoomed,
        visible: visible && !iconic,
        workspace: None,
        toplevel: None,
        desktop: DESKTOP,
    }
}

fn game_window(pid: u32) -> Result<Window> {
    dpi_aware(|| main_window(pid).map(describe))
}

/// Restore the window from minimized or maximized without activating it and give its client
/// area this logical size.
fn resize(pid: u32, width: u32, height: u32) -> Result<Window> {
    dpi_aware(|| {
        let window = main_window(pid)?;
        // SAFETY: the window comes from EnumWindows; the RECT outlives the calls.
        unsafe {
            if IsZoomed(window) != 0 || IsIconic(window) != 0 {
                ShowWindow(window, SW_SHOWNOACTIVATE);
            }
            let (dpi, scale) = dpi_scale(window);
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: (f64::from(width) * scale).round() as i32,
                bottom: (f64::from(height) * scale).round() as i32,
            };
            let style = GetWindowLongPtrW(window, GWL_STYLE) as u32;
            let ex_style = GetWindowLongPtrW(window, GWL_EXSTYLE) as u32;
            AdjustWindowRectExForDpi(&mut rect, style, 0, ex_style, dpi);
            if SetWindowPos(
                window,
                std::ptr::null_mut(),
                0,
                0,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            ) == 0
            {
                bail!(
                    "Cannot resize Minecraft's window: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
        Ok(describe(window))
    })
}

/// A minimized game stops presenting frames, so restore it without activating it. Windows
/// keeps rendering a covered or unfocused window at full rate.
fn keep_rendering(pid: u32) -> Result<()> {
    let window = dpi_aware(|| main_window(pid))?;
    // SAFETY: the window comes from EnumWindows.
    unsafe {
        if IsIconic(window) != 0 {
            ShowWindow(window, SW_SHOWNOACTIVATE);
        }
    }
    Ok(())
}

/// The Win32 window manager.
pub struct Win32;

/// [`Desktop::name`] of [`Win32`].
const DESKTOP: &str = "windows";

/// The desktop backend: Win32.
pub fn desktop() -> &'static dyn Desktop {
    &Win32
}

impl Desktop for Win32 {
    fn name(&self) -> &'static str {
        DESKTOP
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

    /// Windows has no workspaces the server places windows on.
    fn open_on_workspace(&self, _workspace: &str) -> Result<()> {
        Err(Error::msg(
            "workspaces exist only on Hyprland, so the window opens where Windows puts it",
        ))
    }

    fn move_to_workspace(&self, _pid: u32, workspace: &str) -> Result<()> {
        self.open_on_workspace(workspace)
    }

    /// The agent's frame copy captures the window, covered or not.
    fn screenshot(
        &self,
        _window: &Window,
        _path: &Path,
        _max_width: Option<u32>,
    ) -> Option<Result<Option<Vec<u8>>>> {
        None
    }

    /// Windows does not slow down hidden windows, so there is no rate to raise.
    fn raise_hidden_fps(&self, _fps: i64) -> std::result::Result<Option<i64>, String> {
        Ok(None)
    }

    fn restore_hidden_fps(&self, _previous: i64) {}
}

/// The file the agent copies frames of game `pid` into, in the temporary folder.
pub fn frame_file(pid: u32, stamp_ms: u128) -> PathBuf {
    env::temp_dir().join(format!("bedrock-frames-{pid}-{stamp_ms}"))
}

/// The local ports of the UDP table of address family `family`.
fn udp_ports(family: u16) -> Vec<u16> {
    let mut size = 0u32;
    // SAFETY: a null table asks for the needed size.
    unsafe {
        GetExtendedUdpTable(
            std::ptr::null_mut(),
            &mut size,
            0,
            u32::from(family),
            UDP_TABLE_BASIC,
            0,
        )
    };
    // u32 elements keep the table's fields aligned.
    let mut table = vec![0u32; (size as usize).div_ceil(4) + 16];
    size = (table.len() * 4) as u32;
    // SAFETY: `table` holds `size` bytes.
    let status = unsafe {
        GetExtendedUdpTable(
            table.as_mut_ptr().cast(),
            &mut size,
            0,
            u32::from(family),
            UDP_TABLE_BASIC,
            0,
        )
    };
    if status != 0 {
        return Vec::new();
    }
    let count = table[0] as usize;
    let rows = table[1..].as_ptr().cast::<u8>();
    let port = |raw: u32| u16::from_be(raw as u16);
    (0..count)
        .map(|index| {
            // SAFETY: the table holds `count` rows of the family's row type after the count.
            unsafe {
                if family == AF_INET {
                    port(
                        rows.cast::<MIB_UDPROW>()
                            .add(index)
                            .read_unaligned()
                            .dwLocalPort,
                    )
                } else {
                    port(
                        rows.cast::<MIB_UDP6ROW>()
                            .add(index)
                            .read_unaligned()
                            .dwLocalPort,
                    )
                }
            }
        })
        .collect()
}

/// Whether any socket has UDP port `port` bound.
pub fn udp_port_bound(port: u16) -> bool {
    [AF_INET, AF_INET6]
        .into_iter()
        .any(|family| udp_ports(family).contains(&port))
}

/// A child process in a Job Object, so stopping it stops everything it started.
pub struct ProcessGroup {
    child: Child,
    job: Handle,
    stdin: Option<ChildStdin>,
}

impl ProcessGroup {
    /// Spawn `command` in a new process group without a console window, with its stdin
    /// piped so [`ProcessGroup::stop`] can ask a Bedrock server to stop.
    pub fn spawn(command: &mut Command) -> std::io::Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
            .spawn()?;
        // SAFETY: a nameless job with default security.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            let error = std::io::Error::last_os_error();
            let _ = child.kill();
            return Err(error);
        }
        let job = Handle(job);
        // SAFETY: plain data; the job and child handles are valid.
        let assigned = unsafe {
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) != 0
                && AssignProcessToJobObject(job.0, child.as_raw_handle()) != 0
        };
        if !assigned {
            let error = std::io::Error::last_os_error();
            let _ = child.kill();
            return Err(error);
        }
        let stdin = child.stdin.take();
        Ok(Self { child, job, stdin })
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }

    pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Write `stop` to the process (a Bedrock server's console command) and close its input,
    /// then terminate the whole job when it has not exited after `grace`.
    pub fn stop(mut self, clock: &dyn Clock, grace: Duration) {
        if let Some(mut stdin) = self.stdin.take() {
            let _ = stdin.write_all(b"stop\n");
        }
        let exited = poll(clock, grace, Duration::from_millis(100), || {
            Ok(self.child.try_wait()?)
        });
        // SAFETY: the job handle is owned by this value.
        unsafe { TerminateJobObject(self.job.0, 1) };
        if !matches!(exited, Ok(Some(_))) {
            let _ = self.child.wait();
        }
    }
}

/// Encoders chosen per codec, so each is probed once.
static CHOSEN: Mutex<BTreeMap<String, VideoEncoder>> = Mutex::new(BTreeMap::new());

/// The first encoder for `codec` that encodes a test frame: NVENC, AMF, Quick Sync, Media
/// Foundation, then x264 or x265 on the CPU.
pub fn video_encoder(codec: &str) -> Result<VideoEncoder> {
    let mut chosen = CHOSEN.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some(encoder) = chosen.get(codec) {
        return Ok(encoder.clone());
    }
    let tried = candidates(codec)?;
    let Some(encoder) = tried.iter().find(|encoder| probe(encoder)).cloned() else {
        let names: Vec<&str> = tried
            .iter()
            .map(|encoder| encoder.codec[1].as_str())
            .collect();
        bail!(
            "ffmpeg has no working {codec} encoder; tried {}. Is ffmpeg on PATH?",
            names.join(", ")
        );
    };
    chosen.insert(codec.to_owned(), encoder.clone());
    Ok(encoder)
}

/// Every encoder for `codec`, best first, with constant-quality rate control.
fn candidates(codec: &str) -> Result<Vec<VideoEncoder>> {
    let (prefix, software) = match codec {
        "h264" => ("h264", ["libx264", "-preset", "veryfast", "-crf", "18"]),
        "hevc" => ("hevc", ["libx265", "-preset", "fast", "-crf", "20"]),
        other => bail!("codec must be h264 or hevc, not {other}."),
    };
    let encoder = |name: String, options: &[&str]| VideoEncoder {
        global: Vec::new(),
        upload: "format=nv12".into(),
        codec: ["-c:v".to_owned(), name]
            .into_iter()
            .chain(options.iter().map(|option| (*option).to_owned()))
            .collect(),
    };
    Ok(vec![
        encoder(
            format!("{prefix}_nvenc"),
            &["-preset", "p4", "-rc", "constqp", "-qp", "18"],
        ),
        encoder(
            format!("{prefix}_amf"),
            &["-rc", "cqp", "-qp_i", "18", "-qp_p", "18"],
        ),
        encoder(format!("{prefix}_qsv"), &["-global_quality", "18"]),
        encoder(
            format!("{prefix}_mf"),
            &[
                "-hw_encoding",
                "1",
                "-rate_control",
                "quality",
                "-quality",
                "90",
            ],
        ),
        encoder(software[0].to_owned(), &software[1..]),
    ])
}

/// Whether ffmpeg encodes one generated frame with `encoder`.
fn probe(encoder: &VideoEncoder) -> bool {
    Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=256x256:r=30",
            "-frames:v",
            "1",
            "-vf",
            &encoder.upload,
        ])
        .args(&encoder.codec)
        .args(["-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .is_ok_and(|status| status.success())
}

/// Spawn `command` reading its stdin from a 1 MiB pipe; returns the pipe's write end.
pub fn spawn_piped(command: &mut Command) -> std::io::Result<(Child, File)> {
    let (mut read, mut write): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: both handles are written by the call; neither end is inheritable, and the
    // child receives its own duplicate of the read end.
    if unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), PIPE_BYTES) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: CreatePipe returned two fresh handles this process owns.
    let (read, write) = unsafe {
        (
            OwnedHandle::from_raw_handle(read),
            OwnedHandle::from_raw_handle(write),
        )
    };
    let child = command
        .stdin(Stdio::from(read))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;
    Ok((child, File::from(write)))
}

/// The default output's audio, recorded through WASAPI loopback into a WAV file by a thread
/// of its own.
pub struct AudioCapture {
    path: PathBuf,
    started: Instant,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<std::result::Result<(), String>>>,
}

impl AudioCapture {
    /// How the mux puts this audio into the MP4: the WAV is encoded to AAC.
    pub const MUX_CODEC: &[&str] = &["-c:v", "copy", "-c:a", "aac", "-b:a", "192k"];

    /// Record next to `recording`, as `<stem>.audio.wav`.
    pub fn start(recording: &Path) -> Result<Self> {
        let path = recording.with_extension("audio.wav");
        let stop = Arc::new(AtomicBool::new(false));
        let (ready, started) = sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("bedrock-audio".into())
            .spawn({
                let (path, stop) = (path.clone(), Arc::clone(&stop));
                move || loopback::record(&path, &stop, &ready)
            })
            .map_err(|error| Error::msg(format!("Cannot start the audio capture: {error}")))?;
        match started.recv() {
            Ok(Ok(started)) => Ok(Self {
                path,
                started,
                stop,
                worker: Some(worker),
            }),
            Ok(Err(error)) => bail!("Cannot record audio: {error}"),
            Err(_) => {
                let error = worker
                    .join()
                    .ok()
                    .and_then(std::result::Result::err)
                    .unwrap_or_else(|| "the capture thread ended".into());
                bail!("Cannot record audio: {error}")
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// When capture started.
    pub fn started(&self) -> Instant {
        self.started
    }

    /// Stop the capture and finish the file; false when it failed. The capture thread
    /// checks for a stop every 10 ms, so this needs no timeout.
    pub fn finish(&mut self, _clock: &dyn Clock, _timeout: Duration) -> bool {
        self.stop.store(true, Ordering::Release);
        self.worker
            .take()
            .is_some_and(|worker| matches!(worker.join(), Ok(Ok(()))))
    }

    pub fn kill(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// WASAPI loopback capture of the default render endpoint.
mod loopback {
    use std::{
        fs::File,
        io::{BufWriter, Seek, SeekFrom, Write},
        path::Path,
        sync::{
            atomic::{AtomicBool, Ordering},
            mpsc::SyncSender,
        },
        time::{Duration, Instant},
    };

    use windows::Win32::{
        Media::Audio::{
            AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
            IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
            WAVEFORMATEX, eConsole, eRender,
        },
        System::Com::{
            CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
            CoUninitialize,
        },
    };

    /// The shared-mode buffer WASAPI keeps: 200 ms, in 100 ns units.
    const BUFFER: i64 = 2_000_000;
    const TICK: Duration = Duration::from_millis(10);

    /// Record into `path` until `stop`; reports the start time (or why it could not start)
    /// through `ready`.
    pub fn record(
        path: &Path,
        stop: &AtomicBool,
        ready: &SyncSender<std::result::Result<Instant, String>>,
    ) -> std::result::Result<(), String> {
        // SAFETY: COM is initialised for this thread and uninitialised when it ends.
        let initialised = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        let result = capture(path, stop, ready);
        if let Err(error) = &result {
            let _ = ready.try_send(Err(error.clone()));
        }
        if initialised {
            // SAFETY: balances the CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
        result
    }

    fn capture(
        path: &Path,
        stop: &AtomicBool,
        ready: &SyncSender<std::result::Result<Instant, String>>,
    ) -> std::result::Result<(), String> {
        let failed =
            |what: &'static str| move |error: windows::core::Error| format!("{what}: {error}");
        // SAFETY: WASAPI calls on COM interfaces this function owns; the mix format is
        // read before it is freed, and capture buffers are read before they are released.
        unsafe {
            let devices: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(failed("no audio device enumerator"))?;
            let device = devices
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(failed("no default output device"))?;
            let client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(failed("cannot open the output device"))?;
            let format = client
                .GetMixFormat()
                .map_err(failed("no output mix format"))?;
            let format_bytes = std::slice::from_raw_parts(
                format.cast::<u8>(),
                size_of::<WAVEFORMATEX>() + usize::from((*format).cbSize),
            )
            .to_vec();
            let block = usize::from((*format).nBlockAlign);
            let rate = (*format).nSamplesPerSec;
            let initialised = client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                BUFFER,
                0,
                format,
                None,
            );
            CoTaskMemFree(Some(format.cast_const().cast()));
            initialised.map_err(failed("cannot capture the output"))?;
            let source: IAudioCaptureClient =
                client.GetService().map_err(failed("no capture service"))?;
            let mut wav = Wav::create(path, &format_bytes, block)
                .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
            client.Start().map_err(failed("cannot start capturing"))?;
            let started = Instant::now();
            let _ = ready.try_send(Ok(started));
            let mut written = 0u64;
            let result = loop {
                if stop.load(Ordering::Acquire) {
                    break Ok(());
                }
                std::thread::sleep(TICK);
                let mut got = false;
                let step = (|| -> std::result::Result<(), String> {
                    while source
                        .GetNextPacketSize()
                        .map_err(failed("capture failed"))?
                        > 0
                    {
                        let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                        source
                            .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                            .map_err(failed("capture failed"))?;
                        let bytes = frames as usize * block;
                        let wrote = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                            wav.silence(frames as u64)
                        } else {
                            wav.write(std::slice::from_raw_parts(data, bytes))
                        };
                        source
                            .ReleaseBuffer(frames)
                            .map_err(failed("capture failed"))?;
                        wrote.map_err(|error| error.to_string())?;
                        written += u64::from(frames);
                        got = true;
                    }
                    Ok(())
                })();
                if let Err(error) = step {
                    break Err(error);
                }
                // Loopback delivers nothing while nothing plays; fill the gap with silence
                // so the audio stays in time with the video.
                let due = (started.elapsed().as_secs_f64() * f64::from(rate)) as u64;
                let slack = u64::from(rate / 10);
                if !got && due > written + slack {
                    let gap = due - written - u64::from(rate / 50);
                    if let Err(error) = wav.silence(gap) {
                        break Err(error.to_string());
                    }
                    written += gap;
                }
            };
            let _ = client.Stop();
            let finished = wav.finish().map_err(|error| error.to_string());
            result.and(finished)
        }
    }

    /// A WAV file in the device's own format; sizes are filled in by [`Wav::finish`].
    struct Wav {
        file: BufWriter<File>,
        block: usize,
        data: u64,
        data_at: u64,
    }

    impl Wav {
        fn create(path: &Path, format: &[u8], block: usize) -> std::io::Result<Self> {
            let mut file = BufWriter::new(File::create(path)?);
            file.write_all(b"RIFF\0\0\0\0WAVEfmt ")?;
            file.write_all(&(format.len() as u32).to_le_bytes())?;
            file.write_all(format)?;
            if format.len() % 2 == 1 {
                file.write_all(&[0])?;
            }
            file.write_all(b"data\0\0\0\0")?;
            let data_at = file.stream_position()? - 4;
            Ok(Self {
                file,
                block,
                data: 0,
                data_at,
            })
        }

        fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
            self.file.write_all(bytes)?;
            self.data += bytes.len() as u64;
            Ok(())
        }

        fn silence(&mut self, frames: u64) -> std::io::Result<()> {
            let zeros = [0u8; 4096];
            let mut left = frames * self.block as u64;
            while left > 0 {
                let chunk = left.min(zeros.len() as u64) as usize;
                self.write(&zeros[..chunk])?;
                left -= chunk as u64;
            }
            Ok(())
        }

        fn finish(mut self) -> std::io::Result<()> {
            let data = u32::try_from(self.data).unwrap_or(u32::MAX);
            self.file.seek(SeekFrom::Start(self.data_at))?;
            self.file.write_all(&data.to_le_bytes())?;
            let riff = u32::try_from(self.data_at + 4 - 8 + self.data).unwrap_or(u32::MAX);
            self.file.seek(SeekFrom::Start(4))?;
            self.file.write_all(&riff.to_le_bytes())?;
            self.file.flush()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_codec_ends_with_a_software_encoder() {
        let h264 = candidates("h264").unwrap();
        assert_eq!(h264[0].codec[1], "h264_nvenc");
        assert_eq!(h264.last().unwrap().codec[1], "libx264");
        assert_eq!(
            candidates("hevc").unwrap().last().unwrap().codec[1],
            "libx265"
        );
        assert!(candidates("av1").is_err());
    }

    #[test]
    fn this_process_is_alive_and_has_a_start_time() {
        let pid = std::process::id();
        assert!(alive(pid));
        let started = started_at(pid).unwrap();
        assert!(started <= SystemTime::now());
        assert!(SystemTime::now().duration_since(started).unwrap() < Duration::from_secs(3600));
    }
}
