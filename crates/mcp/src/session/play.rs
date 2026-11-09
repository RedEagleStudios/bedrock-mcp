//! Playing: input, chat and scripted cameras.

use std::{sync::Arc, time::Duration};

use serde::Deserialize;
use serde_json::{Value, json};

use super::Session;
use crate::{
    camera::{CameraPath, Easing, Keyframe},
    error::{Error, Result, bail},
    input::{InputEnv, InputRequest},
    platform,
};

/// How long a typed chat line gets to come back as a PlayerMessage event.
const CHAT_CONFIRM_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatArgs {
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraArgs {
    keyframes: Option<Vec<Keyframe>>,
    easing: Option<Easing>,
    #[serde(default, rename = "loop")]
    looping: bool,
    hide_hand: Option<bool>,
    #[serde(default)]
    release: bool,
}

impl Session {
    pub(crate) fn input(&mut self, request: InputRequest) -> Result<Value> {
        self.input_free()?;
        let world = match request.look {
            Some(_) => Some(self.ensure_world()?),
            None => None,
        };
        let bindings = self.bindings();
        let pid = self.install.game_pid();
        let scale = move || -> Result<f64> {
            let pid = pid.ok_or_else(|| Error::msg("Minecraft is not running."))?;
            Ok(platform::desktop()
                .window(pid)?
                .map_or(1.0, |window| window.scale))
        };
        let clock = Arc::clone(&self.clock);
        self.with_input(|agent, controller| {
            let env = InputEnv {
                bindings: &bindings,
                scale: &scale,
                world: world.as_ref(),
                clock: &*clock,
            };
            controller.apply(agent, &env, &request)
        })?;
        Ok(json!({ "held": self.controller.held() }))
    }

    pub(crate) fn chat(&mut self, args: ChatArgs) -> Result<Value> {
        self.input_free()?;
        let text = args.text.trim_end_matches(['\r', '\n']);
        if text.is_empty() {
            bail!("chat needs text.");
        }
        if text.trim_start().starts_with('/') {
            bail!(
                "chat_send only sends chat text. Run commands with command_execute, which returns \
                 the command's status code and message."
            );
        }
        let world = self.ensure_world()?;
        let after = world.socket().event_count();
        let bindings = self.bindings();
        let clock = Arc::clone(&self.clock);
        self.with_input(|agent, controller| controller.send_chat(agent, &*clock, &bindings, text))?;
        let event = world
            .socket()
            .wait_event(after, CHAT_CONFIRM_TIMEOUT, |event| {
                event.name == "PlayerMessage" && event.body["message"].as_str() == Some(text)
            });
        match event {
            Some(event) => Ok(json!({ "sent": true, "event": event.body })),
            None => bail!(
                "The game did not report the chat message within {} s; chat may be closed, muted, or the \
                 text was altered by a stray key.",
                CHAT_CONFIRM_TIMEOUT.as_secs()
            ),
        }
    }

    pub(crate) fn camera_path(&mut self, args: CameraArgs) -> Result<Value> {
        if args.release {
            let world = self.world()?;
            match self.camera.take() {
                Some(camera) => camera.release(&world)?,
                None => {
                    world.command("camera @s clear")?;
                }
            }
            return Ok(json!({ "released": true }));
        }
        let Some(keyframes) = args.keyframes else {
            bail!("camera_play needs keyframes, or release: true.");
        };
        let world = self.ensure_world()?;
        if let Some(previous) = self.camera.take() {
            // The new path takes over the camera; failures here are superseded.
            let _ = previous.release(&world);
        }
        let camera = CameraPath::start(
            world,
            &keyframes,
            args.easing.unwrap_or_default(),
            args.looping,
            args.hide_hand.unwrap_or(true),
        )?;
        let status = camera.status();
        self.camera = Some(camera);
        Ok(status)
    }
}
