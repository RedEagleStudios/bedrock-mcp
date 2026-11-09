//! Client worlds under `com.mojang/minecraftWorlds`: one folder per world, named by an id,
//! with the display name in `levelname.txt` and the applied packs in
//! `world_behavior_packs.json` / `world_resource_packs.json`.

use std::{
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use crate::{
    error::{Result, bail},
    nbt::{LevelDat, Tag},
    packs::{self, GlobalPack, Kind, Pack},
};

const WORLDS: &str = "minecraftWorlds";

/// One world folder.
#[derive(Clone, Debug, PartialEq)]
pub struct WorldFolder {
    /// The folder name, the world's id.
    pub folder: String,
    /// The name the world list shows.
    pub name: String,
    pub path: PathBuf,
    /// When the world was last played (`LastPlayed` in `level.dat`, seconds since 1970), the
    /// order of the game's world list, most recent first.
    pub last_played: Option<i64>,
}

impl WorldFolder {
    /// When the world was last played, as UTC text such as `2026-11-09 04:39 UTC`.
    pub fn last_played_utc(&self) -> Option<String> {
        self.last_played.map(utc_text)
    }
}

/// `LastPlayed` from a world's `level.dat`.
fn read_last_played(path: &Path) -> Option<i64> {
    let level = LevelDat::parse(&fs::read(path.join("level.dat")).ok()?).ok()?;
    match level.root.get("LastPlayed")? {
        Tag::Long(seconds) => Some(*seconds),
        _ => None,
    }
}

/// Seconds since 1970 as UTC text, `YYYY-MM-DD HH:MM UTC`.
fn utc_text(seconds: i64) -> String {
    let (days, rest) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    // Days to a civil date (Howard Hinnant's days_from_civil, inverted).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        rest / 3600,
        rest % 3600 / 60
    )
}

/// Every world, by name.
pub fn list(shared: &Path) -> Vec<WorldFolder> {
    let mut worlds: Vec<WorldFolder> = fs::read_dir(shared.join(WORLDS))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.join("level.dat").is_file())
        .map(|path| WorldFolder {
            folder: path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            name: fs::read_to_string(path.join("levelname.txt"))
                .map(|name| name.trim().to_owned())
                .unwrap_or_default(),
            last_played: read_last_played(&path),
            path,
        })
        .collect();
    worlds.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.folder.cmp(&b.folder)));
    worlds
}

/// The worlds named `name` (ignoring case), most recently played first: the order the code
/// assumes the game's world list shows them in.
pub fn namesakes(worlds: &[WorldFolder], name: &str) -> Vec<WorldFolder> {
    let mut named: Vec<WorldFolder> = worlds
        .iter()
        .filter(|world| world.name.eq_ignore_ascii_case(name.trim()))
        .cloned()
        .collect();
    named.sort_by_key(|world| std::cmp::Reverse(world.last_played));
    named
}

/// The world whose folder id is `key`, or else every world named `key` (ignoring case), most
/// recently played first. Fails, listing the worlds, when none matches.
pub fn matching(shared: &Path, key: &str) -> Result<Vec<WorldFolder>> {
    let worlds = list(shared);
    if let Some(world) = worlds.iter().find(|world| world.folder == key) {
        return Ok(vec![world.clone()]);
    }
    let named = namesakes(&worlds, key);
    if named.is_empty() {
        bail!(
            "No world is named {key:?}. Worlds: {}.",
            worlds
                .iter()
                .map(|world| format!("{:?} ({})", world.name, world.folder))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(named)
}

/// The world whose folder id is `key`, or else the one named `key` (ignoring case). A name
/// that several worlds share fails, listing their folder ids: only a folder id tells them
/// apart.
pub fn find(shared: &Path, key: &str) -> Result<WorldFolder> {
    let mut matched = matching(shared, key)?;
    if matched.len() == 1 {
        return Ok(matched.remove(0));
    }
    bail!(
        "{} worlds are named {key:?}; pass the folder id of the one you mean instead of the \
         name: {}.",
        matched.len(),
        matched
            .iter()
            .map(|world| format!(
                "{} (last played {})",
                world.folder,
                world
                    .last_played_utc()
                    .unwrap_or_else(|| "unknown".to_owned())
            ))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// The packs of `kind` a world can apply: its own (location "world") first, then the
/// installed ones.
pub fn available_packs(shared: &Path, world: &WorldFolder, kind: Kind) -> Vec<Pack> {
    let mut available = packs::packs_in(&world.path.join(kind.world_dir()), "world", kind);
    available.extend(packs::installed_packs(shared, kind));
    available
}

/// The pack a reference names: a pack uuid, a pack folder name, or a manifest name (ignoring
/// case), in that order.
pub fn resolve<'a>(available: &'a [Pack], reference: &str) -> Result<&'a Pack> {
    let reference = reference.trim();
    let by = |matches: &dyn Fn(&Pack) -> bool| {
        available
            .iter()
            .filter(|pack| matches(pack))
            .collect::<Vec<_>>()
    };
    for candidates in [
        by(&|pack| pack.uuid.eq_ignore_ascii_case(reference)),
        by(&|pack| pack.folder == reference),
        by(&|pack| pack.name.eq_ignore_ascii_case(reference)),
    ] {
        // The same pack may be installed twice (development and regular); the first wins.
        if let Some(first) = candidates.first() {
            if candidates.iter().all(|pack| pack.uuid == first.uuid) {
                return Ok(first);
            }
            bail!(
                "{reference:?} names several packs; pass a uuid: {}.",
                candidates
                    .iter()
                    .map(|pack| format!("{} ({}/{})", pack.uuid, pack.location, pack.folder))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    bail!("No pack matches {reference:?}; pass a pack uuid, folder name or manifest name.")
}

/// The list entry that applies `pack`.
pub fn entry(pack: &Pack) -> GlobalPack {
    GlobalPack {
        pack_id: pack.uuid.clone(),
        version: pack.version.clone(),
    }
}

/// When a world folder's files were last written: the folder's own files and those one level
/// down (the world database). Opening a world writes some of them at once.
fn last_written(path: &Path) -> Option<SystemTime> {
    let modified = |path: &Path| fs::metadata(path).and_then(|meta| meta.modified()).ok();
    let entries = |path: &Path| -> Vec<PathBuf> {
        fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .collect()
    };
    entries(path)
        .into_iter()
        .flat_map(|entry| {
            let below = if entry.is_dir() {
                entries(&entry)
            } else {
                Vec::new()
            };
            std::iter::once(entry).chain(below)
        })
        .filter_map(|entry| modified(&entry))
        .max()
}

/// The world folders a click on a world's name may open (every world of that name: the menus
/// show no folder id), with when each was last written. The code expects the game to write a
/// local world's files as soon as it starts loading it, and treats a join that writes none of
/// them as another host's game.
#[derive(Debug)]
pub struct Written(Vec<(PathBuf, Option<SystemTime>)>);

impl Written {
    pub fn now(shared: &Path, name: &str) -> Self {
        Self(
            list(shared)
                .into_iter()
                .filter(|world| world.name.eq_ignore_ascii_case(name))
                .map(|world| {
                    let written = last_written(&world.path);
                    (world.path, written)
                })
                .collect(),
        )
    }

    /// Whether any of the folders was written since the snapshot.
    pub fn changed(&self) -> bool {
        self.changed_paths().next().is_some()
    }

    /// The folders written since the snapshot.
    pub fn changed_paths(&self) -> impl Iterator<Item = &Path> {
        self.0
            .iter()
            .filter(|(path, before)| last_written(path) > *before)
            .map(|(path, _)| path.as_path())
    }
}

/// World folders for tests.
#[cfg(test)]
pub mod fake {
    use super::*;
    use crate::nbt::Compound;

    /// A world folder named `folder` showing `name`, last played at `last_played`.
    pub fn world(shared: &Path, folder: &str, name: &str, last_played: Option<i64>) -> PathBuf {
        let path = shared.join(WORLDS).join(folder);
        fs::create_dir_all(&path).unwrap();
        let level = LevelDat {
            storage_version: 10,
            root_name: String::new(),
            root: Compound(
                last_played
                    .map(|seconds| ("LastPlayed".to_owned(), Tag::Long(seconds)))
                    .into_iter()
                    .collect(),
            ),
        };
        fs::write(path.join("level.dat"), level.to_bytes().unwrap()).unwrap();
        fs::write(path.join("levelname.txt"), name).unwrap();
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir::TestDir;

    fn pack(dir: &Path, folder: &str, uuid: &str, name: &str, module: &str) {
        let path = dir.join(folder);
        fs::create_dir_all(&path).unwrap();
        fs::write(
            path.join("manifest.json"),
            format!(
                r#"{{"header": {{"name": "{name}", "uuid": "{uuid}", "version": [1, 0, 0]}}, "modules": [{{"type": "{module}"}}]}}"#
            ),
        )
        .unwrap();
    }

    fn world(shared: &Path, folder: &str, name: &str) {
        fake::world(shared, folder, name, None);
    }

    #[test]
    fn opening_a_world_shows_in_its_files() {
        let dir = TestDir::new("worlds-written");
        world(dir.path(), "abc=", "Demo");
        world(dir.path(), "def=", "Other");
        let written = Written::now(dir.path(), "demo");
        assert!(!written.changed());
        std::thread::sleep(std::time::Duration::from_millis(20));
        let other = dir.path().join(WORLDS).join("def=/db");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("CURRENT"), b"x").unwrap();
        assert!(!written.changed(), "another world's files do not count");
        let db = dir.path().join(WORLDS).join("abc=/db");
        fs::create_dir_all(&db).unwrap();
        fs::write(db.join("CURRENT"), b"x").unwrap();
        assert!(written.changed());
    }

    #[test]
    fn worlds_are_found_by_folder_id_then_by_unique_name() {
        let dir = TestDir::new("worlds-find");
        world(dir.path(), "abc=", "My World");
        fake::world(dir.path(), "def=", "Twin", Some(1_762_662_000));
        fake::world(dir.path(), "ghi=", "twin", Some(1_762_000_000));
        assert_eq!(find(dir.path(), "my world").unwrap().folder, "abc=");
        // A folder id always names one world, even one whose name another world shares.
        assert_eq!(find(dir.path(), "def=").unwrap().name, "Twin");
        assert_eq!(find(dir.path(), "ghi=").unwrap().name, "twin");
        // A shared name fails and lists each folder with when it was last played.
        let twins = find(dir.path(), "Twin").unwrap_err().to_string();
        assert!(
            twins.contains("def= (last played 2025-11-09 04:20 UTC)")
                && twins.contains("ghi= (last played 2025-11-01 12:26 UTC)"),
            "{twins}"
        );
        let order: Vec<String> = namesakes(&list(dir.path()), "TWIN")
            .into_iter()
            .map(|world| world.folder)
            .collect();
        assert_eq!(order, ["def=", "ghi="], "most recently played first");
        let missing = find(dir.path(), "Nope").unwrap_err().to_string();
        assert!(missing.contains("\"My World\" (abc=)"), "{missing}");
    }

    #[test]
    fn packs_resolve_by_uuid_folder_or_name_and_world_packs_come_first() {
        let dir = TestDir::new("worlds-packs");
        world(dir.path(), "abc=", "My World");
        let found = find(dir.path(), "abc=").unwrap();
        pack(
            &found.path.join("behavior_packs"),
            "local",
            "1111",
            "Local",
            "data",
        );
        pack(
            &dir.path().join("development_behavior_packs"),
            "forms_bp",
            "2222",
            "Forms",
            "script",
        );
        pack(
            &dir.path().join("development_behavior_packs"),
            "skins",
            "3333",
            "Skins",
            "skin_pack",
        );
        pack(
            &dir.path().join("development_resource_packs"),
            "forms_rp",
            "4444",
            "Forms",
            "resources",
        );
        let behavior = available_packs(dir.path(), &found, Kind::Behavior);
        let uuids: Vec<&str> = behavior.iter().map(|pack| pack.uuid.as_str()).collect();
        assert_eq!(uuids, ["1111", "2222"]);
        assert_eq!(resolve(&behavior, "forms").unwrap().uuid, "2222");
        assert_eq!(resolve(&behavior, "forms_bp").unwrap().uuid, "2222");
        assert_eq!(resolve(&behavior, "1111").unwrap().location, "world");
        assert!(resolve(&behavior, "4444").is_err());
        let resource = available_packs(dir.path(), &found, Kind::Resource);
        assert_eq!(resolve(&resource, "Forms").unwrap().uuid, "4444");
    }
}
