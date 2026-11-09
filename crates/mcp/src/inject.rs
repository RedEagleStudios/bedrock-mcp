//! Loading the agent DLL into the running game and connecting to it. The platform module
//! does the injection itself.

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use protocol::{DLL_NAME, LOG_FILE};

use crate::{
    agent::{AgentConnection, try_connect},
    clock::{Clock, poll},
    error::{Error, Result, bail},
    game_files::menu_reached,
    install::{self, Install},
    platform::{self, CRASH_HANDLER},
};

/// The agent may wait for the game window before it listens.
const ENDPOINT_TIMEOUT: Duration = Duration::from_secs(40);
/// A running game that never writes its main-menu markers is injected after this long.
const MENU_CEILING: Duration = Duration::from_secs(150);
const POLL: Duration = Duration::from_millis(200);

/// Copy `source` to `dir/<name>` unless the same bytes are already there. The copy replaces
/// the old file by rename, so a game that maps the previous DLL keeps its inode.
pub fn stage_dll(source: &Path, dir: &Path, name: &str) -> Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let target = dir.join(name);
    let wanted = fs::read(source)
        .map_err(|error| Error::msg(format!("Cannot read {}: {error}", source.display())))?;
    if fs::read(&target).is_ok_and(|current| current == wanted) {
        return Ok(target);
    }
    let staged = dir.join(format!(".{name}.staging"));
    fs::write(&staged, &wanted)?;
    // Windows refuses to replace a DLL that a running process has loaded.
    fs::rename(&staged, &target).map_err(|error| {
        let _ = fs::remove_file(&staged);
        Error::msg(format!(
            "Cannot replace {} with the new build ({error}); a running game still has the old one loaded, so quit it first.",
            target.display()
        ))
    })?;
    Ok(target)
}

/// Connect to the agent in the running game, injecting it first when none answers.
/// Injection waits for the main menu, since agents loaded earlier lose mouse input.
pub fn attach(install: &Install, clock: &dyn Clock) -> Result<AgentConnection> {
    let dir = install.agent_dir();
    if let Some(connection) = try_connect(&dir)? {
        return Ok(connection);
    }
    let Some(pid) = install.game_pid() else {
        bail!("Minecraft is not running; call game_launch first.");
    };
    if install.crash_handler_running() {
        bail!("Minecraft is stopped in {CRASH_HANDLER}; quit and launch it again.");
    }
    if let Some(started) = platform::started_at(pid) {
        poll(clock, MENU_CEILING, POLL, || {
            if !platform::alive(pid) {
                bail!("Minecraft exited before reaching its main menu.");
            }
            Ok(menu_reached(&install.prefix, started).then_some(()))
        })?;
    }
    inject_and_connect(
        install,
        pid,
        DLL_NAME,
        LOG_FILE,
        ENDPOINT_TIMEOUT,
        clock,
        || try_connect(&dir),
    )
}

/// Stage `dll_name` from the Windows artifacts, inject it into `pid`, and poll `connect`
/// until its endpoint answers, the game dies, or `timeout` passes.
pub fn inject_and_connect<T>(
    install: &Install,
    pid: u32,
    dll_name: &str,
    log_file: &str,
    timeout: Duration,
    clock: &dyn Clock,
    mut connect: impl FnMut() -> Result<Option<T>>,
) -> Result<T> {
    let dir = install.agent_dir();
    let log = dir.join(log_file);
    let dll = stage_dll(&install::windows_artifact(dll_name)?, &dir, dll_name)?;
    platform::inject(install, pid, &dll, clock)?;
    let connection = poll(clock, timeout, POLL, || {
        if install.crash_handler_running() {
            bail!(
                "Minecraft crashed right after {dll_name} loaded. Details: {}",
                log.display()
            );
        }
        if !platform::alive(pid) {
            bail!(
                "Minecraft exited right after {dll_name} loaded. Details: {}",
                log.display()
            );
        }
        connect()
    })?;
    connection.ok_or_else(|| {
        Error::msg(format!(
            "{dll_name} loaded but never opened its endpoint. Details: {}",
            log.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir::TestDir;

    #[test]
    fn staging_copies_only_changed_dlls() {
        let dir = TestDir::new("stage");
        let source = dir.path().join("build.dll");
        fs::write(&source, b"one").unwrap();
        let agent = dir.path().join("agent");
        let target = stage_dll(&source, &agent, "bedrock_agent.dll").unwrap();
        assert_eq!(target, agent.join("bedrock_agent.dll"));
        let first = fs::metadata(&target).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        stage_dll(&source, &agent, "bedrock_agent.dll").unwrap();
        assert_eq!(
            fs::metadata(&target).unwrap().modified().unwrap(),
            first,
            "identical copy kept"
        );
        fs::write(&source, b"two").unwrap();
        stage_dll(&source, &agent, "bedrock_agent.dll").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"two");
        assert!(!agent.join(".bedrock_agent.dll.staging").exists());
    }

    /// A game that maps the previous DLL keeps its inode.
    #[cfg(unix)]
    #[test]
    fn staging_replaces_changed_dlls_by_rename_only() {
        use std::os::unix::fs::MetadataExt;
        let dir = TestDir::new("stage-inode");
        let source = dir.path().join("build.dll");
        fs::write(&source, b"one").unwrap();
        let agent = dir.path().join("agent");
        let target = stage_dll(&source, &agent, "bedrock_agent.dll").unwrap();
        let first = fs::metadata(&target).unwrap().ino();
        fs::write(&source, b"two").unwrap();
        stage_dll(&source, &agent, "bedrock_agent.dll").unwrap();
        assert_ne!(fs::metadata(&target).unwrap().ino(), first);
    }
}
