//! Bedrock Dedicated Server runs with no client: GameTests and pack load checks.

use std::{path::PathBuf, time::Duration};

use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Map, Value};

use super::Session;
use crate::{
    bds::{self, DEFAULT_IDLE_MS, DEFAULT_TIMEOUT_MS, RunDirs},
    error::{Error, Result, bail},
    game_files,
    gametest::Run,
    worlds,
};

/// How long a load check may take unless told otherwise.
const DEFAULT_CHECK_TIMEOUT_MS: u64 = 120_000;

/// The arguments of a tool that takes one run's options at the top level, or a `runs` list of
/// them next to the shared options.
pub struct Batch<Shared, Item> {
    shared: Shared,
    runs: Vec<Item>,
    single: bool,
}

impl<'de, Shared: DeserializeOwned + SharedFields, Item: DeserializeOwned> Deserialize<'de>
    for Batch<Shared, Item>
{
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let fields = Map::<String, Value>::deserialize(deserializer)?;
        split_batch(fields).map_err(serde::de::Error::custom)
    }
}

/// The option names a call shares across its runs.
pub trait SharedFields {
    const FIELDS: &'static [&'static str];
}

fn split_batch<Shared: DeserializeOwned + SharedFields, Item: DeserializeOwned>(
    mut fields: Map<String, Value>,
) -> Result<Batch<Shared, Item>> {
    let mut shared = Map::new();
    for key in Shared::FIELDS {
        if let Some(value) = fields.remove(*key) {
            shared.insert((*key).to_owned(), value);
        }
    }
    let shared: Shared = serde_json::from_value(Value::Object(shared))?;
    match fields.remove("runs") {
        Some(runs) => {
            if !fields.is_empty() {
                let keys: Vec<&str> = fields.keys().map(String::as_str).collect();
                bail!(
                    "With runs, give {} inside each run, not next to runs.",
                    keys.join(", ")
                );
            }
            let runs: Vec<Item> = serde_json::from_value(runs)
                .map_err(|error| Error::msg(format!("runs: {error}")))?;
            if runs.is_empty() {
                bail!("runs needs at least one run.");
            }
            Ok(Batch {
                shared,
                runs,
                single: false,
            })
        }
        None => Ok(Batch {
            shared,
            runs: vec![serde_json::from_value(Value::Object(fields))?],
            single: true,
        }),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckShared {
    #[serde(default)]
    server: Option<PathBuf>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

impl SharedFields for CheckShared {
    const FIELDS: &'static [&'static str] = &["server", "timeout_ms"];
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckRun {
    #[serde(default)]
    label: Option<String>,
    packs: Vec<PathBuf>,
    #[serde(default)]
    beta_apis: bool,
}

pub type CheckArgs = Batch<CheckShared, CheckRun>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerShared {
    #[serde(default)]
    server: Option<PathBuf>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    idle_ms: Option<u64>,
}

impl SharedFields for ServerShared {
    const FIELDS: &'static [&'static str] = &["server", "timeout_ms", "idle_ms"];
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerRun {
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    packs: Vec<PathBuf>,
    #[serde(default)]
    world: Option<String>,
    #[serde(default)]
    test: Option<String>,
    #[serde(default)]
    tag: Option<String>,
    #[serde(default)]
    until_fail: bool,
    #[serde(default)]
    repeat: Option<u32>,
    #[serde(default)]
    stop_on_failure: Option<bool>,
    #[serde(default)]
    rotation: Option<u8>,
    #[serde(default)]
    commands: Vec<String>,
    #[serde(default)]
    shards: Option<usize>,
}

pub type ServerTestArgs = Batch<ServerShared, ServerRun>;

impl Session {
    pub(crate) fn bds_check(&mut self, args: CheckArgs) -> Result<Value> {
        bds::check(&bds::CheckOptions {
            server: args.shared.server.unwrap_or_else(bds::default_server),
            dirs: RunDirs::new(&self.install.agent_dir()),
            runs: args
                .runs
                .into_iter()
                .map(|run| bds::CheckSpec {
                    label: run.label,
                    packs: run.packs,
                    beta_apis: run.beta_apis,
                })
                .collect(),
            single: args.single,
            timeout: Duration::from_millis(
                args.shared.timeout_ms.unwrap_or(DEFAULT_CHECK_TIMEOUT_MS),
            ),
        })
    }

    pub(crate) fn gametest_server(&mut self, args: ServerTestArgs) -> Result<Value> {
        let mut runs = Vec::new();
        for run in args.runs {
            let world = match &run.world {
                Some(key) => {
                    let shared = game_files::shared_dir(&self.install.prefix)?;
                    Some((worlds::find(&shared, key)?, shared))
                }
                None => None,
            };
            runs.push(bds::RunSpec {
                label: run.label,
                packs: run.packs,
                world,
                commands: run.commands,
                run: Run {
                    test: run.test,
                    tag: run.tag,
                    rotation: run.rotation,
                    repeat: run.repeat,
                    stop_on_failure: run.stop_on_failure,
                    until_fail: run.until_fail,
                    wait_ms: 0,
                },
                shards: run.shards.unwrap_or(1),
            });
        }
        bds::run(&bds::Options {
            server: args.shared.server.unwrap_or_else(bds::default_server),
            dirs: RunDirs::new(&self.install.agent_dir()),
            runs,
            single: args.single,
            timeout: Duration::from_millis(args.shared.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)),
            idle: Duration::from_millis(args.shared.idle_ms.unwrap_or(DEFAULT_IDLE_MS)),
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn single_and_batched_arguments() {
        let single: ServerTestArgs =
            serde_json::from_value(json!({ "packs": ["bp"], "shards": 3, "timeout_ms": 5 }))
                .unwrap();
        assert!(single.single);
        assert_eq!(single.runs[0].shards, Some(3));
        assert_eq!(single.shared.timeout_ms, Some(5));

        let batch: ServerTestArgs = serde_json::from_value(json!({
            "idle_ms": 7,
            "runs": [{ "label": "a", "packs": ["a"] }, { "label": "b", "packs": ["b"], "tag": "smoke" }]
        }))
        .unwrap();
        assert!(!batch.single);
        assert_eq!(batch.runs.len(), 2);
        assert_eq!(batch.runs[1].tag.as_deref(), Some("smoke"));
        assert_eq!(batch.shared.idle_ms, Some(7));

        let mixed = serde_json::from_value::<ServerTestArgs>(
            json!({ "packs": ["a"], "runs": [{ "packs": ["b"] }] }),
        );
        assert!(mixed.is_err_and(|error| error.to_string().contains("inside each run")));
        assert!(serde_json::from_value::<ServerTestArgs>(json!({ "runs": [] })).is_err());
        assert!(
            serde_json::from_value::<CheckArgs>(json!({ "packs": ["a"], "bogus": 1 })).is_err()
        );
        let checks: CheckArgs =
            serde_json::from_value(json!({ "runs": [{ "packs": ["a"], "beta_apis": true }] }))
                .unwrap();
        assert!(checks.runs[0].beta_apis && !checks.single);
    }
}
