//! The live camera and HUD from the unstable layer: `camera_get`, `camera_set`, `hud_get`,
//! and the death screen for `game_status.attention`.

use std::{sync::Arc, time::Duration};

use protocol::unstable::{CameraState, HudState, Perspective, SceneStack, UiScreen};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Session, unstable::LIVE_POLL};
use crate::{
    clock::poll,
    error::{Result, bail},
    ui_tree,
};

const CAMERA: &str = "camera";
const HUD: &str = "hud_text";
/// The binding `camera_set` presses; the game cycles first person, third person back, third
/// person front.
const TOGGLE_PERSPECTIVE: &str = "key.togglePerspective";
/// How long one press may take to show in the perspective the camera reads.
const PERSPECTIVE_WAIT: Duration = Duration::from_millis(1500);
/// A perspective read longer ago than this is not trusted to be current.
const PERSPECTIVE_FRESH_MS: u64 = 1000;
/// Chat lines read to find the death message.
const DEATH_CHAT_LINES: u32 = 10;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraSetArgs {
    perspective: Perspective,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HudArgs {
    chat_limit: Option<u32>,
}

fn perspective_name(perspective: Perspective) -> Value {
    json!(perspective)
}

/// Bedrock yaw and pitch in degrees for a view direction: yaw 0 faces +Z (south) and 90
/// faces -X (west); positive pitch looks down.
pub fn yaw_pitch([x, y, z]: [f32; 3]) -> (f32, f32) {
    let yaw = (-x).atan2(z).to_degrees();
    let pitch = (-y).clamp(-1.0, 1.0).asin().to_degrees();
    (yaw, pitch)
}

/// `value` rounded to `places` decimals, as JSON shows it.
fn round(value: f32, places: i32) -> f64 {
    let scale = 10f64.powi(places);
    (f64::from(value) * scale).round() / scale
}

fn rounded(vector: [f32; 3], places: i32) -> [f64; 3] {
    vector.map(|value| round(value, places))
}

pub fn camera_json(camera: &CameraState) -> Value {
    let view = camera.view.as_ref().map(|view| {
        let (yaw, pitch) = yaw_pitch(view.forward);
        json!({
            // A zero position may be a camera-relative origin rather than a world position,
            // so the reply shows null for it.
            "position": (view.position.iter().any(|axis| axis.abs() > 1e-3))
                .then(|| rounded(view.position, 3)),
            "yaw": round(yaw, 2),
            "pitch": round(pitch, 2),
            "forward": rounded(view.forward, 4),
            "up": rounded(view.up, 4),
            "fov": view.fov.map(|fov| round(fov, 2)),
            "aspect_ratio": view.aspect_ratio.map(|ratio| round(ratio, 4)),
            "age_ms": view.age_ms,
        })
    });
    json!({
        "perspective": camera.perspective,
        "perspective_age_ms": camera.perspective_age_ms,
        "fov": camera.fov.map(|fov| round(fov, 2)),
        "fov_age_ms": camera.fov_age_ms,
        "view": view,
        "detail": camera.detail,
    })
}

/// The game's `ToastMessageType` names, from LeviLamina's `ToastMessageType.h`.
const TOAST_KINDS: &[&str] = &[
    "unknown",
    "achievement",
    "invite",
    "raw_invite",
    "focus_lost",
    "import_resource_pack",
    "import_export_world",
    "global_resource_crash_recovery",
    "message",
    "player_messaging",
    "dialog_message",
    "chat_screen_notification",
    "debug",
    "recipe_unlocked",
    "snackbar",
    "progress_popup",
    "party_invite",
    "party_travel_opt_out",
    "party_travel_opt_in",
    "party_destination_cookie_opt_out",
    "party_destination_cookie_opt_in",
    "return_to_game",
    "matchmaking",
];

/// The game's `GuiMessage::MessageType` names, from LeviLamina's `GuiMessage.h`.
const MESSAGE_KINDS: &[&str] = &[
    "none",
    "chat",
    "client",
    "localized",
    "system",
    "whisper",
    "text_object",
    "text_object_whisper",
    "announcement",
    "audio_subtitle",
];

fn kind_name(names: &[&'static str], kind: u32) -> Value {
    names
        .get(kind as usize)
        .map_or_else(|| json!(kind), |name| json!(name))
}

/// The player is dead: what the death screen, the camera and the HUD say about it.
#[derive(Debug, Default, PartialEq)]
pub struct Death {
    /// The JSON UI death screen on the gameplay stack, if the death screen is a JSON UI
    /// screen; an Ore UI death screen shows only as the Ore UI page.
    pub screen: Option<String>,
    /// The death message: from the death screen's tree, else the newest game message the
    /// HUD still shows (the game posts the death message there unless the
    /// showDeathMessages game rule is off).
    pub message: Option<String>,
    /// The texts on a JSON UI death screen that are not button labels.
    pub texts: Vec<String>,
    /// Whether a Respawn button shows and is enabled; None without a UI tree.
    pub respawn: Option<bool>,
    /// Game ticks since death, from the camera.
    pub ticks: Option<f32>,
}

/// The JSON UI death screen's name on the gameplay stack, if it is up.
pub fn death_screen(stacks: &[SceneStack]) -> Option<&str> {
    stacks
        .iter()
        .find(|stack| {
            stack
                .screens
                .iter()
                .any(|name| name == "in_game_play_screen")
        })?
        .screens
        .iter()
        .map(String::as_str)
        .find(|name| name.contains("death"))
}

/// What the death screen's UI tree says.
pub fn death_from_tree(screen: &str, trees: &[UiScreen]) -> Death {
    let mut death = Death {
        screen: Some(screen.to_owned()),
        ..Death::default()
    };
    let Some(tree) = trees
        .iter()
        .find(|tree| tree.scene == screen && !tree.nodes.is_empty())
    else {
        return death;
    };
    let view = ui_tree::screen_json(tree, None, ui_tree::View::Controls);
    let controls = view["controls"].as_array().cloned().unwrap_or_default();
    death.texts = controls
        .iter()
        .filter(|control| control["kind"] == "text")
        .filter_map(|control| control["text"].as_str().map(str::to_owned))
        .collect();
    death.message = death.texts.first().cloned();
    death.respawn = Some(controls.iter().any(|control| {
        control["kind"] == "button"
            && control["enabled"] != false
            && control["text"]
                .as_str()
                .is_some_and(|text| text.to_lowercase().contains("respawn"))
    }));
    death
}

/// `GuiMessage::MessageType::LocalizedMessage`, the kind of the death message.
const LOCALIZED_MESSAGE: u32 = 3;

/// The newest localized game message the HUD still shows.
pub fn shown_game_message(hud: &HudState) -> Option<String> {
    hud.chat
        .iter()
        .rev()
        .find(|line| line.kind == LOCALIZED_MESSAGE && line.duration > 0.0)
        .map(|line| ui_tree::plain(&line.message))
}

fn hud_text(text: &Option<protocol::unstable::HudText>) -> Value {
    match text {
        Some(text) if !text.text.is_empty() => {
            json!({ "text": text.text, "changed_ms": text.changed_ms })
        }
        Some(_) => Value::Null,
        None => json!({ "unread": true }),
    }
}

pub fn hud_json(hud: &HudState) -> Value {
    let toasts: Vec<Value> = hud
        .toasts
        .iter()
        .map(|toast| {
            json!({
                "seq": toast.seq,
                "at_ms": toast.at_ms,
                "kind": kind_name(TOAST_KINDS, toast.kind),
                "title": toast.title,
                "message": toast.subtitle,
                "message_id": (!toast.message_id.is_empty()).then_some(&toast.message_id),
                "persistent": toast.persistent,
            })
        })
        .collect();
    let chat: Vec<Value> = hud
        .chat
        .iter()
        .map(|line| {
            json!({
                "kind": kind_name(MESSAGE_KINDS, line.kind),
                "username": (!line.username.is_empty()).then_some(&line.username),
                "message": ui_tree::plain(&line.message),
                "full": ui_tree::plain(&line.full),
                "duration": line.duration,
                "seen": line.seen,
            })
        })
        .collect();
    json!({
        "toasts": toasts,
        "title": hud_text(&hud.title),
        "subtitle": hud_text(&hud.subtitle),
        "actionbar": hud_text(&hud.actionbar),
        "title_ticks": hud.title_ticks.map(|[fade_in, stay, fade_out]| {
            json!({ "fade_in": fade_in, "stay": stay, "fade_out": fade_out })
        }),
        "popup": hud_text(&hud.popup),
        "tip": hud_text(&hud.tip),
        "chat": chat,
        "detail": hud.detail,
        "fresh": hud.fresh,
    })
}

impl Session {
    fn require(&mut self, capability: &str, tool: &str) -> Result<()> {
        self.connect_running_agent();
        let mut notes = Vec::new();
        if !self.unstable_capability(capability, &mut notes) {
            bail!(
                "{tool} needs the unstable layer's {capability} capability: {}",
                notes.join("; ")
            );
        }
        Ok(())
    }

    pub(crate) fn camera_get(&mut self) -> Result<Value> {
        self.require(CAMERA, "camera_get")?;
        let camera = self.unstable.camera()?;
        Ok(camera_json(&camera))
    }

    /// The perspective the camera read within the last second.
    fn current_perspective(&mut self) -> Result<Option<Perspective>> {
        let camera = self.unstable.camera()?;
        Ok(camera
            .perspective_age_ms
            .filter(|age| *age <= PERSPECTIVE_FRESH_MS)
            .and(camera.perspective))
    }

    pub(crate) fn camera_set(&mut self, args: CameraSetArgs) -> Result<Value> {
        self.require(CAMERA, "camera_set")?;
        if let Some(screen) = self.covering_screen() {
            bail!(
                "{screen} is open over the game, and the perspective key only works on the HUD. \
                 Close it first."
            );
        }
        let Some(from) = self.current_perspective()? else {
            bail!(
                "the camera has not read the perspective in the last second; is a world open \
                 and the game drawing frames?"
            );
        };
        let mut now = from;
        let mut presses = 0;
        // Three perspectives: at most two presses reach any of them.
        while now != args.perspective && presses < 3 {
            let before = now;
            self.input(serde_json::from_value(
                json!({ "press": [TOGGLE_PERSPECTIVE] }),
            )?)?;
            presses += 1;
            let clock = Arc::clone(&self.clock);
            let changed = poll(&*clock, PERSPECTIVE_WAIT, LIVE_POLL, || {
                Ok(self.current_perspective()?.filter(|seen| *seen != before))
            })?;
            match changed {
                Some(seen) => now = seen,
                None => bail!(
                    "pressed {TOGGLE_PERSPECTIVE} but the perspective stayed {} after {} ms",
                    perspective_name(before),
                    PERSPECTIVE_WAIT.as_millis()
                ),
            }
        }
        if now != args.perspective {
            bail!(
                "the perspective is {} after {presses} presses, not {}",
                perspective_name(now),
                perspective_name(args.perspective)
            );
        }
        let camera = self.unstable.camera()?;
        Ok(json!({
            "perspective": now,
            "previous": from,
            "presses": presses,
            "camera": camera_json(&camera),
        }))
    }

    pub(crate) fn hud_get(&mut self, args: HudArgs) -> Result<Value> {
        self.require(HUD, "hud_get")?;
        let hud = self.unstable.hud(args.chat_limit)?;
        let mut out = hud_json(&hud);
        let mut notes = Vec::new();
        out["death"] = match self.death_with(Some(&hud), &mut notes) {
            Some(Some(death)) => death_json(&death),
            Some(None) => json!({ "shown": false }),
            None => json!({ "shown": null }),
        };
        out["notes"] = json!(notes);
        Ok(out)
    }

    /// Whether the player is dead, from the live layer: Some(Some) dead, Some(None) alive,
    /// None when neither the camera (hud_text) nor the screen stack can tell.
    pub(super) fn death(&mut self, notes: &mut Vec<String>) -> Option<Option<Death>> {
        let hud = if self.unstable_capability(HUD, notes) {
            self.unstable.hud(Some(DEATH_CHAT_LINES)).ok()
        } else {
            None
        };
        self.death_with(hud.as_ref(), notes)
    }

    fn death_with(
        &mut self,
        hud: Option<&HudState>,
        notes: &mut Vec<String>,
    ) -> Option<Option<Death>> {
        let screen = if self.unstable_capability("screens", notes) {
            self.unstable
                .screens()
                .ok()
                .map(|stacks| death_screen(&stacks).map(str::to_owned))
        } else {
            None
        };
        // A JSON UI death screen settles it; else the camera's damage parameters. A stack
        // without one says nothing on builds whose death screen is Ore UI.
        let dead = hud.and_then(|hud| hud.dead);
        let mut death = match (screen.flatten(), dead) {
            (Some(screen), _) => {
                let trees = if self.unstable_requestable("ui_tree", notes) {
                    self.unstable.ui_tree(None, None, true).unwrap_or_default()
                } else {
                    Vec::new()
                };
                death_from_tree(&screen, &trees)
            }
            (None, Some(true)) => Death::default(),
            (None, Some(false)) => return Some(None),
            (None, None) => return None,
        };
        death.ticks = hud
            .and_then(|hud| hud.death_ticks)
            .filter(|ticks| *ticks > 0.0);
        if death.message.is_none() {
            death.message = hud.and_then(shown_game_message);
        }
        Some(Some(death))
    }
}

pub fn death_json(death: &Death) -> Value {
    json!({
        "shown": true,
        "screen": death.screen,
        "message": death.message,
        "texts": death.texts,
        "respawn_available": death.respawn,
        "ticks": death.ticks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::unstable::{CameraView, ChatLine, HudText, Toast, UiNode};

    #[test]
    fn yaw_and_pitch_follow_the_bedrock_convention() {
        let (yaw, pitch) = yaw_pitch([0.0, 0.0, 1.0]);
        assert!(yaw.abs() < 1e-4 && pitch.abs() < 1e-4, "south");
        let (yaw, _) = yaw_pitch([-1.0, 0.0, 0.0]);
        assert!((yaw - 90.0).abs() < 1e-4, "west");
        let (_, pitch) = yaw_pitch([0.0, -1.0, 0.0]);
        assert!((pitch - 90.0).abs() < 1e-4, "down");
    }

    #[test]
    fn camera_json_adds_yaw_and_pitch_to_the_view() {
        let camera = CameraState {
            perspective: Some(Perspective::ThirdPersonFront),
            perspective_age_ms: Some(5),
            fov: Some(70.0),
            fov_age_ms: Some(5),
            view: Some(CameraView {
                position: [1.0, 65.6, 2.0],
                forward: [-1.0, 0.0, 0.0],
                up: [0.0, 1.0, 0.0],
                fov: Some(70.0),
                aspect_ratio: Some(1.8),
                age_ms: 10,
            }),
            detail: None,
        };
        let value = camera_json(&camera);
        assert_eq!(value["perspective"], "third_person_front");
        assert_eq!(value["view"]["yaw"], 90.0);
        assert_eq!(value["view"]["position"], json!([1.0, 65.6, 2.0]));
    }

    #[test]
    fn hud_json_names_kinds_and_hides_empty_texts() {
        let hud = HudState {
            toasts: vec![Toast {
                seq: 1,
                at_ms: 1000,
                kind: 13,
                title: "New recipe".into(),
                subtitle: "Torch".into(),
                message_id: String::new(),
                persistent: false,
            }],
            title: Some(HudText {
                text: "§lHello".into(),
                changed_ms: Some(2000),
            }),
            subtitle: Some(HudText {
                text: String::new(),
                changed_ms: None,
            }),
            actionbar: None,
            title_ticks: Some([10, 70, 20]),
            popup: None,
            tip: None,
            chat: vec![
                ChatLine {
                    kind: 3,
                    username: String::new(),
                    message: "Steve died".into(),
                    full: "Steve died".into(),
                    duration: 4.0,
                    seen: true,
                },
                ChatLine {
                    kind: 1,
                    username: "Alex".into(),
                    message: "hi".into(),
                    full: "<Alex> hi".into(),
                    duration: 9.0,
                    seen: true,
                },
            ],
            dead: Some(true),
            death_ticks: Some(12.5),
            detail: None,
            fresh: true,
        };
        let value = hud_json(&hud);
        assert_eq!(value["toasts"][0]["kind"], "recipe_unlocked");
        assert_eq!(value["toasts"][0]["message_id"], Value::Null);
        assert_eq!(value["title"]["text"], "§lHello");
        assert_eq!(value["subtitle"], Value::Null);
        assert_eq!(value["actionbar"]["unread"], true);
        assert_eq!(value["title_ticks"]["stay"], 70);
        assert_eq!(value["chat"][1]["kind"], "chat");
        assert_eq!(value["chat"][1]["username"], "Alex");
        assert_eq!(
            shown_game_message(&hud).as_deref(),
            Some("Steve died"),
            "a chat line from a player is not the death message"
        );
    }

    #[test]
    fn the_death_screen_reports_its_message_and_respawn_button() {
        let stack = |screens: &[&str]| SceneStack {
            id: 1,
            screens: screens.iter().map(|s| (*s).to_owned()).collect(),
            frames_since_change: 0,
        };
        assert_eq!(
            death_screen(&[stack(&["in_game_play_screen", "hud_screen"])]),
            None
        );
        let stacks = [stack(&[
            "in_game_play_screen",
            "hud_screen",
            "death_screen",
        ])];
        assert_eq!(death_screen(&stacks), Some("death_screen"));
        let node = |parent: Option<u32>, name: &str, rect: [f32; 4]| UiNode {
            parent,
            name: name.into(),
            rect,
            alpha: 1.0,
            visible: true,
            enabled: true,
            hover: false,
            clips: false,
            components: Vec::new(),
            text: None,
            checked: None,
            child_count: 0,
        };
        let tree = UiScreen {
            stack: 1,
            scene: "death_screen".into(),
            nodes: vec![
                node(None, "root", [0.0, 0.0, 400.0, 200.0]),
                UiNode {
                    components: vec!["TextComponent".into()],
                    text: Some("Steve fell from a high place".into()),
                    ..node(Some(0), "message", [100.0, 40.0, 200.0, 10.0])
                },
                UiNode {
                    components: vec!["ButtonComponent".into()],
                    ..node(Some(0), "respawn_button", [100.0, 100.0, 200.0, 20.0])
                },
                UiNode {
                    components: vec!["TextComponent".into()],
                    text: Some("Respawn".into()),
                    ..node(Some(2), "label", [150.0, 105.0, 100.0, 10.0])
                },
            ],
            truncated: false,
            detail: None,
            read_us: 1,
            fresh: true,
        };
        let death = death_from_tree("death_screen", &[tree]);
        assert_eq!(death.texts, ["Steve fell from a high place"]);
        assert_eq!(death.respawn, Some(true));
        let value = death_json(&death);
        assert_eq!(value["message"], "Steve fell from a high place");
        assert_eq!(value["screen"], "death_screen");
        assert_eq!(
            death_from_tree("death_screen", &[]).respawn,
            None,
            "no tree: unknown"
        );
    }
}
