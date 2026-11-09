//! Bringing edited packs into the game before `/reload all`: an rgl project's pack folders,
//! and copying pack folders over the development pack folder the game already loaded them
//! from, matched by manifest uuid. The game finds new pack folders only when it starts, so a
//! pack it has never seen is refused instead of copied.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::Serialize;
use serde_json::Value;

use crate::{
    bds::{self, UserPack},
    error::{Error, Result, bail},
    packs::{self, Kind},
};

/// The pack folders an rgl (Regolith format) project's `config.json` names.
pub fn project_packs(project: &Path) -> Result<Vec<(Kind, PathBuf)>> {
    let path = project.join("config.json");
    let text = fs::read(&path).map_err(|error| {
        Error::msg(format!(
            "{} is not an rgl project ({}: {error}).",
            project.display(),
            path.display()
        ))
    })?;
    let config: Value = serde_json::from_slice(&text)
        .map_err(|error| Error::msg(format!("{} is not valid JSON: {error}", path.display())))?;
    let found: Vec<(Kind, PathBuf)> = [
        (Kind::Behavior, "behaviorPack"),
        (Kind::Resource, "resourcePack"),
    ]
    .into_iter()
    .filter_map(|(kind, key)| {
        let folder = config["packs"][key].as_str()?;
        Some((kind, project.join(folder)))
    })
    .collect();
    if found.is_empty() {
        bail!(
            "{} names no packs.behaviorPack or packs.resourcePack.",
            path.display()
        );
    }
    Ok(found)
}

/// One pack brought into the game.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Target {
    #[serde(serialize_with = "kind_name")]
    pub kind: Kind,
    pub name: String,
    pub uuid: String,
    pub source: PathBuf,
    /// The development folder the game loads it from.
    pub installed: PathBuf,
    /// `copied`, `in_place` when the source is that folder, or `rgl` when rgl exported it.
    pub method: &'static str,
}

/// The development folder holding each pack's uuid. Fails, saying a restart is needed, when
/// the game has never seen one of them, and when two folders hold the same uuid.
pub fn match_installed(shared: &Path, given: &[UserPack]) -> Result<Vec<Target>> {
    let mut targets = Vec::new();
    let mut unknown = Vec::new();
    for (kind, pack, source) in given {
        let location = bds::development_dir(*kind);
        let dir = shared.join(location);
        let matches: Vec<_> = packs::packs_in(&dir, location, *kind)
            .into_iter()
            .filter(|installed| installed.uuid.eq_ignore_ascii_case(&pack.uuid))
            .collect();
        let installed = match matches.as_slice() {
            [] => {
                unknown.push(format!("{} ({}) in {location}", pack.name, pack.uuid));
                continue;
            }
            [one] => dir.join(&one.folder),
            several => bail!(
                "{location} holds {} folders with uuid {} ({}); remove the extra ones so the \
                 game and this tool agree on which one loads.",
                several.len(),
                pack.uuid,
                several
                    .iter()
                    .map(|pack| pack.folder.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        let same = fs::canonicalize(&installed).ok().as_deref() == Some(source.as_path());
        targets.push(Target {
            kind: *kind,
            name: pack.name.clone(),
            uuid: pack.uuid.clone(),
            source: source.clone(),
            installed,
            method: if same { "in_place" } else { "copied" },
        });
    }
    if !unknown.is_empty() {
        bail!(
            "The game has never loaded {}. It finds new pack folders only when it starts, so \
             this tool does not create them: copy the pack into that folder once, restart the \
             game (game_stop, then game_launch), add the pack to the world, then call \
             game_reload again.",
            unknown.join(" and ")
        );
    }
    Ok(targets)
}

fn kind_name<S: serde::Serializer>(
    kind: &Kind,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_str(match kind {
        Kind::Behavior => "behavior",
        Kind::Resource => "resource",
    })
}

/// Replace each installed folder with a copy of its source. The copy goes to a sibling
/// folder first, so a failed copy leaves the installed pack as it was.
pub fn copy_all(targets: &[Target]) -> Result<()> {
    for target in targets.iter().filter(|target| target.method == "copied") {
        let name = target
            .installed
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let staging = target
            .installed
            .with_file_name(format!(".{name}.bedrock-mcp-new"));
        if staging.exists() {
            fs::remove_dir_all(&staging)?;
        }
        let copied = bds::copy_dir(&target.source, &staging).map_err(|error| {
            Error::msg(format!(
                "Cannot copy {} to {}: {error}",
                target.source.display(),
                staging.display()
            ))
        });
        if let Err(error) = copied {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
        fs::remove_dir_all(&target.installed)?;
        fs::rename(&staging, &target.installed)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir::TestDir;

    fn manifest(dir: &Path, uuid: &str, module: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(
            dir.join("manifest.json"),
            format!(
                "{{\"header\": {{\"name\": \"Demo\", \"uuid\": \"{uuid}\", \"version\": [1, 0, 0]}}, \
                 \"modules\": [{{\"type\": \"{module}\"}}]}}"
            ),
        )
        .unwrap();
    }

    #[test]
    fn edits_replace_the_installed_folder_with_the_same_uuid() {
        let dir = TestDir::new("sync-copy");
        let shared = dir.path().join("com.mojang");
        let installed = shared.join("development_behavior_packs/demo_bp");
        manifest(&installed, "AAAA", "data");
        fs::write(installed.join("stale.json"), "{}").unwrap();
        manifest(
            &shared.join("development_behavior_packs/other"),
            "BBBB",
            "data",
        );
        let source = dir.path().join("work/BP");
        manifest(&source, "aaaa", "data");
        fs::create_dir_all(source.join("recipes")).unwrap();
        fs::write(source.join("recipes/a.json"), "{\"new\": true}").unwrap();

        let given = bds::user_packs(std::slice::from_ref(&source)).unwrap();
        let targets = match_installed(&shared, &given).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].installed, installed);
        assert_eq!(targets[0].method, "copied");
        copy_all(&targets).unwrap();
        assert_eq!(
            fs::read_to_string(installed.join("recipes/a.json")).unwrap(),
            "{\"new\": true}"
        );
        assert!(!installed.join("stale.json").exists());
        assert!(
            shared
                .join("development_behavior_packs/other/manifest.json")
                .exists()
        );
    }

    #[test]
    fn packs_the_game_never_loaded_are_refused() {
        let dir = TestDir::new("sync-new");
        let shared = dir.path().join("com.mojang");
        fs::create_dir_all(shared.join("development_resource_packs")).unwrap();
        let source = dir.path().join("RP");
        manifest(&source, "CCCC", "resources");
        let given = bds::user_packs(&[source]).unwrap();
        let error = match_installed(&shared, &given).unwrap_err().to_string();
        assert!(
            error.contains("development_resource_packs") && error.contains("restart"),
            "{error}"
        );
        assert!(!shared.join("development_resource_packs/RP").exists());
    }

    #[test]
    fn a_pack_edited_in_place_is_not_copied() {
        let dir = TestDir::new("sync-in-place");
        let shared = dir.path().join("com.mojang");
        let installed = shared.join("development_behavior_packs/demo");
        manifest(&installed, "DDDD", "script");
        let given = bds::user_packs(std::slice::from_ref(&installed)).unwrap();
        let targets = match_installed(&shared, &given).unwrap();
        assert_eq!(targets[0].method, "in_place");
        copy_all(&targets).unwrap();
        assert!(installed.join("manifest.json").exists());
    }

    #[test]
    fn projects_name_their_pack_folders() {
        let dir = TestDir::new("sync-project");
        fs::write(
            dir.path().join("config.json"),
            "{\"packs\": {\"behaviorPack\": \"./packs/BP\", \"resourcePack\": \"./packs/RP\"}}",
        )
        .unwrap();
        let found = project_packs(dir.path()).unwrap();
        assert_eq!(
            found,
            vec![
                (Kind::Behavior, dir.path().join("./packs/BP")),
                (Kind::Resource, dir.path().join("./packs/RP")),
            ]
        );
        assert!(project_packs(&dir.path().join("missing")).is_err());
    }
}
