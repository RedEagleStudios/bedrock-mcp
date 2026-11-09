//! The game installation: where its files and this server's files live. Process discovery,
//! launch and stop are in the platform module's `impl Install`.

use std::{
    env,
    path::{Path, PathBuf},
};

use crate::{
    error::{Error, Result},
    platform,
};

pub const GAME_EXE: &str = "minecraft.windows.exe";

/// Paths of one installation.
#[derive(Clone, Debug)]
pub struct Install {
    /// This server's folder for the game: the staged agent, its endpoint file, logs and
    /// captures live in `profile/agent`. Linux: the mcbe-gdk-linux profile for its prefix,
    /// else a folder per prefix under this server's data folder. Windows:
    /// `%LOCALAPPDATA%\bedrock-mcp`.
    pub profile: PathBuf,
    /// Where the game's `Minecraft Bedrock` data folders are found (see
    /// [`platform::roaming_dirs`]). Linux: the Wine prefix. Windows: `%APPDATA%`.
    pub prefix: PathBuf,
}

impl Install {
    /// Where the staged agent DLL, its endpoint file, its log and captures live.
    pub fn agent_dir(&self) -> PathBuf {
        self.profile.join("agent")
    }
}

/// The Windows build outputs: `$BEDROCK_MCP_WINDOWS_DIR`, this executable's directory, then
/// the platform's build directories next to it.
pub fn windows_artifact(name: &str) -> Result<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = env::var_os("BEDROCK_MCP_WINDOWS_DIR").filter(|dir| !dir.is_empty()) {
        dirs.push(PathBuf::from(dir));
    }
    if let Some(exe_dir) = env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    {
        let builds = platform::ARTIFACT_DIRS.iter().map(|dir| exe_dir.join(dir));
        dirs.extend(std::iter::once(exe_dir.clone()).chain(builds));
    }
    find_artifact(name, &dirs)
}

fn find_artifact(name: &str, dirs: &[PathBuf]) -> Result<PathBuf> {
    dirs.iter()
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
        .ok_or_else(|| {
            let searched: Vec<_> = dirs.iter().map(|dir| dir.display().to_string()).collect();
            Error::msg(format!(
                "{name} is missing; {} or set BEDROCK_MCP_WINDOWS_DIR. Searched: {}",
                platform::BUILD_HINT,
                searched.join(", ")
            ))
        })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn artifacts_are_taken_from_the_first_directory_that_has_them() {
        let temp = crate::testdir::TestDir::new("artifacts");
        let (first, second) = (temp.path().join("a"), temp.path().join("b"));
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::write(second.join("x.dll"), b"").unwrap();
        assert_eq!(
            find_artifact("x.dll", &[first.clone(), second.clone()]).unwrap(),
            second.join("x.dll")
        );
        fs::write(first.join("x.dll"), b"").unwrap();
        assert_eq!(
            find_artifact("x.dll", &[first.clone(), second]).unwrap(),
            first.join("x.dll")
        );
        assert!(find_artifact("y.exe", &[first]).is_err());
    }
}
