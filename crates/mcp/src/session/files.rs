//! Game files: global resource packs, the debug log filter, skin packs and the embedded packs.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

use super::Session;
use crate::{
    embedded,
    error::{Result, bail},
    game_files,
    packs::{self, GlobalPack, Pack},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Index {
    index: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackAction {
    Apply,
    Activate(Index),
    Deactivate(Index),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PacksArgs {
    action: Option<PackAction>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogFilterArgs {
    spec: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkinArgs {
    path: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PacksInstallArgs {
    #[serde(default)]
    check: bool,
}

fn active_json(active: &[GlobalPack], available: &[Pack]) -> Vec<Value> {
    active
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let name = available.iter().find(|pack| pack.uuid == entry.pack_id).map(|pack| pack.name.as_str());
            json!({ "index": index, "pack_id": entry.pack_id, "version": entry.version, "name": name })
        })
        .collect()
}

impl Session {
    pub(crate) fn global_packs(&mut self, args: PacksArgs) -> Result<Value> {
        let shared = game_files::shared_dir(&self.install.prefix)?;
        let user = game_files::user_dir(&self.install.prefix)?;
        let available = packs::resource_packs(&shared);
        let applied = packs::read_global_packs(&user)?;
        let mut active = self.staged_packs.clone().unwrap_or_else(|| applied.clone());
        let mut restart_required = None;
        match args.action {
            None => {}
            Some(PackAction::Activate(Index { index })) => {
                let Some(pack) = available.get(index) else {
                    bail!(
                        "available has no index {index} ({} packs).",
                        available.len()
                    );
                };
                active.retain(|entry| entry.pack_id != pack.uuid);
                active.insert(
                    0,
                    GlobalPack {
                        pack_id: pack.uuid.clone(),
                        version: pack.version.clone(),
                    },
                );
                self.staged_packs = Some(active.clone());
            }
            Some(PackAction::Deactivate(Index { index })) => {
                if index >= active.len() {
                    bail!("active has no index {index} ({} packs).", active.len());
                }
                active.remove(index);
                self.staged_packs = Some(active.clone());
            }
            Some(PackAction::Apply) => {
                packs::write_global_packs(&user, &active)?;
                self.staged_packs = None;
                restart_required = Some(self.install.game_pid().is_some());
            }
        }
        let available_json: Vec<Value> = available
            .iter()
            .enumerate()
            .map(|(index, pack)| json!({ "index": index, "name": pack.name, "uuid": pack.uuid, "version": pack.version, "location": pack.location, "folder": pack.folder }))
            .collect();
        let pending = self
            .staged_packs
            .as_ref()
            .is_some_and(|staged| *staged != applied);
        Ok(json!({
            "available": available_json,
            "active": active_json(&active, &available),
            "pending": pending,
            "restart_required": restart_required,
        }))
    }

    pub(crate) fn log_filter_set(&mut self, args: LogFilterArgs) -> Result<Value> {
        let changes = game_files::parse_log_filter(&args.spec)?;
        if self.install.game_pid().is_some() {
            bail!(
                "Minecraft is running and may rewrite options.txt when it exits; quit first, then set the log filter."
            );
        }
        let path = game_files::options_path(&self.install.prefix)?;
        let changes: Vec<(&str, &str)> = changes
            .iter()
            .map(|(key, value)| (*key, value.as_str()))
            .collect();
        game_files::write_options(&path, &changes)?;
        let effective = game_files::log_filter(&game_files::read_options(&path)?);
        Ok(json!({ "spec": args.spec, "effective": effective, "applies_on_next_launch": true }))
    }

    pub(crate) fn log_filter_get(&mut self) -> Result<Value> {
        let path = game_files::options_path(&self.install.prefix)?;
        Ok(game_files::log_filter(&game_files::read_options(&path)?))
    }

    pub(crate) fn import_skin(&mut self, args: SkinArgs) -> Result<Value> {
        let source = std::path::absolute(&args.path)?;
        if !source.exists() {
            bail!("{} does not exist.", source.display());
        }
        let shared = game_files::shared_dir(&self.install.prefix)?;
        let (installed, name) = packs::import_skin_pack(&source, &shared)?;
        Ok(json!({
            "installed": installed,
            "name": name,
            "restart_required": self.install.game_pid().is_some(),
            "note": "Minecraft loads development skin packs at startup; they appear in the Dressing Room.",
        }))
    }

    pub(crate) fn packs_install(&mut self, args: PacksInstallArgs) -> Result<Value> {
        if embedded::PACKS.is_empty() {
            bail!(
                "This bedrock-mcp build embeds no packs. Export them from the repository with `cd \
                 mc/bridge && bun install && rgl run` and `cd mc/gametest && bun install && rgl \
                 run`, or run `rgl run build` in both folders before building the server."
            );
        }
        let dev = game_files::shared_dir(&self.install.prefix)?.join("development_behavior_packs");
        let running = self.install.game_pid().is_some();
        let Installed {
            report,
            added,
            updated,
        } = install_packs(embedded::PACKS, &dev, args.check)?;
        let mut notes = Vec::new();
        if running && added {
            notes.push(
                "Minecraft finds new development packs only when it starts: restart it, then add \
                 the packs to a world (world_packs_set)."
                    .to_owned(),
            );
        }
        if running && updated {
            notes.push(
                "Run /reload all (command_execute) in a world that uses an updated pack; it \
                 reopens the world, which registers changed commands."
                    .to_owned(),
            );
        }
        Ok(json!({ "packs": report, "notes": notes }))
    }
}

/// What [`install_packs`] did: an entry per pack, and whether it added or updated any.
struct Installed {
    report: Vec<Value>,
    added: bool,
    updated: bool,
}

/// Write each pack that is missing or outdated in `dev`, unless `check`, and report each.
fn install_packs(packs: &[embedded::EmbeddedPack], dev: &Path, check: bool) -> Result<Installed> {
    let mut done = Installed {
        report: Vec::new(),
        added: false,
        updated: false,
    };
    for pack in packs {
        let dir = dev.join(pack.folder);
        let status = pack.status(&dir);
        let write = !check && status != embedded::Status::Current;
        if write {
            pack.install(&dir)?;
            done.added |= status == embedded::Status::Missing;
            done.updated |= status == embedded::Status::Outdated;
        }
        let mut entry = pack.header();
        entry["folder"] = json!(pack.folder);
        entry["path"] = json!(dir);
        if write {
            entry["status"] = json!("updated");
            entry["before"] = json!(status.name());
        } else {
            entry["status"] = json!(status.name());
        }
        entry["written"] = json!(write);
        done.report.push(entry);
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{embedded::EmbeddedPack, testdir::TestDir};

    static PACK: EmbeddedPack = EmbeddedPack {
        folder: "Test_bp",
        files: &[(
            "manifest.json",
            br#"{"header": {"uuid": "u-1", "version": [1, 0, 0]}}"#,
        )],
    };

    #[test]
    fn packs_install_reports_whether_this_call_wrote_each_pack() {
        let dir = TestDir::new("files-install");
        let dev = dir.path().join("development_behavior_packs");
        let packs = std::slice::from_ref(&PACK);
        // A check writes nothing.
        let checked = install_packs(packs, &dev, true).unwrap();
        assert_eq!(checked.report[0]["status"], "missing");
        assert_eq!(checked.report[0]["written"], false);
        assert!(!dev.join(PACK.folder).exists());
        let first = install_packs(packs, &dev, false).unwrap();
        assert!(first.added && !first.updated);
        assert_eq!(first.report[0]["status"], "updated");
        assert_eq!(first.report[0]["before"], "missing");
        assert_eq!(first.report[0]["written"], true);
        assert_eq!(first.report[0]["uuid"], "u-1");
        // Installed and current: left alone.
        let again = install_packs(packs, &dev, false).unwrap();
        assert!(!again.added && !again.updated);
        assert_eq!(again.report[0]["status"], "current");
        assert_eq!(again.report[0]["written"], false);
        assert!(again.report[0].get("installed").is_none());
    }
}
