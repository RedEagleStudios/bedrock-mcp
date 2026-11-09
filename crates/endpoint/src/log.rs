//! A DLL's log file next to it.

use std::{
    fs::File,
    io::Write,
    path::Path,
    sync::{Mutex, OnceLock},
    time::Instant,
};

static LOG: OnceLock<(Mutex<File>, Instant)> = OnceLock::new();

/// Start a fresh log; later [`write`] calls append to it.
pub fn open(path: &Path) {
    if let Ok(file) = File::create(path) {
        let _ = LOG.set((Mutex::new(file), Instant::now()));
    }
}

/// Append one line, prefixed with milliseconds since the agent started.
pub fn write(message: std::fmt::Arguments<'_>) {
    if let Some((file, start)) = LOG.get()
        && let Ok(mut file) = file.lock()
    {
        let _ = writeln!(file, "[{}] {message}", start.elapsed().as_millis());
    }
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::log::write(format_args!($($arg)*)) };
}
