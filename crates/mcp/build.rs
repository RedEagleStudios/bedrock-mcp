//! Embeds the Bridge and GameTest behavior packs when they are built (`rgl run build` in
//! `mc/bridge` and `mc/gametest` writes `build/<name>_bp`), so `packs_install` can write the
//! packs that match this server. Without a build the server embeds nothing and says how to
//! export the packs.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

const PACKS: [&str; 2] = ["bridge", "gametest"];

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            files(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// The exported behavior pack folder of `mc/<pack>`, if it was built.
fn built(repo: &Path, pack: &str) -> Option<PathBuf> {
    let build = repo.join("mc").join(pack).join("build");
    let mut folders: Vec<PathBuf> = fs::read_dir(&build)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path.join("manifest.json").is_file()
                && path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().ends_with("_bp"))
        })
        .collect();
    folders.sort();
    folders.into_iter().next()
}

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let repo = manifest_dir.join("../..");
    println!("cargo:rerun-if-env-changed=BEDROCK_MCP_REQUIRE_PACKS");
    let mut code = String::from("pub static PACKS: &[EmbeddedPack] = &[\n");
    for pack in PACKS {
        let build = repo.join("mc").join(pack).join("build");
        println!("cargo:rerun-if-changed={}", build.display());
        let Some(folder) = built(&repo, pack) else {
            // Release builds set this so a binary never ships without its packs.
            if env::var_os("BEDROCK_MCP_REQUIRE_PACKS").is_some_and(|value| value == "1") {
                panic!("mc/{pack}/build has no exported pack; run `rgl run build` in mc/{pack}.");
            }
            continue;
        };
        let mut paths = Vec::new();
        files(&folder, &mut paths);
        let name = folder.file_name().unwrap().to_string_lossy().into_owned();
        code.push_str(&format!(
            "    EmbeddedPack {{\n        folder: {name:?},\n        files: &[\n"
        ));
        for path in paths {
            println!("cargo:rerun-if-changed={}", path.display());
            let relative = path.strip_prefix(&folder).unwrap();
            let relative = relative
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            let absolute = fs::canonicalize(&path).unwrap();
            code.push_str(&format!(
                "            ({relative:?}, include_bytes!({:?})),\n",
                absolute.display().to_string()
            ));
        }
        code.push_str("        ],\n    },\n");
    }
    code.push_str("];\n");
    let target = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("embedded_packs.rs");
    // Rewriting identical code would rebuild the crate for nothing.
    if fs::read_to_string(&target).ok().as_deref() != Some(code.as_str()) {
        fs::write(&target, code).unwrap();
    }
}
