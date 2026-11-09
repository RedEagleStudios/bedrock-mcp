//! Loading a DLL into a running Windows process: the DLL's path is written into the target
//! and `LoadLibraryW` runs on a remote thread. `bedrock-inject.exe` wraps this for Wine;
//! bedrock-mcp on Windows calls it in-process.
//!
//! Failures carry the injector's exit code: 3 process not found, 4 OpenProcess failed,
//! 5 writing the path failed, 6 the remote thread failed to start, 7 LoadLibraryW returned
//! null, 8 loading timed out.

#[cfg(windows)]
pub use windows::{Failure, find_process, inject};

#[cfg(windows)]
mod windows {
    use std::{ffi::c_void, mem::size_of};

    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0},
        System::{
            Diagnostics::{
                Debug::WriteProcessMemory,
                ToolHelp::{
                    CreateToolhelp32Snapshot, MODULEENTRY32W, Module32FirstW, PROCESSENTRY32W,
                    Process32FirstW, Process32NextW, TH32CS_SNAPMODULE, TH32CS_SNAPPROCESS,
                },
            },
            LibraryLoader::{GetModuleHandleW, GetProcAddress},
            Memory::{
                MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAllocEx, VirtualFreeEx,
            },
            Threading::{
                CreateRemoteThread, GetExitCodeThread, OpenProcess, PROCESS_CREATE_THREAD,
                PROCESS_QUERY_INFORMATION, PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE,
                WaitForSingleObject,
            },
        },
    };

    const LOAD_TIMEOUT_MS: u32 = 30_000;

    /// An exit code and a message.
    pub type Failure = (i32, String);

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    /// Closes a handle when dropped.
    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    /// The text of a NUL-terminated UTF-16 buffer.
    fn text(buffer: &[u16]) -> String {
        let length = buffer
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(buffer.len());
        String::from_utf16_lossy(&buffer[..length])
    }

    /// The last component of a Windows, NT (`\??\Z:\...`) or Unix path.
    fn basename(path: &str) -> &str {
        path.rsplit(['\\', '/']).next().unwrap_or(path)
    }

    /// Every process as `(pid, executable name)`.
    fn processes() -> Vec<(u32, String)> {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Vec::new();
        }
        let _snapshot = Handle(snapshot);
        let mut found = Vec::new();
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut more = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
        while more {
            found.push((entry.th32ProcessID, text(&entry.szExeFile)));
            more = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
        }
        found
    }

    /// The path of the main module of process `pid`, the first module its loader lists.
    fn main_module(pid: u32) -> Option<String> {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, pid) };
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        let _snapshot = Handle(snapshot);
        let mut entry: MODULEENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = size_of::<MODULEENTRY32W>() as u32;
        (unsafe { Module32FirstW(snapshot, &mut entry) } != 0).then(|| text(&entry.szExePath))
    }

    /// The pid of the first process whose executable is named `name` (case-insensitive). A
    /// process started from an unusual path (an NT path, or an image a launcher mapped from
    /// elsewhere) can list no executable name; its main module's name is checked then.
    pub fn find_process(name: &str) -> Option<u32> {
        let all = processes();
        all.iter()
            .find(|(_, exe)| exe.eq_ignore_ascii_case(name))
            .or_else(|| {
                all.iter()
                    .filter(|(_, exe)| exe.is_empty())
                    .find(|(pid, _)| {
                        main_module(*pid)
                            .is_some_and(|path| basename(&path).eq_ignore_ascii_case(name))
                    })
            })
            .map(|(pid, _)| *pid)
    }

    /// Load the DLL at the Windows path `dll` into process `pid`.
    pub fn inject(dll: &str, pid: u32) -> Result<(), Failure> {
        let access = PROCESS_CREATE_THREAD
            | PROCESS_QUERY_INFORMATION
            | PROCESS_VM_OPERATION
            | PROCESS_VM_READ
            | PROCESS_VM_WRITE;
        let process = unsafe { OpenProcess(access, 0, pid) };
        if process.is_null() {
            return Err((4, format!("could not open pid {pid}")));
        }
        let process = Handle(process);
        let path = wide(dll);
        let bytes = path.len() * size_of::<u16>();
        let remote = unsafe {
            VirtualAllocEx(
                process.0,
                std::ptr::null(),
                bytes,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        if remote.is_null() {
            return Err((5, "could not allocate the path in the target".into()));
        }
        let free =
            |remote: *mut c_void| unsafe { VirtualFreeEx(process.0, remote, 0, MEM_RELEASE) };
        let mut written = 0;
        if unsafe {
            WriteProcessMemory(process.0, remote, path.as_ptr().cast(), bytes, &mut written)
        } == 0
            || written != bytes
        {
            free(remote);
            return Err((5, "could not write the path into the target".into()));
        }
        // kernel32 is mapped at the same address in every process of a session.
        let kernel32 = unsafe { GetModuleHandleW(wide("kernel32.dll").as_ptr()) };
        let Some(load_library) =
            (unsafe { GetProcAddress(kernel32, c"LoadLibraryW".as_ptr().cast()) })
        else {
            free(remote);
            return Err((6, "LoadLibraryW is missing from kernel32".into()));
        };
        // SAFETY: LoadLibraryW takes one pointer argument, like a thread routine.
        let start: unsafe extern "system" fn(*mut c_void) -> u32 =
            unsafe { std::mem::transmute(load_library) };
        let thread = unsafe {
            CreateRemoteThread(
                process.0,
                std::ptr::null(),
                0,
                Some(start),
                remote,
                0,
                std::ptr::null_mut(),
            )
        };
        if thread.is_null() {
            free(remote);
            return Err((6, "could not start the remote loader thread".into()));
        }
        let thread = Handle(thread);
        if unsafe { WaitForSingleObject(thread.0, LOAD_TIMEOUT_MS) } != WAIT_OBJECT_0 {
            // The thread may still read the path; leaving it allocated avoids a use-after-free.
            return Err((8, "loading the DLL timed out".into()));
        }
        let mut module = 0u32;
        unsafe { GetExitCodeThread(thread.0, &mut module) };
        free(remote);
        if module == 0 {
            return Err((
                7,
                format!(
                    "LoadLibraryW could not load {dll}; check it is 64-bit and its imports exist"
                ),
            ));
        }
        Ok(())
    }
}
