//! Minecraft's own files: data folders, options.txt, the log filter options, log files, and
//! the main-menu markers.

use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use serde_json::{Value, json};

use crate::{
    error::{Error, Result, bail},
    platform,
};

/// The data folders under the roaming app data folder, for the release and for Preview, as
/// https://learn.microsoft.com/en-us/minecraft/creator/documents/gdkpcprojectfolder lists
/// them. The same page names `Users/Shared/games/com.mojang` for packs and worlds, and `logs`
/// for logs.
const DATA_DIRS: [&str; 2] = ["Minecraft Bedrock", "Minecraft Bedrock Preview"];
const OPTIONS: &str = "options.txt";
/// Bedrock's log priorities, most verbose first.
pub const LOG_PRIORITIES: [&str; 4] = ["VERBOSE", "INFO", "WARN", "ERROR"];
/// Files the game writes while it starts up, before the main menu shows (checked on the public
/// 1.26.62 build); launch waits for them as one sign the menu is near.
const MENU_MARKERS: [&str; 2] = [
    "Flighting/currentTreatments",
    "bootstrapStorage/bootstrap_settings.json",
];
/// Options the WebSocket connection needs.
pub const WEBSOCKET_OPTIONS: [(&str, &str); 2] =
    [("websockets_enabled", "1"), ("websocket_encryption", "0")];

fn subdirs(path: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    found.sort();
    found
}

/// Every distinct `Minecraft Bedrock` data folder of the installation at `prefix`,
/// deduplicated by realpath.
pub fn data_roots(prefix: &Path) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut found = Vec::new();
    for roaming in platform::roaming_dirs(prefix) {
        for name in DATA_DIRS {
            let root = roaming.join(name);
            if let Ok(real) = fs::canonicalize(&root)
                && real.is_dir()
                && seen.insert(real)
            {
                found.push(root);
            }
        }
    }
    found
}

fn mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|meta| meta.modified()).ok()
}

/// The path with the newest modification time.
pub fn newest(paths: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    paths
        .into_iter()
        .filter_map(|path| Some((mtime(&path)?, path)))
        .max()
        .map(|(_, path)| path)
}

/// The signed-in user's `minecraftpe` folder. The code looks for
/// `Users/<id>/games/com.mojang/minecraftpe/options.txt` in each data folder and takes the
/// newest.
pub fn user_dir(prefix: &Path) -> Result<PathBuf> {
    let options = data_roots(prefix).into_iter().flat_map(|root| {
        subdirs(&root.join("Users"))
            .into_iter()
            .map(|user| user.join("games/com.mojang/minecraftpe").join(OPTIONS))
    });
    newest(options)
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .ok_or_else(|| Error::msg("Minecraft has no options.txt yet; launch it once first."))
}

pub fn options_path(prefix: &Path) -> Result<PathBuf> {
    Ok(user_dir(prefix)?.join(OPTIONS))
}

/// The shared `com.mojang` folder holding packs and worlds.
pub fn shared_dir(prefix: &Path) -> Result<PathBuf> {
    data_roots(prefix)
        .into_iter()
        .map(|root| root.join("Users/Shared/games/com.mojang"))
        .find(|path| path.is_dir())
        .ok_or_else(|| {
            Error::msg("Minecraft's shared data folder is missing; launch it once first.")
        })
}

/// Files in every data root's `logs/` whose names match, newest first.
fn logs(prefix: &Path, matches: impl Fn(&str) -> bool) -> Vec<PathBuf> {
    let mut found: Vec<(SystemTime, PathBuf)> = data_roots(prefix)
        .into_iter()
        .flat_map(|root| {
            fs::read_dir(root.join("logs"))
                .into_iter()
                .flatten()
                .flatten()
        })
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(&matches)
        })
        .filter_map(|path| Some((mtime(&path)?, path)))
        .collect();
    found.sort_by(|a, b| b.cmp(a));
    found.into_iter().map(|(_, path)| path).collect()
}

/// The game's most recently written log file, of any kind.
pub fn newest_log(prefix: &Path) -> Option<PathBuf> {
    logs(prefix, |_| true).into_iter().next()
}

pub fn content_logs(prefix: &Path) -> Vec<PathBuf> {
    logs(prefix, |name| {
        name.starts_with("ContentLog") && name.ends_with(".txt")
    })
}

/// Whether the game wrote a main-menu marker at or after `since`.
pub fn menu_reached(prefix: &Path, since: SystemTime) -> bool {
    data_roots(prefix).iter().any(|root| {
        MENU_MARKERS
            .iter()
            .any(|marker| mtime(&root.join(marker)).is_some_and(|time| time >= since))
    })
}

/// `key:value` lines of options.txt.
pub fn read_options(path: &Path) -> Result<BTreeMap<String, String>> {
    let text = fs::read(path)
        .map_err(|error| Error::msg(format!("Cannot read {}: {error}", path.display())))?;
    Ok(parse_options(&String::from_utf8_lossy(&text)))
}

pub fn parse_options(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

/// Set `changes` in options.txt text, keeping every other line in place.
pub fn update_options(text: &str, changes: &[(&str, &str)]) -> String {
    let mut pending: Vec<_> = changes.to_vec();
    let mut lines: Vec<String> = text
        .lines()
        .map(|line| {
            let key = line.split_once(':').map_or(line, |(key, _)| key);
            match pending.iter().position(|(name, _)| *name == key) {
                Some(index) => {
                    let (name, value) = pending.remove(index);
                    format!("{name}:{value}")
                }
                None => line.to_owned(),
            }
        })
        .collect();
    lines.extend(
        pending
            .into_iter()
            .map(|(name, value)| format!("{name}:{value}")),
    );
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Rewrite options.txt atomically. Minecraft may rewrite it on exit, so callers only do this
/// while the game is stopped.
pub fn write_options(path: &Path, changes: &[(&str, &str)]) -> Result<()> {
    let text = String::from_utf8_lossy(&fs::read(path)?).into_owned();
    let updated = update_options(&text, changes);
    if updated != text {
        replace_file(path, updated.as_bytes())?;
    }
    Ok(())
}

/// Write `data` beside `path` and rename it over `path`.
pub fn replace_file(path: &Path, data: &[u8]) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let staged = path.with_file_name(format!(".{name}.staging"));
    fs::write(&staged, data)?;
    fs::rename(&staged, path)?;
    Ok(())
}

/// Options for a spec of comma-separated `area=level` directives. Bedrock filters by a set
/// of areas and a set of priorities: the named areas (`all` = every area) and every
/// priority at or above the most verbose level named.
pub fn parse_log_filter(spec: &str) -> Result<[(&'static str, String); 2]> {
    let mut areas: Vec<String> = Vec::new();
    let mut lowest: Option<usize> = None;
    for directive in spec
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        let Some((area, level)) = directive.split_once('=') else {
            bail!("Log directive {directive:?} must be area=level.");
        };
        let (area, level) = (area.trim().to_uppercase(), level.trim().to_uppercase());
        if area.is_empty() || level.is_empty() {
            bail!("Log directive {directive:?} must be area=level.");
        }
        let Some(rank) = LOG_PRIORITIES.iter().position(|&known| known == level) else {
            bail!(
                "Unknown log level {:?}; use verbose, info, warn or error.",
                level.to_lowercase()
            );
        };
        lowest = Some(lowest.map_or(rank, |lowest| lowest.min(rank)));
        if !areas.contains(&area) {
            areas.push(area);
        }
    }
    let Some(lowest) = lowest else {
        bail!("The log filter spec is empty.")
    };
    let areas = if areas.iter().any(|area| area == "ALL") {
        "ALL".to_owned()
    } else {
        areas.join("|")
    };
    Ok([
        ("dev_log_area_filter", areas),
        (
            "dev_log_priority_filter",
            LOG_PRIORITIES[lowest..].join("|"),
        ),
    ])
}

/// The log filter options as `{areas, priorities, spec}`, `spec` being the equivalent
/// log_levels_set spec.
pub fn log_filter(options: &BTreeMap<String, String>) -> Value {
    let list = |key: &str| -> Vec<String> {
        options
            .get(key)
            .map(|value| {
                value
                    .split('|')
                    .filter(|part| !part.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let (areas, priorities) = (list("dev_log_area_filter"), list("dev_log_priority_filter"));
    let lowest = LOG_PRIORITIES
        .iter()
        .find(|level| priorities.iter().any(|p| p == *level));
    let spec = lowest.map(|level| {
        areas
            .iter()
            .map(|area| format!("{}={}", area.to_lowercase(), level.to_lowercase()))
            .collect::<Vec<_>>()
            .join(",")
    });
    json!({ "areas": areas, "priorities": priorities, "spec": spec })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir::{TestDir, roaming};

    #[test]
    fn options_updates_keep_other_lines_and_append_new_keys() {
        let text = "gfx_fov:70\nwebsockets_enabled:0\nmp_username:Steve:x\n";
        let updated = update_options(text, &WEBSOCKET_OPTIONS);
        assert_eq!(
            updated,
            "gfx_fov:70\nwebsockets_enabled:1\nmp_username:Steve:x\nwebsocket_encryption:0\n"
        );
        let options = parse_options(&updated);
        assert_eq!(options["mp_username"], "Steve:x");
        assert_eq!(options["websocket_encryption"], "0");
    }

    #[test]
    fn options_file_round_trip_is_atomic_and_idempotent() {
        let dir = TestDir::new("options");
        let path = dir.path().join(OPTIONS);
        fs::write(&path, "a:1\nb:2").unwrap();
        write_options(&path, &[("b", "3"), ("c", "4")]).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "a:1\nb:3\nc:4\n");
        write_options(&path, &[("b", "3")]).unwrap();
        assert_eq!(read_options(&path).unwrap().len(), 3);
        assert!(!dir.path().join(".options.txt.staging").exists());
    }

    #[test]
    fn log_filter_spec_selects_areas_and_priorities_from_the_most_verbose_level() {
        let [areas, priorities] =
            parse_log_filter("network=info, Scripting=warn,network=error").unwrap();
        assert_eq!(
            areas,
            ("dev_log_area_filter", "NETWORK|SCRIPTING".to_owned())
        );
        assert_eq!(
            priorities,
            ("dev_log_priority_filter", "INFO|WARN|ERROR".to_owned())
        );
        let [areas, priorities] = parse_log_filter("all=warn,ui=error").unwrap();
        assert_eq!(areas.1, "ALL");
        assert_eq!(priorities.1, "WARN|ERROR");
        assert!(parse_log_filter("").is_err());
        assert!(parse_log_filter("network").is_err());
        assert!(parse_log_filter("network=debug").is_err());
        assert!(parse_log_filter("=warn").is_err());
    }

    #[test]
    fn log_filter_reports_the_equivalent_spec() {
        let options =
            parse_options("dev_log_area_filter:ALL\ndev_log_priority_filter:WARN|ERROR\n");
        let filter = log_filter(&options);
        assert_eq!(filter["spec"], "all=warn");
        assert_eq!(filter["areas"], json!(["ALL"]));
        assert_eq!(log_filter(&BTreeMap::new())["spec"], Value::Null);
    }

    #[test]
    fn data_roots_find_user_options_and_menu_markers() {
        let dir = TestDir::new("prefix");
        let root = roaming(dir.path()).join("Minecraft Bedrock");
        let pe = root.join("Users/123/games/com.mojang/minecraftpe");
        fs::create_dir_all(&pe).unwrap();
        fs::create_dir_all(root.join("Users/Shared/games/com.mojang")).unwrap();
        fs::write(pe.join(OPTIONS), "gfx_fov:70\n").unwrap();
        assert_eq!(data_roots(dir.path()), std::slice::from_ref(&root));
        assert_eq!(user_dir(dir.path()).unwrap(), pe);
        assert!(shared_dir(dir.path()).unwrap().ends_with("com.mojang"));
        let before = SystemTime::now() - std::time::Duration::from_secs(60);
        assert!(!menu_reached(dir.path(), before));
        fs::create_dir_all(root.join("Flighting")).unwrap();
        fs::write(root.join("Flighting/currentTreatments"), "").unwrap();
        assert!(menu_reached(dir.path(), before));
        assert!(!menu_reached(
            dir.path(),
            SystemTime::now() + std::time::Duration::from_secs(60)
        ));
    }
}
