//! A scratch folder and a log for every server a call starts, named by process id and a
//! per-process counter so calls from several MCP servers never share one. Scratch folders go
//! after their server stops; logs stay, the newest few per kind.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime},
};

/// Logs kept per kind; older ones are deleted when a call ends.
pub const KEEP_LOGS: usize = 40;
/// Scratch folders this old belong to a server process that died without cleaning up.
const STALE_SCRATCH: Duration = Duration::from_secs(24 * 60 * 60);

static NEXT: AtomicU64 = AtomicU64::new(1);

/// What a server run is for; it names the folder and the log.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Gametest,
    Check,
}

impl Kind {
    fn prefix(self) -> &'static str {
        match self {
            Self::Gametest => "gametest",
            Self::Check => "check",
        }
    }
}

/// The folders under the agent folder.
#[derive(Clone, Debug)]
pub struct RunDirs {
    scratch: PathBuf,
    logs: PathBuf,
}

/// One server's scratch folder and log.
#[derive(Clone, Debug, PartialEq)]
pub struct RunDir {
    pub scratch: PathBuf,
    pub log: PathBuf,
}

impl RunDirs {
    pub fn new(agent_dir: &Path) -> Self {
        Self {
            scratch: agent_dir.join("bds-scratch"),
            logs: agent_dir.join("bds-logs"),
        }
    }

    /// A new `<kind>-<pid>-<n>` scratch folder path and log path; nothing is created yet.
    pub fn next(&self, kind: Kind) -> RunDir {
        let name = format!(
            "{}-{}-{}",
            kind.prefix(),
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        RunDir {
            scratch: self.scratch.join(&name),
            log: self.logs.join(format!("{name}.log")),
        }
    }

    /// Delete all but the newest [`KEEP_LOGS`] logs of `kind`, and scratch folders left by
    /// servers that died a day or more ago.
    pub fn tidy(&self, kind: Kind) {
        prune_logs(&self.logs, kind.prefix(), KEEP_LOGS);
        let Ok(entries) = fs::read_dir(&self.scratch) else {
            return;
        };
        let now = SystemTime::now();
        for entry in entries.flatten() {
            let old = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|modified| {
                    now.duration_since(modified).unwrap_or_default() >= STALE_SCRATCH
                });
            if old {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }
}

fn prune_logs(dir: &Path, prefix: &str, keep: usize) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut logs: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with(&format!("{prefix}-")) && name.ends_with(".log")
        })
        .filter_map(|entry| {
            let modified = entry.metadata().and_then(|metadata| metadata.modified());
            Some((modified.ok()?, entry.path()))
        })
        .collect();
    logs.sort_by(|a, b| b.cmp(a));
    for (_, path) in logs.into_iter().skip(keep) {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir::TestDir;

    #[test]
    fn every_run_gets_its_own_folder_and_log() {
        let dir = TestDir::new("bds-dirs");
        let dirs = RunDirs::new(dir.path());
        let first = dirs.next(Kind::Gametest);
        let second = dirs.next(Kind::Gametest);
        let check = dirs.next(Kind::Check);
        assert_ne!(first, second);
        let pid = std::process::id().to_string();
        let name = first
            .scratch
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(name.starts_with(&format!("gametest-{pid}-")), "{name}");
        assert_eq!(
            first.log.file_name().unwrap().to_string_lossy(),
            format!("{name}.log")
        );
        assert!(
            check
                .log
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("check-")
        );
        assert_ne!(first.scratch.parent(), first.log.parent());
    }

    #[test]
    fn pruning_keeps_the_newest_logs_of_one_kind() {
        let dir = TestDir::new("bds-prune");
        for n in 0..5 {
            fs::write(dir.path().join(format!("gametest-1-{n}.log")), "x").unwrap();
            std::thread::sleep(Duration::from_millis(15));
        }
        fs::write(dir.path().join("check-1-9.log"), "x").unwrap();
        prune_logs(dir.path(), "gametest", 2);
        let mut left: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            ["check-1-9.log", "gametest-1-3.log", "gametest-1-4.log"]
        );
    }
}
