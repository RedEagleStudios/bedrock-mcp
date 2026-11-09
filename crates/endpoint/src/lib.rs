//! Loopback control endpoint shared by the DLLs bedrock-mcp loads into Minecraft.
//!
//! A DLL listens on a random loopback port and writes an
//! [`protocol::Endpoint`] file next to itself. A client's first line must be
//! the service's hello with the endpoint token; after that every request
//! line gets one reply line. A new authenticated connection replaces the
//! current client, so a stuck controller cannot lock others out, and a
//! client that goes away has its effects released. Wine does not wake a
//! blocked `recv` when another thread shuts the socket down, so client
//! threads read with a short timeout and check whether they were replaced.

#![cfg(windows)]

pub mod log;

use std::{
    io::{self, ErrorKind, Read, Write},
    net::{Ipv4Addr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use protocol::{Endpoint, MAX_LINE};
use serde::{Serialize, de::DeserializeOwned};
use windows_sys::Win32::{
    Foundation::HMODULE,
    Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom},
    System::{LibraryLoader::GetModuleFileNameW, Threading::GetCurrentProcessId},
};

/// Work a request asks for after its reply is sent.
pub type After = Option<Box<dyn FnOnce() + Send>>;

const HELLO_TIMEOUT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(100);

/// What a DLL serves on its endpoint.
pub trait Service: Sync + 'static {
    type Request: DeserializeOwned;
    type Reply: Serialize;
    /// Protocol version the service speaks.
    const PROTOCOL: u32;
    /// The token and protocol of a hello request; `None` for anything else.
    fn hello(request: &Self::Request) -> Option<(&str, u32)>;
    fn ok(&self) -> Self::Reply;
    fn error(&self, message: String) -> Self::Reply;
    /// Handle one request. The action, if any, runs after the reply is sent.
    fn handle(&self, request: Self::Request) -> (Self::Reply, After);
    /// The client went away or was replaced: undo whatever it left active.
    fn release(&self);
    /// A client authenticated and is about to be served.
    fn attached(&self) {}
}

/// The directory of the DLL `module`.
///
/// # Safety
/// `module` must be a module handle of this process.
pub unsafe fn module_dir(module: HMODULE) -> Option<PathBuf> {
    let mut buffer = [0u16; 1024];
    let length = unsafe { GetModuleFileNameW(module, buffer.as_mut_ptr(), buffer.len() as u32) };
    if length == 0 || length as usize >= buffer.len() {
        return None;
    }
    let path = PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize]));
    path.parent().map(PathBuf::from)
}

pub fn pid() -> u32 {
    unsafe { GetCurrentProcessId() }
}

pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn token() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status != 0 {
        return Err(io::Error::other(format!(
            "BCryptGenRandom failed: {status:#x}"
        )));
    }
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn write_endpoint(dir: &Path, file: &str, endpoint: &Endpoint) -> io::Result<()> {
    let staged = dir.join(format!("{file}.tmp"));
    std::fs::write(&staged, protocol::encode(endpoint))?;
    std::fs::rename(&staged, dir.join(file))
}

/// Reads newline-terminated requests with a bounded line length.
struct Lines {
    stream: TcpStream,
    buffer: Vec<u8>,
}

/// What one read from a client produced.
enum Incoming {
    Line(String),
    Idle,
    Closed,
}

impl Lines {
    fn next(&mut self) -> Incoming {
        if let Some(end) = self.buffer.iter().position(|&byte| byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=end).collect();
            return Incoming::Line(String::from_utf8_lossy(&line).into_owned());
        }
        if self.buffer.len() >= MAX_LINE {
            return Incoming::Closed;
        }
        let mut chunk = [0u8; 1024];
        match self.stream.read(&mut chunk) {
            Ok(0) => Incoming::Closed,
            Ok(read) => {
                self.buffer.extend_from_slice(&chunk[..read]);
                self.next()
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                Incoming::Idle
            }
            Err(_) => Incoming::Closed,
        }
    }

    fn reply<T: Serialize>(&mut self, reply: &T) -> bool {
        self.stream
            .write_all(protocol::encode(reply).as_bytes())
            .is_ok()
    }
}

/// Read the hello within [`HELLO_TIMEOUT`]; true when the token and version
/// match. Failures are answered here; success is answered by the caller once
/// the previous client has stopped.
fn authenticate<S: Service>(service: &S, lines: &mut Lines, token: &str) -> bool {
    let deadline = Instant::now() + HELLO_TIMEOUT;
    while Instant::now() < deadline {
        match lines.next() {
            Incoming::Line(line) => {
                let error = match protocol::decode::<S::Request>(&line)
                    .ok()
                    .as_ref()
                    .and_then(S::hello)
                {
                    Some((given, protocol)) if given == token && protocol == S::PROTOCOL => {
                        return true;
                    }
                    Some((_, protocol)) if protocol != S::PROTOCOL => {
                        format!(
                            "protocol {protocol} is not supported; this endpoint speaks {}",
                            S::PROTOCOL
                        )
                    }
                    _ => "the first request must be hello with the endpoint token".to_owned(),
                };
                lines.reply(&service.error(error));
                return false;
            }
            Incoming::Idle => continue,
            Incoming::Closed => return false,
        }
    }
    false
}

fn serve_client<S: Service>(
    service: &'static S,
    mut lines: Lines,
    generation: &'static AtomicU64,
    mine: u64,
) {
    let replaced = || generation.load(Ordering::Acquire) != mine;
    loop {
        match lines.next() {
            Incoming::Line(_) if replaced() => break,
            Incoming::Line(line) => {
                let (reply, after) = match protocol::decode::<S::Request>(&line) {
                    Ok(request) => service.handle(request),
                    Err(error) => (service.error(format!("bad request: {error}")), None),
                };
                if !lines.reply(&reply) {
                    break;
                }
                if let Some(after) = after {
                    let _ = lines.stream.flush();
                    after();
                }
            }
            Incoming::Idle if !replaced() => {}
            Incoming::Idle | Incoming::Closed => break,
        }
    }
    service.release();
}

/// Listen on loopback, publish `endpoint_file` in `dir` and serve forever.
pub fn serve<S: Service>(service: &'static S, dir: &Path, endpoint_file: &str) -> io::Result<()> {
    static GENERATION: AtomicU64 = AtomicU64::new(0);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let endpoint = Endpoint {
        port: listener.local_addr()?.port(),
        token: token()?,
        pid: pid(),
    };
    write_endpoint(dir, endpoint_file, &endpoint)?;
    crate::log!("listening on 127.0.0.1:{}", endpoint.port);
    let mut current: Option<JoinHandle<()>> = None;
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if stream.set_read_timeout(Some(POLL)).is_err() {
            continue;
        }
        let mut lines = Lines {
            stream,
            buffer: Vec::new(),
        };
        if !authenticate(service, &mut lines, &endpoint.token) {
            continue;
        }
        // Stop the previous client first; it releases its effects before the
        // new client is told it is connected.
        let generation = GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
        if let Some(previous) = current.take() {
            let _ = previous.join();
        }
        if !lines.reply(&service.ok()) {
            continue;
        }
        service.attached();
        current = std::thread::Builder::new()
            .name("bedrock-endpoint-client".into())
            .spawn(move || serve_client(service, lines, &GENERATION, generation))
            .ok();
    }
    Ok(())
}
