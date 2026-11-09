//! GameTests on a Bedrock Dedicated Server, with no client.

use std::{path::PathBuf, time::Duration};

use serde::Deserialize;
use serde_json::Value;

use super::Session;
use crate::{
    bds::{self, DEFAULT_IDLE_MS, DEFAULT_TIMEOUT_MS},
    error::Result,
    game_files,
    gametest::Run,
    worlds,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerTestArgs {
    #[serde(default)]
    server: Option<PathBuf>,
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
    timeout_ms: Option<u64>,
    #[serde(default)]
    idle_ms: Option<u64>,
}

impl Session {
    pub(crate) fn gametest_server(&mut self, args: ServerTestArgs) -> Result<Value> {
        let world = match &args.world {
            Some(key) => {
                let shared = game_files::shared_dir(&self.install.prefix)?;
                Some((worlds::find(&shared, key)?, shared))
            }
            None => None,
        };
        let options = bds::Options {
            server: args.server.unwrap_or_else(bds::default_server),
            run_dir: self.install.agent_dir().join("bds"),
            packs: args.packs,
            world,
            commands: args.commands,
            run: Run {
                test: args.test,
                tag: args.tag,
                rotation: args.rotation,
                repeat: args.repeat,
                stop_on_failure: args.stop_on_failure,
                until_fail: args.until_fail,
                wait_ms: 0,
            },
            timeout: Duration::from_millis(args.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)),
            idle: Duration::from_millis(args.idle_ms.unwrap_or(DEFAULT_IDLE_MS)),
        };
        bds::run(&options)
    }
}
