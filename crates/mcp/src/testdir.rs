//! Unique scratch directories for tests, removed on drop.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
};

pub struct TestDir(PathBuf);

impl TestDir {
    pub fn new(name: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "bedrock-mcp-test-{}-{name}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A roaming AppData folder in the installation at `prefix`, laid out as the platform's
/// [`crate::platform::roaming_dirs`] finds it.
pub fn roaming(prefix: &Path) -> PathBuf {
    if cfg!(windows) {
        prefix.to_path_buf()
    } else {
        prefix.join("drive_c/users/steamuser/AppData/Roaming")
    }
}
