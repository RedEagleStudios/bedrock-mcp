//! The files `pack_check` reads, from a folder or a pack archive, and the packs among them.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    ffi::OsStr,
    fs,
    io::{self, Read},
    path::Path,
};

use super::{Diagnostic, Pack, PackKind, Severity, version_text};
use crate::{
    docs::jsonpos::{self, Node},
    error::{Error, Result, bail},
};

/// More files than any add-on has; stops a walk of a whole drive.
const MAX_FILES: usize = 200_000;
/// Larger pack files are skipped rather than read.
const MAX_TEXT: u64 = 16 << 20;
/// Bytes of text read from one archive at most.
const MAX_ARCHIVE_TEXT: u64 = 1 << 30;
/// How deep below the checked folder a pack's manifest.json may be (`a/b/c/BP/manifest.json`).
const MAX_PACK_DEPTH: usize = 4;

/// Every file path under the checked folder (`/`-separated, relative), and the text of the
/// files the checks read: `.json` and `.mcfunction`.
#[derive(Default)]
pub struct Tree {
    files: BTreeSet<String>,
    texts: BTreeMap<String, String>,
    /// What was skipped while reading.
    pub notes: Vec<String>,
}

fn is_text(path: &str) -> bool {
    path.ends_with(".json") || path.ends_with(".mcfunction")
}

impl Tree {
    /// Read a folder, or a .mcpack, .mcaddon, .mcworld or .zip file.
    pub fn load(path: &Path) -> Result<Self> {
        let path = std::path::absolute(path)?;
        let mut tree = Self::default();
        if path.is_dir() {
            tree.walk(&path, &path)?;
        } else if path.is_file() {
            tree.read_archive(
                fs::File::open(&path)?,
                "",
                &path.display().to_string(),
                true,
            )?;
        } else {
            bail!("{} does not exist.", path.display());
        }
        Ok(tree)
    }

    /// Add one file; `text` reads it when the checks need its text.
    fn add(
        &mut self,
        path: String,
        size: u64,
        text: impl FnOnce() -> io::Result<Vec<u8>>,
    ) -> Result<()> {
        if self.files.len() >= MAX_FILES {
            bail!("The folder has more than {MAX_FILES} files; give a pack or project folder.");
        }
        if is_text(&path) {
            if size > MAX_TEXT {
                self.notes.push(format!(
                    "Skipped {path}: larger than {} MB.",
                    MAX_TEXT >> 20
                ));
            } else {
                let bytes = text()?;
                self.texts
                    .insert(path.clone(), String::from_utf8_lossy(&bytes).into_owned());
            }
        }
        self.files.insert(path);
        Ok(())
    }

    /// Hidden folders (`.git`, `.regolith`) and `node_modules` hold no pack files of the
    /// project, only tooling and copies.
    fn walk(&mut self, root: &Path, dir: &Path) -> Result<()> {
        let mut entries: Vec<_> = fs::read_dir(dir)?.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                let hidden =
                    name.to_string_lossy().starts_with('.') || name == OsStr::new("node_modules");
                if !hidden {
                    self.walk(root, &path)?;
                }
            } else if kind.is_file()
                && let Ok(relative) = path.strip_prefix(root)
            {
                let key: Vec<_> = relative
                    .components()
                    .map(|part| part.as_os_str().to_string_lossy())
                    .collect();
                let size = entry.metadata().map_or(0, |meta| meta.len());
                self.add(key.join("/"), size, || fs::read(&path))?;
            }
        }
        Ok(())
    }

    /// The members of a zip archive under `prefix`; a .mcaddon's inner .mcpack files are read
    /// as folders named after them when `nested` allows it.
    fn read_archive(
        &mut self,
        reader: impl Read + io::Seek,
        prefix: &str,
        name: &str,
        nested: bool,
    ) -> Result<()> {
        let zip_error = |error: zip::result::ZipError| Error::msg(format!("{name}: {error}"));
        let mut zip = zip::ZipArchive::new(reader).map_err(zip_error)?;
        let mut budget = MAX_ARCHIVE_TEXT;
        for index in 0..zip.len() {
            let mut member = zip.by_index(index).map_err(zip_error)?;
            if member.is_dir() {
                continue;
            }
            let path = format!(
                "{prefix}{}",
                member.name().map_err(zip_error)?.replace('\\', "/")
            );
            let size = member.size();
            if nested
                && (path.ends_with(".mcpack") || path.ends_with(".zip"))
                && size <= MAX_ARCHIVE_TEXT
            {
                let mut bytes = Vec::new();
                member.read_to_end(&mut bytes)?;
                let inner = format!("{path}/");
                self.read_archive(io::Cursor::new(bytes), &inner, &path, false)?;
                continue;
            }
            let wanted = is_text(&path) && size <= MAX_TEXT;
            if wanted && size > budget {
                bail!(
                    "{name} holds more than {} MB of pack text.",
                    MAX_ARCHIVE_TEXT >> 20
                );
            }
            budget -= if wanted { size } else { 0 };
            self.add(path, size, || {
                let mut bytes = Vec::new();
                (&mut member).take(MAX_TEXT).read_to_end(&mut bytes)?;
                Ok(bytes)
            })?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn from_files(files: &[(&str, &str)]) -> Self {
        let mut tree = Self::default();
        for (path, text) in files {
            tree.add(path.to_string(), text.len() as u64, || {
                Ok(text.as_bytes().to_vec())
            })
            .unwrap();
        }
        tree
    }

    /// Every file under `folder` (empty or ending with `/`), by its path below it.
    pub fn files_under<'a>(&'a self, folder: &str) -> impl Iterator<Item = &'a str> + use<'a> {
        let folder = folder.to_owned();
        self.files
            .range(folder.clone()..)
            .map_while(move |path| path.strip_prefix(folder.as_str()))
    }

    /// The text files under `folder`, by their path below it.
    pub fn texts_under<'a>(
        &'a self,
        folder: &str,
    ) -> impl Iterator<Item = (&'a str, &'a str)> + use<'a> {
        let folder = folder.to_owned();
        self.texts
            .range(folder.clone()..)
            .map_while(move |(path, text)| {
                Some((path.strip_prefix(folder.as_str())?, text.as_str()))
            })
    }
}

/// The pack type a manifest declares by its module types; None for skin packs, world
/// templates and anything else this check does not cover.
fn pack_kind(manifest: &Node) -> Option<PackKind> {
    let types: Vec<&str> = manifest
        .get("modules")
        .map_or(&[][..], Node::items)
        .iter()
        .filter_map(|module| module.get("type")?.as_str())
        .collect();
    if types.contains(&"resources") {
        Some(PackKind::Resource)
    } else if types
        .iter()
        .any(|kind| matches!(*kind, "data" | "script" | "javascript" | "client_data"))
    {
        Some(PackKind::Behavior)
    } else {
        None
    }
}

/// The behavior and resource packs in `tree`: folders with a manifest.json, shallowest
/// first, not inside another pack. A second pack with the same header uuid is skipped.
pub fn packs(tree: &Tree, diagnostics: &mut Vec<Diagnostic>, notes: &mut Vec<String>) -> Vec<Pack> {
    let mut manifests: Vec<&String> = tree
        .texts
        .keys()
        .filter(|path| *path == "manifest.json" || path.ends_with("/manifest.json"))
        .filter(|path| path.matches('/').count() <= MAX_PACK_DEPTH)
        .collect();
    manifests.sort_by_key(|path| (path.matches('/').count(), path.as_str()));
    let mut packs: Vec<Pack> = Vec::new();
    let mut uuids = HashSet::new();
    for path in manifests {
        let folder = &path[..path.len() - "manifest.json".len()];
        if packs.iter().any(|pack| folder.starts_with(&pack.folder)) {
            continue;
        }
        let manifest = match jsonpos::parse(&tree.texts[path]) {
            Ok(manifest) => manifest,
            Err(error) => {
                diagnostics.push(Diagnostic {
                    file: path.clone(),
                    line: Some(error.pos.line),
                    column: Some(error.pos.column),
                    severity: Severity::Error,
                    rule: "json-syntax",
                    message: format!("{}; the game cannot read this manifest", error.message),
                });
                continue;
            }
        };
        let Some(kind) = pack_kind(&manifest) else {
            continue;
        };
        let header = manifest.get("header");
        let uuid = header
            .and_then(|header| header.get("uuid")?.as_str())
            .map(str::to_owned);
        if let Some(uuid) = &uuid
            && !uuids.insert(uuid.to_ascii_lowercase())
        {
            notes.push(format!(
                "Skipped {path}: another pack checked here has header uuid {uuid}."
            ));
            continue;
        }
        packs.push(Pack {
            kind,
            folder: folder.to_owned(),
            label: String::new(),
            name: header
                .and_then(|header| header.get("name")?.as_str())
                .unwrap_or_default()
                .to_owned(),
            uuid,
            version: header
                .and_then(|header| header.get("version"))
                .and_then(version_text),
        });
    }
    for index in 0..packs.len() {
        let kind = packs[index].kind;
        let several = packs.iter().filter(|pack| pack.kind == kind).count() > 1;
        let folder = packs[index].folder.trim_end_matches('/');
        packs[index].label = if several && !folder.is_empty() {
            format!("{}/{folder}", kind.label())
        } else {
            kind.label().to_owned()
        };
    }
    packs
}
