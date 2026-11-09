//! A scripted unstable host and endpoint for tests.

use std::sync::{Arc, Mutex};

use protocol::unstable::{
    Build, CameraState, Capability, CapabilityState, EntitySnapshot, HudState, LogBatch, LogEntry,
    NativeEffect, NativeEntity, NativePlayer, OreUiView, PROTOCOL_VERSION, Perf, Perspective,
    PlayerRender, PropertyCollection, PropertySnapshot, PropertyTarget, Reply, Request, SceneStack,
    Status, UiScreen,
};

use super::{Host, Link};
use crate::error::{Error, Result};

/// What the fake game and DLL do; shared so tests can change it between calls.
#[derive(Clone)]
pub struct Script {
    pub pid: Option<u32>,
    pub setting: Option<String>,
    /// `Err` makes attaching fail with this message.
    pub attach: Result<(), String>,
    /// Name, state and the DLL's detail of each capability.
    pub capabilities: Vec<(&'static str, CapabilityState, &'static str)>,
    pub log: Vec<LogEntry>,
    /// Appended to `log` once a content-log request comes back empty (after a caller
    /// found the end), so waits see them arrive.
    pub arrivals: Vec<LogEntry>,
    pub stacks: Vec<SceneStack>,
    /// What a ui_tree request returns, before its stack filter.
    pub ui_screens: Vec<UiScreen>,
    pub camera: CameraState,
    pub hud: HudState,
    /// Requests after which the connection breaks.
    pub disconnect_after: Option<usize>,
    /// Entities a world_entities request returns (filtered by id when it names one);
    /// `None` answers that no local server ticked.
    pub world_entities: Option<Vec<NativeEntity>>,
    /// The player a world_player request returns when it names no one or this player's name
    /// tag; `None` answers that no local server ticked.
    pub world_player: Option<NativePlayer>,
    /// Collections a dynamic_properties request returns, before its collection filter.
    pub dynamic_properties: Vec<PropertyCollection>,
    pub attaches: usize,
    /// What an oreui request returns, before its URL filter.
    pub oreui_views: Vec<OreUiView>,
    pub requests: Vec<Request>,
    /// What a player_render request returns.
    pub player_render: PlayerRender,
    /// What a perf request returns.
    pub perf: Perf,
}

impl Script {
    /// A game with every capability active.
    pub fn working() -> Self {
        Self {
            pid: Some(4242),
            setting: None,
            attach: Ok(()),
            capabilities: vec![
                ("content_log", CapabilityState::Active, ""),
                ("screens", CapabilityState::Active, ""),
                ("ui_tree", CapabilityState::Active, ""),
                ("camera", CapabilityState::Active, ""),
                ("hud_text", CapabilityState::Active, ""),
                ("world_state", CapabilityState::Active, ""),
                ("oreui", CapabilityState::Active, ""),
                ("player_render", CapabilityState::Active, ""),
                ("perf", CapabilityState::Active, ""),
            ],
            log: Vec::new(),
            arrivals: Vec::new(),
            stacks: Vec::new(),
            ui_screens: Vec::new(),
            camera: camera(),
            hud: hud(),
            disconnect_after: None,
            attaches: 0,
            world_entities: None,
            world_player: None,
            dynamic_properties: Vec::new(),
            oreui_views: Vec::new(),
            requests: Vec::new(),
            player_render: PlayerRender {
                frame: 0,
                components: Vec::new(),
                visits_by_group: Vec::new(),
                detail: None,
            },
            perf: Perf::default(),
        }
    }
}

pub type Shared = Arc<Mutex<Script>>;

pub struct FakeHost(pub Shared);

impl Host for FakeHost {
    fn game_pid(&self) -> Option<u32> {
        self.0.lock().unwrap().pid
    }

    fn setting(&self) -> Option<String> {
        self.0.lock().unwrap().setting.clone()
    }

    fn attach(&self, _pid: u32) -> Result<Box<dyn Link>> {
        let mut script = self.0.lock().unwrap();
        script.attaches += 1;
        script.attach.clone().map_err(Error::msg)?;
        Ok(Box::new(FakeLink(Arc::clone(&self.0))))
    }
}

pub struct FakeLink(Shared);

impl Link for FakeLink {
    fn request(&mut self, request: &Request) -> Result<Reply> {
        let mut script = self.0.lock().unwrap();
        if script
            .disconnect_after
            .is_some_and(|limit| script.requests.len() >= limit)
        {
            return Err(Error::Disconnected("closed".into()));
        }
        script.requests.push(request.clone());
        Ok(match request {
            Request::Hello { .. } => Reply::Ok,
            Request::Status => Reply::Status(Status {
                protocol: PROTOCOL_VERSION,
                pid: 77,
                build: Build {
                    version: Some("1.0.0.0".into()),
                    timestamp: 0x1234_5678,
                    image_size: 0x0100_0000,
                    rtti_classes: 1000,
                },
                capabilities: script
                    .capabilities
                    .iter()
                    .map(|(name, state, detail)| Capability {
                        name: (*name).into(),
                        state: *state,
                        detail: (*detail).into(),
                    })
                    .collect(),
            }),
            Request::ContentLog { since, limit } => {
                let entries: Vec<LogEntry> = script
                    .log
                    .iter()
                    .filter(|entry| entry.seq > *since)
                    .take(*limit as usize)
                    .cloned()
                    .collect();
                let next = entries.last().map_or(*since, |entry| entry.seq);
                if entries.is_empty() {
                    let arrivals = std::mem::take(&mut script.arrivals);
                    script.log.extend(arrivals);
                }
                Reply::ContentLog(LogBatch {
                    entries,
                    next,
                    dropped: 0,
                })
            }
            Request::Screens => Reply::Screens {
                stacks: script.stacks.clone(),
            },
            Request::UiTree { stack, .. } => Reply::UiTree {
                screens: script
                    .ui_screens
                    .iter()
                    .filter(|screen| stack.is_none_or(|only| only == screen.stack))
                    .cloned()
                    .collect(),
            },
            Request::Camera => Reply::Camera(script.camera.clone()),
            Request::Hud { .. } => Reply::Hud(script.hud.clone()),
            Request::WorldEntities { id, .. } => match &script.world_entities {
                Some(entities) => {
                    let entities: Vec<NativeEntity> = entities
                        .iter()
                        .filter(|entity| id.is_none_or(|id| id == entity.id))
                        .cloned()
                        .collect();
                    Reply::WorldEntities(EntitySnapshot {
                        tick: 1,
                        centre: None,
                        total: entities.len() as u32,
                        entities,
                        skipped: 0,
                        read_us: 1,
                    })
                }
                None => Reply::Error {
                    message: "the game's local server did not tick".into(),
                },
            },
            Request::WorldPlayer { name } => match &script.world_player {
                Some(player)
                    if name
                        .as_ref()
                        .is_none_or(|name| player.entity.name_tag.as_ref() == Some(name)) =>
                {
                    Reply::WorldPlayer(player.clone())
                }
                Some(_) => Reply::Error {
                    message: format!("no player named {name:?} is on the local server"),
                },
                None => Reply::Error {
                    message: "the game's local server did not tick".into(),
                },
            },
            Request::DynamicProperties { target, collection } => {
                Reply::DynamicProperties(PropertySnapshot {
                    tick: 1,
                    entity: match target {
                        PropertyTarget::Entity { id } => Some(*id),
                        _ => None,
                    },
                    collections: script
                        .dynamic_properties
                        .iter()
                        .filter(|found| collection.as_ref().is_none_or(|only| *only == found.name))
                        .cloned()
                        .collect(),
                    truncated: false,
                    read_us: 1,
                })
            }
            Request::OreUi { url } => Reply::OreUi {
                views: script
                    .oreui_views
                    .iter()
                    .filter(|view| url.as_deref().is_none_or(|url| view.url.contains(url)))
                    .cloned()
                    .collect(),
            },
            Request::PlayerRender { .. } => Reply::PlayerRender(script.player_render.clone()),
            Request::Perf => Reply::Perf(script.perf.clone()),
        })
    }
}

pub fn entry(seq: u64, level: u32, message: &str) -> LogEntry {
    LogEntry {
        seq,
        area: 1,
        level,
        message: message.into(),
    }
}

pub fn stack(screens: &[&str]) -> SceneStack {
    SceneStack {
        id: 1,
        screens: screens.iter().map(|screen| (*screen).to_owned()).collect(),
        frames_since_change: 0,
    }
}

/// A screen whose tree is only its root, 400×200 UI units.
pub fn ui_screen(stack: u64, scene: &str) -> UiScreen {
    UiScreen {
        stack,
        scene: scene.into(),
        nodes: vec![protocol::unstable::UiNode {
            parent: None,
            name: scene.into(),
            rect: [0.0, 0.0, 400.0, 200.0],
            alpha: 1.0,
            visible: true,
            enabled: true,
            hover: false,
            clips: false,
            components: Vec::new(),
            text: None,
            checked: None,
            child_count: 0,
        }],
        truncated: false,
        detail: None,
        read_us: 1,
        fresh: true,
    }
}

/// A camera in first person that has read nothing else.
pub fn camera() -> CameraState {
    CameraState {
        perspective: Some(Perspective::FirstPerson),
        perspective_age_ms: Some(5),
        fov: Some(70.0),
        fov_age_ms: Some(5),
        view: None,
        detail: None,
    }
}

/// A HUD with no text, whose camera has not said whether the player is alive.
pub fn hud() -> HudState {
    HudState {
        toasts: Vec::new(),
        title: None,
        subtitle: None,
        actionbar: None,
        title_ticks: None,
        popup: None,
        tip: None,
        chat: Vec::new(),
        dead: None,
        death_ticks: None,
        detail: None,
        fresh: true,
    }
}

/// Steve alone on the local server: feet at (1.5, 64, -2.5) in the overworld facing yaw 90,
/// slot 2 selected, hunger 17, with speed II for 600 ticks.
pub fn native_player() -> NativePlayer {
    NativePlayer {
        tick: 1,
        entity: NativeEntity {
            id: -4_294_967_295,
            type_id: Some("minecraft:player".into()),
            name_tag: Some("Steve".into()),
            player: true,
            dimension: Some(0),
            position: [1.5, 64.0, -2.5],
            rotation: Some([10.0, 90.0]),
            health: Some([18.0, 20.0]),
            distance: None,
        },
        hunger: Some([17.0, 20.0]),
        saturation: Some([3.5, 20.0]),
        selected_slot: Some(2),
        effects: Some(vec![NativeEffect {
            id: 1,
            amplifier: 1,
            duration: 600,
        }]),
        players: 1,
        notes: Vec::new(),
        read_us: 1,
    }
}
