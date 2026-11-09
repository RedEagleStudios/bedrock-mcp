//! The Bridge and GameTest behavior packs built into this server (see `build.rs`), and
//! installing them into the game's development behavior packs.

use std::{fs, io, path::Path};

use serde_json::{Value, json};

use crate::error::{Error, Result};

/// One built behavior pack: its export folder name and its files by relative path.
pub struct EmbeddedPack {
    pub folder: &'static str,
    pub files: &'static [(&'static str, &'static [u8])],
}

include!(concat!(env!("OUT_DIR"), "/embedded_packs.rs"));

/// How an installed copy compares with the embedded pack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Missing,
    Outdated,
    Current,
}

impl Status {
    pub fn name(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Outdated => "outdated",
            Self::Current => "current",
        }
    }
}

impl EmbeddedPack {
    /// The manifest header's name, uuid and version.
    pub fn header(&self) -> Value {
        let manifest = self
            .files
            .iter()
            .find(|(path, _)| *path == "manifest.json")
            .and_then(|(_, data)| serde_json::from_slice::<Value>(data).ok())
            .unwrap_or(Value::Null);
        let header = &manifest["header"];
        json!({ "uuid": header["uuid"], "version": header["version"] })
    }

    /// Whether `dir` (the pack's folder in development_behavior_packs) holds these files.
    pub fn status(&self, dir: &Path) -> Status {
        if !dir.join("manifest.json").is_file() {
            return Status::Missing;
        }
        let same = self
            .files
            .iter()
            .all(|(path, data)| fs::read(dir.join(path)).is_ok_and(|installed| installed == *data));
        if same {
            Status::Current
        } else {
            Status::Outdated
        }
    }

    /// Replace `dir` with these files: write them beside it, then swap the folders.
    pub fn install(&self, dir: &Path) -> Result<()> {
        let parent = dir
            .parent()
            .ok_or_else(|| Error::msg(format!("{} has no parent folder.", dir.display())))?;
        fs::create_dir_all(parent)?;
        let staging = parent.join(format!(".{}.staging", self.folder));
        remove_dir(&staging)?;
        for (path, data) in self.files {
            let target = staging.join(path);
            if let Some(folder) = target.parent() {
                fs::create_dir_all(folder)?;
            }
            fs::write(target, data)?;
        }
        remove_dir(dir)?;
        fs::rename(&staging, dir)?;
        Ok(())
    }
}

fn remove_dir(dir: &Path) -> Result<()> {
    match fs::remove_dir_all(dir) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir::TestDir;

    static PACK: EmbeddedPack = EmbeddedPack {
        folder: "Test_bp",
        files: &[
            (
                "manifest.json",
                br#"{"header": {"uuid": "u-1", "version": [1, 2, 3]}}"#,
            ),
            ("scripts/main.js", b"console.log(1);"),
        ],
    };

    #[test]
    fn installing_replaces_an_outdated_copy_and_then_reads_as_current() {
        let dir = TestDir::new("embedded");
        let target = dir
            .path()
            .join("development_behavior_packs")
            .join(PACK.folder);
        assert_eq!(PACK.status(&target), Status::Missing);
        fs::create_dir_all(target.join("scripts")).unwrap();
        fs::write(target.join("manifest.json"), b"{}").unwrap();
        fs::write(target.join("old.js"), b"stale").unwrap();
        assert_eq!(PACK.status(&target), Status::Outdated);
        PACK.install(&target).unwrap();
        assert_eq!(PACK.status(&target), Status::Current);
        assert!(
            !target.join("old.js").exists(),
            "files the pack dropped are removed"
        );
        assert_eq!(
            fs::read(target.join("scripts/main.js")).unwrap(),
            b"console.log(1);"
        );
        assert_eq!(PACK.header()["version"], json!([1, 2, 3]));
    }
}
