//! Resource packs, the global resource pack list, and skin pack imports.

use std::{
    fs,
    io::{self, Read},
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    error::{Error, Result, bail},
    game_files::replace_file,
};

pub const GLOBAL_PACKS: &str = "global_resource_packs.json";
const RESOURCE_DIRS: [&str; 2] = ["development_resource_packs", "resource_packs"];
const BEHAVIOR_DIRS: [&str; 2] = ["development_behavior_packs", "behavior_packs"];

/// Behavior or resource packs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Behavior,
    Resource,
}

impl Kind {
    /// Whether a manifest with these module types is this kind of pack.
    fn accepts(self, types: &[String]) -> bool {
        types.iter().any(|kind| match self {
            Self::Behavior => kind == "data" || kind == "script",
            Self::Resource => kind == "resources",
        })
    }

    /// The folder a world keeps its own packs of this kind in.
    pub fn world_dir(self) -> &'static str {
        match self {
            Self::Behavior => "behavior_packs",
            Self::Resource => "resource_packs",
        }
    }

    /// The world's list of applied packs of this kind.
    pub fn world_list(self) -> &'static str {
        match self {
            Self::Behavior => "world_behavior_packs.json",
            Self::Resource => "world_resource_packs.json",
        }
    }
}

/// An installed pack's manifest header.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Pack {
    pub name: String,
    pub uuid: String,
    pub version: Value,
    pub folder: String,
    pub location: String,
}

/// One entry of `global_resource_packs.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GlobalPack {
    pub pack_id: String,
    pub version: Value,
}

struct Manifest {
    name: String,
    uuid: String,
    version: Value,
    types: Vec<String>,
}

fn manifest(data: &[u8], fallback_name: &str) -> Option<Manifest> {
    let manifest: Value = serde_json::from_slice(strip_bom(data)).ok()?;
    let header = manifest.get("header")?;
    Some(Manifest {
        name: header
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(fallback_name)
            .to_owned(),
        uuid: header.get("uuid")?.as_str()?.to_owned(),
        version: header.get("version").cloned().unwrap_or(Value::Null),
        types: manifest
            .get("modules")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|module| Some(module.get("type")?.as_str()?.to_owned()))
            .collect(),
    })
}

fn strip_bom(data: &[u8]) -> &[u8] {
    data.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(data)
}

fn folder_manifest(folder: &Path) -> Option<Manifest> {
    let name = folder.file_name()?.to_string_lossy();
    let mut manifest = manifest(&fs::read(folder.join("manifest.json")).ok()?, &name)?;
    // Packs often name themselves with a language key ("pack.name") defined in texts/.
    if let Some(text) = localized(folder, &manifest.name) {
        manifest.name = text;
    }
    Some(manifest)
}

/// The English text of a language key in the pack's `texts/en_US.lang`.
fn localized(folder: &Path, key: &str) -> Option<String> {
    if key.contains(char::is_whitespace) || !key.contains('.') {
        return None;
    }
    let lang = fs::read(folder.join("texts").join("en_US.lang")).ok()?;
    String::from_utf8_lossy(strip_bom(&lang))
        .lines()
        .find_map(|line| {
            let (name, text) = line.split_once('=')?;
            // A `##` starts a comment, also after the text.
            let text = text
                .split("\t#")
                .next()
                .unwrap_or(text)
                .split("##")
                .next()
                .unwrap_or(text);
            (name.trim() == key).then(|| text.trim().to_owned())
        })
}

/// Installed resource packs under the shared `com.mojang` folder: development packs first,
/// each location sorted by folder name.
pub fn resource_packs(shared: &Path) -> Vec<Pack> {
    installed_packs(shared, Kind::Resource)
}

/// Installed packs of `kind` under the shared `com.mojang` folder: development packs first,
/// each location sorted by folder name.
pub fn installed_packs(shared: &Path, kind: Kind) -> Vec<Pack> {
    let dirs = match kind {
        Kind::Behavior => BEHAVIOR_DIRS,
        Kind::Resource => RESOURCE_DIRS,
    };
    dirs.iter()
        .flat_map(|location| packs_in(&shared.join(location), location, kind))
        .collect()
}

/// The packs of `kind` in one folder, sorted by folder name; `location` labels them.
pub fn packs_in(dir: &Path, location: &str, kind: Kind) -> Vec<Pack> {
    let mut folders: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    folders.sort();
    folders
        .into_iter()
        .filter_map(|folder| {
            let manifest = folder_manifest(&folder)?;
            kind.accepts(&manifest.types).then(|| Pack {
                name: manifest.name,
                uuid: manifest.uuid,
                version: manifest.version,
                folder: folder
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                location: location.to_owned(),
            })
        })
        .collect()
}

/// The active global packs, top priority first; a missing file means none.
pub fn read_global_packs(user_dir: &Path) -> Result<Vec<GlobalPack>> {
    read_pack_list(&user_dir.join(GLOBAL_PACKS))
}

pub fn write_global_packs(user_dir: &Path, packs: &[GlobalPack]) -> Result<()> {
    write_pack_list(&user_dir.join(GLOBAL_PACKS), packs)
}

/// A `[{pack_id, version}]` list: global resource packs or a world's packs. A missing file
/// means none.
pub fn read_pack_list(path: &Path) -> Result<Vec<GlobalPack>> {
    match fs::read(path) {
        Ok(data) => serde_json::from_slice(strip_bom(&data))
            .map_err(|error| Error::msg(format!("{} is not valid: {error}", path.display()))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

pub fn write_pack_list(path: &Path, packs: &[GlobalPack]) -> Result<()> {
    let mut text = serde_json::to_string_pretty(packs)?;
    text.push('\n');
    replace_file(path, text.as_bytes())
}

/// Install a skin pack folder, `.mcpack` or `.zip` into `development_skin_packs`.
/// Returns the installed folder and the pack's name.
pub fn import_skin_pack(source: &Path, shared: &Path) -> Result<(PathBuf, String)> {
    let target_root = shared.join("development_skin_packs");
    fs::create_dir_all(&target_root)?;
    let not_a_pack = || {
        Error::msg(format!(
            "{} is not a skin pack (manifest.json and skins.json).",
            source.display()
        ))
    };
    let target = if source.is_dir() {
        if folder_manifest(source).is_none() || !source.join("skins.json").is_file() {
            return Err(not_a_pack());
        }
        let target = target_root.join(source.file_name().ok_or_else(not_a_pack)?);
        replace_dir(&target)?;
        copy_dir(source, &target)?;
        target
    } else if is_zip(source) {
        extract_skin_zip(source, &target_root)?.ok_or_else(not_a_pack)?
    } else if source
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
    {
        bail!(
            "{} is a single skin PNG. Bedrock imports single skins only through the Dressing \
             Room's file picker (Dressing Room → Classic Skins → Owned → Import → Choose New \
             Skin); use input to drive it, or import a skin pack folder, .mcpack or .zip.",
            source.display()
        );
    } else {
        bail!(
            "{} is not a skin pack folder, .mcpack or .zip.",
            source.display()
        );
    };
    let name = folder_manifest(&target)
        .map(|manifest| manifest.name)
        .unwrap_or_else(|| {
            target
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });
    Ok((target, name))
}

fn is_zip(path: &Path) -> bool {
    let mut magic = [0; 4];
    fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut magic))
        .is_ok()
        && magic == *b"PK\x03\x04"
}

fn replace_dir(target: &Path) -> Result<()> {
    if target.exists() {
        fs::remove_dir_all(target)?;
    }
    fs::create_dir_all(target)?;
    Ok(())
}

fn copy_dir(source: &Path, target: &Path) -> Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let destination = target.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            fs::create_dir_all(&destination)?;
            copy_dir(&entry.path(), &destination)?;
        } else {
            fs::copy(entry.path(), &destination)?;
        }
    }
    Ok(())
}

/// Relative path inside the archive with only normal components, or None when unsafe.
fn safe_relative(name: &str) -> Option<PathBuf> {
    let path = Path::new(name);
    path.components()
        .all(|component| matches!(component, Component::Normal(_)))
        .then(|| path.to_path_buf())
}

/// Extract the pack rooted at the archive's top-level (or single-folder) manifest.json.
/// `None` when the archive holds no skin pack.
fn extract_skin_zip(source: &Path, target_root: &Path) -> Result<Option<PathBuf>> {
    let file = fs::File::open(source)?;
    let zip_error =
        |error: zip::result::ZipError| Error::msg(format!("{}: {error}", source.display()));
    let mut archive = zip::ZipArchive::new(file).map_err(zip_error)?;
    let names: Vec<String> = archive
        .file_names()
        .map(|name| name.map(|name| name.into_owned()))
        .collect::<Result<_, _>>()
        .map_err(zip_error)?;
    let Some(root) = names
        .iter()
        .filter_map(|name| name.strip_suffix("manifest.json"))
        .find(|root| root.is_empty() || (root.ends_with('/') && root.matches('/').count() == 1))
        .map(str::to_owned)
    else {
        return Ok(None);
    };
    if !names
        .iter()
        .any(|name| *name == format!("{root}skins.json"))
    {
        return Ok(None);
    }
    let folder = match root.trim_end_matches('/') {
        "" => source
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        folder => folder.to_owned(),
    };
    let Some(folder) = safe_relative(&folder).filter(|path| path.components().count() == 1) else {
        bail!("{} contains an unsafe path: {folder}", source.display());
    };
    let target = target_root.join(folder);
    replace_dir(&target)?;
    for index in 0..archive.len() {
        let mut member = archive.by_index(index).map_err(zip_error)?;
        let name = member.name().map_err(zip_error)?.into_owned();
        let Some(relative) = name.strip_prefix(&root) else {
            continue;
        };
        if member.is_dir() || relative.is_empty() {
            continue;
        }
        let Some(relative) = safe_relative(relative) else {
            let _ = fs::remove_dir_all(&target);
            bail!("{} contains an unsafe path: {name}", source.display());
        };
        let destination = target.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        io::copy(&mut member, &mut fs::File::create(&destination)?)?;
    }
    Ok(Some(target))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::testdir::TestDir;

    const MANIFEST: &str = r#"{"header":{"name":"Test Skins","uuid":"u-1","version":[1,0,0]},"modules":[{"type":"skin_pack"}]}"#;

    fn zip(path: &Path, members: &[(&str, &str)]) {
        let mut writer = zip::ZipWriter::new(fs::File::create(path).unwrap());
        for (name, data) in members {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(data.as_bytes()).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn resource_packs_list_only_resource_modules_development_first() {
        let dir = TestDir::new("packs");
        let write = |location: &str, folder: &str, manifest: &str| {
            let path = dir.path().join(location).join(folder);
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("manifest.json"), manifest).unwrap();
        };
        write(
            "resource_packs",
            "a",
            r#"{"header":{"name":"Vanilla+","uuid":"r-1","version":[1,2,3]},"modules":[{"type":"resources"}]}"#,
        );
        write(
            "development_resource_packs",
            "z",
            "\u{feff}{\"header\":{\"uuid\":\"d-1\",\"version\":[0,0,1]},\"modules\":[{\"type\":\"resources\"}]}",
        );
        write(
            "development_resource_packs",
            "b",
            r#"{"header":{"uuid":"b-1"},"modules":[{"type":"data"}]}"#,
        );
        write("resource_packs", "broken", "{");
        let packs = resource_packs(dir.path());
        let ids: Vec<_> = packs.iter().map(|pack| pack.uuid.as_str()).collect();
        assert_eq!(ids, ["d-1", "r-1"]);
        assert_eq!(packs[0].name, "z");
        assert_eq!(packs[1].version, serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn pack_names_given_as_language_keys_read_the_english_text() {
        let dir = TestDir::new("packs-lang");
        let path = dir.path().join("development_behavior_packs").join("demo");
        fs::create_dir_all(path.join("texts")).unwrap();
        fs::write(
            path.join("manifest.json"),
            r#"{"header":{"name":"pack.name","uuid":"m-1"},"modules":[{"type":"script"}]}"#,
        )
        .unwrap();
        fs::write(
            path.join("texts").join("en_US.lang"),
            "pack.description=Forms\npack.name=Demo BP\t## shown in the pack list\n",
        )
        .unwrap();
        let packs = installed_packs(dir.path(), Kind::Behavior);
        assert_eq!(packs[0].name, "Demo BP");
    }

    #[test]
    fn global_packs_round_trip_and_default_to_none() {
        let dir = TestDir::new("global");
        assert!(read_global_packs(dir.path()).unwrap().is_empty());
        let packs = vec![GlobalPack {
            pack_id: "u".into(),
            version: serde_json::json!([1, 0, 0]),
        }];
        write_global_packs(dir.path(), &packs).unwrap();
        assert_eq!(read_global_packs(dir.path()).unwrap(), packs);
    }

    #[test]
    fn skin_zips_install_under_their_folder() {
        let dir = TestDir::new("skins");
        let archive = dir.path().join("pack.mcpack");
        zip(
            &archive,
            &[
                ("Cool/manifest.json", MANIFEST),
                ("Cool/skins.json", "{}"),
                ("Cool/textures/a.png", "png"),
            ],
        );
        let (target, name) = import_skin_pack(&archive, dir.path()).unwrap();
        assert_eq!(target, dir.path().join("development_skin_packs/Cool"));
        assert_eq!(name, "Test Skins");
        assert_eq!(
            fs::read_to_string(target.join("textures/a.png")).unwrap(),
            "png"
        );
    }

    #[test]
    fn flat_skin_zips_use_the_archive_name() {
        let dir = TestDir::new("flat");
        let archive = dir.path().join("Mine.zip");
        zip(
            &archive,
            &[("manifest.json", MANIFEST), ("skins.json", "{}")],
        );
        let (target, _) = import_skin_pack(&archive, dir.path()).unwrap();
        assert!(target.ends_with("development_skin_packs/Mine"));
    }

    #[test]
    fn unsafe_and_non_pack_archives_are_refused() {
        let dir = TestDir::new("unsafe");
        let evil = dir.path().join("evil.zip");
        zip(
            &evil,
            &[
                ("manifest.json", MANIFEST),
                ("skins.json", "{}"),
                ("../escape.txt", "x"),
            ],
        );
        assert!(
            import_skin_pack(&evil, dir.path())
                .unwrap_err()
                .to_string()
                .contains("unsafe")
        );
        assert!(
            !dir.path()
                .join("development_skin_packs/escape.txt")
                .exists()
        );
        let plain = dir.path().join("plain.zip");
        zip(&plain, &[("readme.txt", "x")]);
        assert!(
            import_skin_pack(&plain, dir.path())
                .unwrap_err()
                .to_string()
                .contains("not a skin pack")
        );
        let png = dir.path().join("skin.png");
        fs::write(&png, b"\x89PNG").unwrap();
        assert!(
            import_skin_pack(&png, dir.path())
                .unwrap_err()
                .to_string()
                .contains("Dressing Room")
        );
    }

    #[test]
    fn skin_folders_are_copied() {
        let dir = TestDir::new("folder");
        let source = dir.path().join("src/MySkins");
        fs::create_dir_all(source.join("texts")).unwrap();
        fs::write(source.join("manifest.json"), MANIFEST).unwrap();
        fs::write(source.join("skins.json"), "{}").unwrap();
        fs::write(source.join("texts/en_US.lang"), "x").unwrap();
        let (target, _) = import_skin_pack(&source, dir.path()).unwrap();
        assert!(target.join("texts/en_US.lang").is_file());
    }
}
