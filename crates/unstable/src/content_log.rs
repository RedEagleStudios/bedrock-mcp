//! Live content log: every message the game hands its content-log file
//! endpoint, as it happens and before the file's own buffering.
//!
//! `ContentLogFileEndPoint` derives from `ContentLogEndPoint`, whose primary
//! vtable is `[dtor, log(LogArea, LogLevel, const char*), logOnlyOnce]` and
//! whose `Bedrock::LogEndPoint` subobject at +0x18 has five slots. Both
//! counts are checked before slot 1 is replaced. Layout reference: LeviLamina's
//! `src/mc/deps/core/debug/log/{ContentLogEndPoint,ContentLogFileEndPoint,LogEndPoint}.h`.

use std::{
    collections::VecDeque,
    ffi::{CStr, c_char, c_void},
    sync::{
        Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

use protocol::unstable::{CapabilityState, LogEntry};

use crate::{capability, vtable::Resolver};

const NAME: &str = "content_log";
const CLASS: &str = "ContentLogFileEndPoint";
const LOG_SLOT: usize = 1;
const CAPACITY: usize = 4096;
const MAX_MESSAGE: usize = 64 * 1024;
/// `LogLevel` has the values 0 to 3 (`Verbose`, `Inform`, `Warning`, `Error`; `Count` is 4) in
/// LeviLamina's `src/mc/deps/core/debug/log/LogLevel.h`; any other value fails the self-check.
const LEVELS: u32 = 4;

type Log = unsafe extern "system" fn(*mut c_void, u32, u32, *const c_char);

static ORIGINAL: AtomicUsize = AtomicUsize::new(0);
static SEQUENCE: AtomicU64 = AtomicU64::new(0);
static ENTRIES: Mutex<VecDeque<LogEntry>> = Mutex::new(VecDeque::new());

unsafe extern "system" fn log_hook(
    this: *mut c_void,
    area: u32,
    level: u32,
    message: *const c_char,
) {
    if !message.is_null() && level < LEVELS {
        // SAFETY: the game passes a NUL-terminated message.
        let bytes = unsafe { CStr::from_ptr(message) }.to_bytes();
        let bytes = &bytes[..bytes.len().min(MAX_MESSAGE)];
        // The code trims trailing line breaks.
        let text = String::from_utf8_lossy(bytes)
            .trim_end_matches(['\n', '\r'])
            .to_owned();
        let seq = SEQUENCE.fetch_add(1, Ordering::AcqRel) + 1;
        if let Ok(mut entries) = ENTRIES.lock() {
            entries.push_back(LogEntry {
                seq,
                area,
                level,
                message: text,
            });
            if entries.len() > CAPACITY {
                entries.pop_front();
            }
        }
        if seq == 1 {
            capability::set(
                NAME,
                CapabilityState::Active,
                format!("{CLASS}::log confirmed by a live message"),
            );
        }
    } else {
        capability::set(
            NAME,
            CapabilityState::Failed,
            format!("implausible call: level {level}, message {message:p}"),
        );
    }
    // SAFETY: stored from the slot this hook replaced.
    let original: Log = unsafe { std::mem::transmute(ORIGINAL.load(Ordering::Acquire)) };
    unsafe { original(this, area, level, message) };
}

pub fn install(resolver: &Resolver<'_>) {
    let result = resolver
        .table(CLASS, 0, 3)
        .and_then(|primary| resolver.table(CLASS, 0x18, 5).map(|_| primary))
        .and_then(|primary| {
            capability::found(NAME, &[(CLASS, primary.found)]);
            // Live endpoints may be wrapper templates with their own vtables.
            let tables = resolver.sharing(&primary, LOG_SLOT);
            if tables.is_empty() {
                return Err(format!("no vtable holds {CLASS}::log"));
            }
            for table in &tables {
                table.hook(LOG_SLOT, log_hook as Log as usize, &ORIGINAL)?;
            }
            Ok(tables.len())
        });
    match result {
        Ok(count) => capability::set(
            NAME,
            CapabilityState::Pending,
            format!("hooked {count} vtables; waiting for the first message"),
        ),
        Err(reason) => capability::set(NAME, CapabilityState::Unavailable, reason),
    }
}

/// Entries after `since`, at most `limit`, the next cursor and how many
/// entries after `since` were already dropped from the buffer.
pub fn read(since: u64, limit: usize) -> (Vec<LogEntry>, u64, u64) {
    let entries = ENTRIES.lock().unwrap_or_else(|poison| poison.into_inner());
    let oldest = entries
        .front()
        .map_or(SEQUENCE.load(Ordering::Acquire) + 1, |entry| entry.seq);
    let dropped = oldest.saturating_sub(since + 1);
    let batch: Vec<LogEntry> = entries
        .iter()
        .filter(|entry| entry.seq > since)
        .take(limit)
        .cloned()
        .collect();
    let next = batch
        .last()
        .map_or(since.max(oldest.saturating_sub(1)), |entry| entry.seq);
    (batch, next, dropped)
}
