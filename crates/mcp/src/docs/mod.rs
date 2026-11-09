//! Offline helpers for add-on work that run without the game: Minecraft Creator Tools for
//! pack checks and fixes, the Bedrock JSON schemas, and Molang evaluation. Each runs a
//! JavaScript package through Node.js or Bun as a subprocess; packages are fetched once
//! into this server's cache folder.

pub mod mct;
pub mod molang;
pub mod schemas;

use std::{
    collections::BTreeMap,
    env,
    ffi::{OsStr, OsString},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU32, Ordering},
    thread,
    time::{Duration, Instant},
};

use crate::error::{Error, Result, bail};

/// How long a package download may take.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(300);

/// This server's cache folder: `${XDG_CACHE_HOME:-~/.cache}/bedrock-mcp` on Linux,
/// `%LOCALAPPDATA%\bedrock-mcp\cache` on Windows.
pub fn cache_dir() -> Result<PathBuf> {
    cache_dir_from(cfg!(windows), |name| {
        env::var_os(name).filter(|value| !value.is_empty())
    })
}

fn cache_dir_from(windows: bool, var: impl Fn(&str) -> Option<OsString>) -> Result<PathBuf> {
    let found = if windows {
        var("LOCALAPPDATA").map(|dir| PathBuf::from(dir).join("bedrock-mcp").join("cache"))
    } else {
        var("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| var("HOME").map(|home| PathBuf::from(home).join(".cache")))
            .map(|dir| dir.join("bedrock-mcp"))
    };
    found.ok_or_else(|| {
        let names = if windows {
            "LOCALAPPDATA"
        } else {
            "XDG_CACHE_HOME or HOME"
        };
        Error::msg(format!("No cache folder: set {names}."))
    })
}

/// The first `name` program on PATH (on Windows also `name.exe`, `name.cmd`, `name.bat`).
pub fn which(name: &str) -> Option<PathBuf> {
    let suffixes: &[&str] = if cfg!(windows) {
        &[".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    env::split_paths(&env::var_os("PATH")?)
        .flat_map(|dir| {
            suffixes
                .iter()
                .map(move |suffix| dir.join(format!("{name}{suffix}")))
        })
        .find(|path| path.is_file())
}

/// A finished subprocess.
#[derive(Debug)]
pub struct Ran {
    /// None when a signal ended it.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Ran {
    /// The last lines of stderr, without Node's experimental-feature warnings.
    pub fn stderr_tail(&self) -> String {
        let lines: Vec<&str> = self
            .stderr
            .lines()
            .filter(|line| {
                !line.trim().is_empty()
                    && !line.contains("ExperimentalWarning")
                    && !line.contains("--trace-warnings")
            })
            .collect();
        lines[lines.len().saturating_sub(12)..].join("\n")
    }
}

/// Run `program args` in `cwd`, feeding `input` to stdin, and kill it after `timeout`.
pub fn run(
    program: &Path,
    args: &[OsString],
    cwd: &Path,
    input: Option<&[u8]>,
    timeout: Duration,
) -> Result<Ran> {
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env("NO_COLOR", "1")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| Error::msg(format!("Cannot run {}: {error}", program.display())))?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin.write_all(input)?;
    }
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            String::from_utf8_lossy(&bytes).into_owned()
        })
    };
    let stdout = drain(child.stdout.take().map(|pipe| Box::new(pipe) as _));
    let stderr = drain(child.stderr.take().map(|pipe| Box::new(pipe) as _));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "{} did not finish within {} s and was stopped.",
                program.display(),
                timeout.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(25));
    };
    Ok(Ran {
        code: status.code(),
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// A temporary folder, removed on drop.
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn new(label: &str) -> Result<Self> {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = env::temp_dir().join(format!(
            "bedrock-mcp-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path)?;
        Ok(Self(path))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The program that installs npm packages: npm, else Bun.
fn installer() -> Result<(PathBuf, bool)> {
    which("npm")
        .map(|npm| (npm, true))
        .or_else(|| which("bun").map(|bun| (bun, false)))
        .ok_or_else(|| {
            Error::msg(
                "Fetching packages needs npm (part of Node.js) or Bun on PATH; neither was found.",
            )
        })
}

/// The arguments that install `spec` (`name@version`) into the current folder.
fn install_args(npm: bool, spec: &str) -> Vec<OsString> {
    let args: &[&str] = if npm {
        &[
            "install",
            "--no-save",
            "--no-package-lock",
            "--ignore-scripts",
            "--no-audit",
            "--no-fund",
        ]
    } else {
        &["add", "--no-save", "--ignore-scripts"]
    };
    args.iter()
        .map(OsString::from)
        .chain([OsString::from(spec)])
        .collect()
}

/// Where `name@version` lives once fetched into `cache`.
pub fn package_dir(cache: &Path, name: &str, version: &str) -> PathBuf {
    cache
        .join("packages")
        .join(format!("{}@{version}", name.replace('/', "+")))
        .join("node_modules")
        .join(name)
}

/// The folder of npm package `name@version` in the cache, fetching it on first use.
pub fn ensure_package(name: &str, version: &str) -> Result<PathBuf> {
    let cache = cache_dir()?;
    let dir = package_dir(&cache, name, version);
    if dir.join("package.json").is_file() {
        return Ok(dir);
    }
    let (program, npm) = installer()?;
    let home = cache
        .join("packages")
        .join(format!("{}@{version}", name.replace('/', "+")));
    let staging = home.with_extension(format!("staging-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;
    fs::write(staging.join("package.json"), "{}\n")?;
    let spec = format!("{name}@{version}");
    let ran = run(
        &program,
        &install_args(npm, &spec),
        &staging,
        None,
        INSTALL_TIMEOUT,
    );
    let installed = staging.join("node_modules").join(name).join("package.json");
    match ran {
        Ok(ran) if ran.code == Some(0) && installed.is_file() => {}
        Ok(ran) => {
            let _ = fs::remove_dir_all(&staging);
            bail!(
                "Fetching {spec} with {} failed (exit {:?}): {}",
                program.display(),
                ran.code,
                ran.stderr_tail()
            );
        }
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
    }
    // Another server may have finished the same download first; either copy will do.
    if fs::rename(&staging, &home).is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    if dir.join("package.json").is_file() {
        Ok(dir)
    } else {
        bail!("{spec} was fetched but {} is missing.", dir.display())
    }
}

/// The published versions of npm package `name`, oldest first.
pub fn package_versions(name: &str) -> Result<Vec<String>> {
    let (program, npm) = installer()?;
    let scratch = Scratch::new("versions")?;
    fs::write(scratch.path().join("package.json"), "{}\n")?;
    let verb = if npm { "view" } else { "info" };
    let args: Vec<OsString> = [verb, name, "versions", "--json"]
        .into_iter()
        .map(OsString::from)
        .collect();
    let ran = run(
        &program,
        &args,
        scratch.path(),
        None,
        Duration::from_secs(60),
    )?;
    if ran.code != Some(0) {
        bail!(
            "Listing {name} versions failed (exit {:?}): {}",
            ran.code,
            ran.stderr_tail()
        );
    }
    parse_versions(&ran.stdout).ok_or_else(|| {
        Error::msg(format!(
            "Unexpected version list for {name}: {}",
            ran.stdout.trim()
        ))
    })
}

fn parse_versions(text: &str) -> Option<Vec<String>> {
    match serde_json::from_str::<serde_json::Value>(text.trim()).ok()? {
        serde_json::Value::Array(items) => items
            .into_iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect(),
        serde_json::Value::String(one) => Some(vec![one]),
        _ => None,
    }
}

/// The major version `node --version` printed, e.g. 22 for `v22.3.0`.
pub fn node_major(output: &str) -> Option<u32> {
    output
        .trim()
        .strip_prefix('v')?
        .split('.')
        .next()?
        .parse()
        .ok()
}

/// The major version of the `node` at `node`.
pub fn node_version(node: &Path) -> Result<(u32, String)> {
    let scratch = env::temp_dir();
    let ran = run(
        node,
        &[OsString::from("--version")],
        &scratch,
        None,
        Duration::from_secs(20),
    )?;
    let text = ran.stdout.trim().to_owned();
    node_major(&text)
        .map(|major| (major, text.clone()))
        .ok_or_else(|| Error::msg(format!("Unexpected `node --version` output: {text}")))
}

/// Every `.json` file under `root` by its `/`-separated relative path.
pub fn json_files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, found: &mut BTreeMap<String, Vec<u8>>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, found);
            } else if path.extension() == Some(OsStr::new("json"))
                && let (Ok(relative), Ok(bytes)) = (path.strip_prefix(root), fs::read(&path))
            {
                let key: Vec<_> = relative
                    .components()
                    .map(|part| part.as_os_str().to_string_lossy())
                    .collect();
                found.insert(key.join("/"), bytes);
            }
        }
    }
    let mut found = BTreeMap::new();
    walk(root, root, &mut found);
    found
}

/// Copy the folder `from` to `to`, which must not exist yet.
pub fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cache_folder_follows_the_platform_convention() {
        let vars = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        assert_eq!(
            cache_dir_from(false, vars(&[("XDG_CACHE_HOME", "/x"), ("HOME", "/h")])).unwrap(),
            PathBuf::from("/x/bedrock-mcp")
        );
        assert_eq!(
            cache_dir_from(false, vars(&[("HOME", "/h")])).unwrap(),
            PathBuf::from("/h/.cache/bedrock-mcp")
        );
        assert_eq!(
            cache_dir_from(true, vars(&[("LOCALAPPDATA", "L")])).unwrap(),
            PathBuf::from("L").join("bedrock-mcp").join("cache")
        );
        let error = cache_dir_from(true, vars(&[("HOME", "/h")])).unwrap_err();
        assert!(error.to_string().contains("LOCALAPPDATA"), "{error}");
    }

    #[test]
    fn node_versions_are_read_from_the_version_banner() {
        assert_eq!(node_major("v26.13.0\n"), Some(26));
        assert_eq!(node_major("v20.11.1"), Some(20));
        assert_eq!(node_major("26.1.0"), None);
        assert_eq!(node_major(""), None);
    }

    #[test]
    fn packages_install_without_scripts_into_a_per_version_folder() {
        let npm = install_args(true, "molangjs@1.7.1");
        assert_eq!(npm.first().unwrap(), "install");
        assert!(npm.iter().any(|arg| arg == "--ignore-scripts"));
        assert_eq!(npm.last().unwrap(), "molangjs@1.7.1");
        let bun = install_args(false, "molangjs@1.7.1");
        assert_eq!(
            bun[..2],
            [OsString::from("add"), OsString::from("--no-save")]
        );
        assert_eq!(
            package_dir(Path::new("/c"), "@minecraft/bedrock-schemas", "1.26.60"),
            PathBuf::from(
                "/c/packages/@minecraft+bedrock-schemas@1.26.60/node_modules/@minecraft/bedrock-schemas"
            )
        );
        assert_eq!(
            parse_versions("[\"1.0.0\", \"1.1.0-beta.2\"]").unwrap(),
            ["1.0.0", "1.1.0-beta.2"]
        );
        assert_eq!(parse_versions("\"2.0.0\"").unwrap(), ["2.0.0"]);
        assert!(parse_versions("npm ERR!").is_none());
    }

    #[test]
    fn subprocess_output_and_timeouts_are_reported() {
        if cfg!(windows) {
            return;
        }
        let dir = env::temp_dir();
        let sh = Path::new("/bin/sh");
        let args = |script: &str| vec![OsString::from("-c"), OsString::from(script)];
        let ran = run(
            sh,
            &args("cat; echo err >&2; exit 3"),
            &dir,
            Some(b"in"),
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!((ran.code, ran.stdout.as_str()), (Some(3), "in"));
        assert_eq!(ran.stderr_tail(), "err");
        let error = run(sh, &args("sleep 5"), &dir, None, Duration::from_millis(200)).unwrap_err();
        assert!(error.to_string().contains("did not finish"), "{error}");
    }
}
