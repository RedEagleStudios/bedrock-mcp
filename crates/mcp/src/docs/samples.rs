//! Mojang's bedrock-samples releases (https://github.com/Mojang/bedrock-samples): the
//! `metadata/` folder (script API modules, JSON schemas with descriptions, commands, Molang
//! queries) and the vanilla pack files, from the release that matches the installed game.
//! Each release's small archive (`-min.zip`) is downloaded once with curl into the cache.

use std::{
    fs, io,
    path::{Component, Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

use serde_json::Value;

use super::{Scratch, cache_dir, download};
use crate::{
    error::{Error, Result, bail},
    install::Install,
};

pub const REPOSITORY: &str = "Mojang/bedrock-samples";
const RELEASES_URL: &str =
    "https://api.github.com/repos/Mojang/bedrock-samples/releases?per_page=100";
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);
/// More than a release's kept folders unpack to (about 60 MB); stops a broken archive
/// filling the disk.
const MAX_EXTRACTED: u64 = 512 << 20;
/// The archive folders kept: the metadata and the vanilla files that show components in use.
const KEPT: [&str; 6] = [
    "metadata/",
    "behavior_pack/entities/",
    "behavior_pack/items/",
    "behavior_pack/blocks/",
    "behavior_pack/biomes/",
    "resource_pack/particles/",
];
/// The game's package manifest; its `<Identity Version>` is the game build.
const GAME_CONFIGS: [&str; 2] = ["MicrosoftGame.Config", "MicrosoftGame.config"];

/// The installed game's version.
#[derive(Clone, Debug, PartialEq)]
pub struct GameVersion {
    /// `major.minor.patch`, e.g. 1.26.62 for a hotfix of 1.26.60.
    pub version: [u32; 3],
    pub build: u32,
    /// The preview app, whose identity is the Beta package.
    pub preview: bool,
}

impl GameVersion {
    /// The version in a MicrosoftGame.Config: `Version="1.26.6201.0"` is 1.26.62 build 1.
    pub fn from_config(text: &str) -> Option<Self> {
        let identity = &text[text.find("<Identity")?..];
        let identity = &identity[..identity.find('>')?];
        let attribute = |name: &str| {
            let start = identity.find(&format!(" {name}=\""))? + name.len() + 3;
            let value = &identity[start..];
            Some(&value[..value.find('"')?])
        };
        let parts: Vec<u32> = attribute("Version")?
            .split('.')
            .map(str::parse)
            .collect::<Result<_, _>>()
            .ok()?;
        let [major, minor, packed, ..] = parts[..] else {
            return None;
        };
        let name = attribute("Name").unwrap_or_default();
        Some(Self {
            version: [major, minor, packed / 100],
            build: packed % 100,
            preview: name.contains("Beta") || name.contains("Preview"),
        })
    }

    /// The release line a hotfix belongs to: 1.26.62 is in 1.26.60.
    fn line(&self) -> [u32; 3] {
        let [major, minor, patch] = self.version;
        [major, minor, patch / 10 * 10]
    }

    pub fn display(&self) -> String {
        let [major, minor, patch] = self.version;
        let kind = if self.preview { " (preview)" } else { "" };
        format!("{major}.{minor}.{patch}.{}{kind}", self.build)
    }
}

/// The installed game's version: from the running game's folder, else from the game folder
/// mcbe-gdk-linux names in `game-dir` next to its profile.
pub fn game_version(install: &Install) -> Option<GameVersion> {
    let running = install
        .game_pid()
        .and_then(|pid| install.game_exe(pid))
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let launcher = install
        .profile
        .parent()
        .and_then(|root| fs::read_to_string(root.join("game-dir")).ok())
        .map(|dir| PathBuf::from(dir.trim()));
    running
        .into_iter()
        .chain(launcher)
        .flat_map(|dir| GAME_CONFIGS.map(|name| dir.join(name)))
        .find_map(|path| GameVersion::from_config(&fs::read_to_string(path).ok()?))
}

/// One bedrock-samples release: tag `v1.26.60.5` or `v1.26.70.23-preview`.
#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub tag: String,
    pub version: [u32; 4],
    pub preview: bool,
}

impl Release {
    pub fn parse(tag: &str) -> Option<Self> {
        let (text, preview) = match tag.strip_suffix("-preview") {
            Some(text) => (text, true),
            None => (tag, false),
        };
        let parts: Vec<u32> = text
            .strip_prefix('v')?
            .split('.')
            .map(str::parse)
            .collect::<Result<_, _>>()
            .ok()?;
        let version: [u32; 4] = parts.try_into().ok()?;
        Some(Self {
            tag: tag.to_owned(),
            version,
            preview,
        })
    }

    fn base(&self) -> [u32; 3] {
        [self.version[0], self.version[1], self.version[2]]
    }

    /// `1.26.60`: the game release line this release documents.
    pub fn line(&self) -> String {
        let [major, minor, patch] = self.base();
        format!("{major}.{minor}.{patch}")
    }

    pub fn archive_url(&self) -> String {
        format!(
            "https://github.com/{REPOSITORY}/releases/download/{tag}/bedrock-samples-{tag}-min.zip",
            tag = self.tag
        )
    }
}

/// The releases in a GitHub releases listing that ship a `-min.zip` archive.
fn parse_releases(listing: &Value) -> Vec<Release> {
    listing
        .as_array()
        .into_iter()
        .flatten()
        .filter(|release| {
            release["assets"].as_array().is_some_and(|assets| {
                assets.iter().any(|asset| {
                    asset["name"]
                        .as_str()
                        .is_some_and(|name| name.ends_with("-min.zip"))
                })
            })
        })
        .filter_map(|release| Release::parse(release["tag_name"].as_str()?))
        .collect()
}

/// The release for `game` and a note when it is not the game's own. Stable games take the
/// newest stable release of their line or older; previews the newest preview no newer than
/// the game, else that stable choice; an unknown or older game the newest stable release.
pub fn pick(releases: &[Release], game: Option<&GameVersion>) -> Option<(Release, Option<String>)> {
    let newest = |preview: bool, at_most: Option<[u32; 4]>| {
        releases
            .iter()
            .filter(|release| release.preview == preview)
            .filter(|release| at_most.is_none_or(|limit| release.version <= limit))
            .max_by_key(|release| release.version)
            .cloned()
    };
    let Some(game) = game else {
        let release = newest(false, None).or_else(|| newest(true, None))?;
        let note = format!(
            "The installed game version is unknown; using the newest stable release {}.",
            release.tag
        );
        return Some((release, Some(note)));
    };
    let [major, minor, patch] = game.version;
    if game.preview
        && let Some(release) = newest(true, Some([major, minor, patch, game.build]))
    {
        let exact = release.base() == game.version && release.version[3] == game.build;
        let note = (!exact).then(|| {
            format!(
                "No release matches preview game {}; using the nearest older preview {}.",
                game.display(),
                release.tag
            )
        });
        return Some((release, note));
    }
    let [major, minor, patch] = game.line();
    let stable = newest(false, Some([major, minor, patch, u32::MAX]));
    let Some(release) = stable else {
        let release = newest(false, None).or_else(|| newest(true, None))?;
        let note = format!(
            "No release is as old as game {}; using the newest stable release {}, which may describe features the game lacks.",
            game.display(),
            release.tag
        );
        return Some((release, Some(note)));
    };
    let note = (release.base() != game.line() || game.preview).then(|| {
        format!(
            "No release matches game {}; using {}, so newer APIs and components are missing.",
            game.display(),
            release.tag
        )
    });
    Some((release, note))
}

/// A release unpacked in the cache.
#[derive(Debug)]
pub struct Samples {
    pub release: Release,
    pub dir: PathBuf,
    pub game: Option<GameVersion>,
    pub notes: Vec<String>,
}

impl Samples {
    pub fn metadata(&self) -> PathBuf {
        self.dir.join("metadata")
    }

    /// Where the data came from, for replies.
    pub fn source(&self) -> Value {
        serde_json::json!({
            "repository": REPOSITORY,
            "release": self.release.tag,
            "game_version": self.game.as_ref().map(GameVersion::display),
        })
    }
}

/// An unpacked release has the metadata the tools read.
fn is_complete(dir: &Path) -> bool {
    let metadata = dir.join("metadata");
    metadata.join("script_modules").is_dir() && metadata.join("json_schemas").is_dir()
}

/// The releases unpacked under `home`.
fn cached(home: &Path) -> Vec<Release> {
    fs::read_dir(home)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| is_complete(&entry.path()))
        .filter_map(|entry| Release::parse(&entry.file_name().to_string_lossy()))
        .collect()
}

/// The published releases, listed once per process.
fn published() -> Result<Vec<Release>> {
    static LISTED: Mutex<Option<Vec<Release>>> = Mutex::new(None);
    let mut listed = LISTED.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some(releases) = listed.as_ref() {
        return Ok(releases.clone());
    }
    let scratch = Scratch::new("releases")?;
    let file = scratch.path().join("releases.json");
    download(RELEASES_URL, &file, LIST_TIMEOUT)?;
    let listing: Value = serde_json::from_slice(&fs::read(&file)?)
        .map_err(|error| Error::msg(format!("{RELEASES_URL} sent invalid JSON: {error}")))?;
    let releases = parse_releases(&listing);
    if releases.is_empty() {
        bail!("{RELEASES_URL} lists no release with a -min.zip archive.");
    }
    *listed = Some(releases.clone());
    Ok(releases)
}

/// The release for the installed game, downloaded on first use.
pub fn open(install: &Install) -> Result<Samples> {
    let home = cache_dir()?.join("bedrock-samples");
    let game = game_version(install);
    let on_disk = cached(&home);
    let mut notes = Vec::new();
    // A cached release that is the game's own needs no listing.
    let exact = game
        .as_ref()
        .and_then(|game| pick(&on_disk, Some(game)))
        .filter(|(_, note)| note.is_none());
    let (release, note) = match exact {
        Some(choice) => choice,
        None => match published() {
            Ok(releases) => pick(&releases, game.as_ref())
                .ok_or_else(|| Error::msg(format!("{RELEASES_URL} lists no usable release.")))?,
            Err(error) => {
                let choice = pick(&on_disk, game.as_ref()).ok_or_else(|| {
                    Error::msg(format!(
                        "{error} No bedrock-samples release is cached yet, so none can be read."
                    ))
                })?;
                notes.push(format!(
                    "Listing releases failed ({error}); using the cached {}.",
                    choice.0.tag
                ));
                choice
            }
        },
    };
    notes.extend(note);
    let (dir, replaced) = ensure_in(&home, &release, |archive| {
        download(&release.archive_url(), archive, DOWNLOAD_TIMEOUT)
    })?;
    notes.extend(replaced);
    Ok(Samples {
        release,
        dir,
        game,
        notes,
    })
}

/// Release `release` under `home`, fetched with `fetch` (which writes the archive to the path
/// it is given) when missing or incomplete. It is unpacked in a staging folder and renamed
/// into place, so an interrupted download is never taken as cached. The note says when an
/// incomplete copy was replaced.
fn ensure_in(
    home: &Path,
    release: &Release,
    fetch: impl FnOnce(&Path) -> Result<()>,
) -> Result<(PathBuf, Option<String>)> {
    let dir = home.join(&release.tag);
    if is_complete(&dir) {
        return Ok((dir, None));
    }
    let note = dir.exists().then(|| {
        let _ = fs::remove_dir_all(&dir);
        format!(
            "The cached {REPOSITORY} {} was incomplete, so it was downloaded again.",
            release.tag
        )
    });
    let staging = home.join(format!("{}.staging-{}", release.tag, std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;
    let archive = staging.join("min.zip");
    let unpacked = fetch(&archive).and_then(|()| extract(&archive, &staging));
    let _ = fs::remove_file(&archive);
    if let Err(error) = unpacked {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    if !is_complete(&staging) {
        let _ = fs::remove_dir_all(&staging);
        bail!(
            "The {REPOSITORY} archive {} lacks metadata/script_modules or metadata/json_schemas.",
            release.archive_url()
        );
    }
    // Another server may have finished the same download first; either copy will do.
    if fs::rename(&staging, &dir).is_err() {
        let _ = fs::remove_dir_all(&staging);
        if !is_complete(&dir) {
            bail!("Cannot move the downloaded release into {}.", dir.display());
        }
    }
    Ok((dir, note))
}

/// The member's path inside the release: as stored, or below one top folder.
fn release_path(name: &str) -> Option<&str> {
    let kept = |path: &str| KEPT.iter().any(|prefix| path.starts_with(prefix));
    if kept(name) {
        return Some(name);
    }
    name.split_once('/')
        .map(|(_, rest)| rest)
        .filter(|rest| kept(rest))
}

/// Unpack the kept folders of a release archive into `to`.
fn extract(archive: &Path, to: &Path) -> Result<()> {
    let zip_error =
        |error: zip::result::ZipError| Error::msg(format!("{}: {error}", archive.display()));
    let mut zip = zip::ZipArchive::new(fs::File::open(archive)?).map_err(zip_error)?;
    let mut written = 0;
    for index in 0..zip.len() {
        let mut member = zip.by_index(index).map_err(zip_error)?;
        let name = member.name().map_err(zip_error)?.into_owned();
        let Some(relative) = release_path(&name) else {
            continue;
        };
        if member.is_dir() {
            continue;
        }
        let path = Path::new(relative);
        if !path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        {
            bail!("{} contains an unsafe path: {name}", archive.display());
        }
        let target = to.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut limited = io::Read::take(&mut member, MAX_EXTRACTED - written);
        written += io::copy(&mut limited, &mut fs::File::create(&target)?)?;
        if written >= MAX_EXTRACTED {
            bail!(
                "{} unpacks to more than {} MB.",
                archive.display(),
                MAX_EXTRACTED >> 20
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::testdir::TestDir;

    fn game(version: [u32; 3], build: u32, preview: bool) -> GameVersion {
        GameVersion {
            version,
            build,
            preview,
        }
    }

    #[test]
    fn the_game_version_comes_from_the_identity() {
        let config = r#"<Game><Identity Name="Microsoft.MinecraftUWP" Publisher="CN=Demo" Version="1.26.6201.0" /></Game>"#;
        assert_eq!(
            GameVersion::from_config(config),
            Some(game([1, 26, 62], 1, false))
        );
        let config = r#"<Identity Name="Microsoft.MinecraftWindowsBeta" Version="1.2.34521.0"/>"#;
        assert_eq!(
            GameVersion::from_config(config),
            Some(game([1, 2, 345], 21, true))
        );
        assert_eq!(GameVersion::from_config("<Game/>"), None);
    }

    #[test]
    fn tags_parse_with_their_channel() {
        let release = Release::parse("v1.2.30.05").unwrap();
        assert_eq!((release.version, release.preview), ([1, 2, 30, 5], false));
        assert!(Release::parse("v1.2.40.21-preview").unwrap().preview);
        assert_eq!(Release::parse("1.2.30"), None);
        let listing = serde_json::json!([
            { "tag_name": "v1.2.30.4", "assets": [{ "name": "bedrock-samples-v1.2.30.4-min.zip" }] },
            { "tag_name": "v1.2.20.1", "assets": [] },
        ]);
        let tags: Vec<String> = parse_releases(&listing)
            .into_iter()
            .map(|release| release.tag)
            .collect();
        assert_eq!(tags, ["v1.2.30.4"]);
    }

    #[test]
    fn releases_follow_the_game() {
        let releases: Vec<Release> = [
            "v1.2.20.3",
            "v1.2.30.4",
            "v1.2.30.22-preview",
            "v1.2.40.21-preview",
            "v1.2.40.23-preview",
        ]
        .into_iter()
        .map(|tag| Release::parse(tag).unwrap())
        .collect();
        let tag = |game: Option<GameVersion>| {
            let (release, note) = pick(&releases, game.as_ref()).unwrap();
            (release.tag, note.is_some())
        };
        // A hotfix uses its line's release.
        assert_eq!(
            tag(Some(game([1, 2, 32], 1, false))),
            ("v1.2.30.4".into(), false)
        );
        assert_eq!(
            tag(Some(game([1, 2, 20], 30, false))),
            ("v1.2.20.3".into(), false)
        );
        // A stable game newer than every stable release takes the newest, with a note.
        assert_eq!(
            tag(Some(game([1, 2, 40], 2, false))),
            ("v1.2.30.4".into(), true)
        );
        assert_eq!(
            tag(Some(game([1, 2, 40], 23, true))),
            ("v1.2.40.23-preview".into(), false)
        );
        assert_eq!(
            tag(Some(game([1, 2, 40], 22, true))),
            ("v1.2.40.21-preview".into(), true)
        );
        assert_eq!(
            tag(Some(game([1, 2, 10], 1, false))),
            ("v1.2.30.4".into(), true)
        );
        assert_eq!(tag(None), ("v1.2.30.4".into(), true));
    }

    #[test]
    fn a_release_is_unpacked_once_and_keeps_only_its_metadata_and_vanilla_files() {
        let temp = TestDir::new("samples");
        let release = Release::parse("v1.0.0.1").unwrap();
        let write_archive = |path: &Path| {
            let mut zip = zip::ZipWriter::new(fs::File::create(path).unwrap());
            let options = zip::write::SimpleFileOptions::default();
            for name in [
                "metadata/script_modules/@minecraft/server_1.0.0.json",
                "metadata/json_schemas/server/entity/1.0.0/a.json",
                "behavior_pack/entities/cow.json",
                "resource_pack/textures/big.png",
            ] {
                zip.start_file(name, options).unwrap();
                zip.write_all(b"{}").unwrap();
            }
            zip.finish().unwrap();
            Ok(())
        };
        let (dir, note) = ensure_in(temp.path(), &release, write_archive).unwrap();
        assert_eq!(note, None);
        assert!(dir.join("behavior_pack/entities/cow.json").is_file());
        assert!(!dir.join("resource_pack").exists());
        let (again, _) =
            ensure_in(temp.path(), &release, |_| panic!("cached copy refetched")).unwrap();
        assert_eq!(again, dir);
        assert_eq!(cached(temp.path()), [release]);
    }
}
