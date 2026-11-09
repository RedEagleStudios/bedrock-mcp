//! Messages between the bedrock-mcp server and the agent loaded into Minecraft.
//!
//! The agent listens on a random loopback TCP port and writes an
//! [`Endpoint`] file next to its DLL. A client sends one JSON [`Request`] per
//! line, starting with [`Request::Hello`], and reads one JSON [`Reply`] line
//! for each.

use serde::{Deserialize, Serialize};

/// Bumped whenever a request or reply changes shape.
pub const PROTOCOL_VERSION: u32 = 5;
/// File name of the agent DLL that `cargo build` produces for the agent crate.
pub const DLL_NAME: &str = "bedrock_agent.dll";
/// Written by the agent next to its DLL once it listens.
pub const ENDPOINT_FILE: &str = "bedrock-agent.json";
/// The agent's own log, next to its DLL.
pub const LOG_FILE: &str = "bedrock-agent.log";
/// Longest request line the agent accepts.
pub const MAX_LINE: usize = 4096;

/// GameInput mouse button bits (`GameInputMouseButtons`).
pub mod mouse {
    pub const LEFT: u32 = 0x01;
    pub const RIGHT: u32 = 0x02;
    pub const MIDDLE: u32 = 0x04;
    pub const BACK: u32 = 0x08;
    pub const FORWARD: u32 = 0x10;
    pub const ALL: u32 = 0x1f;
}

/// The shared-memory file the agent fills with presented frames.
///
/// Layout: a [`Header`](frames::Header), then `slots` [`Slot`](frames::Slot)s, then the
/// pixel slots at `pixels_offset`, each `slot_bytes` long with rows `width * 4` bytes apart.
/// The agent publishes frame `n` (from 0) in slot `n % slots`: it sets the slot's `seq` to
/// [`frames::WRITING`], writes the pixels and metadata, sets `seq` to `n + 1`, then stores
/// `written = n + 1`. Readers copy a slot and keep it only if `seq` was `n + 1` before and
/// after the copy.
pub mod frames {
    pub const MAGIC: u32 = u32::from_le_bytes(*b"MCFR");
    pub const VERSION: u32 = 1;
    /// Present times kept in the header's ring, one per presented frame.
    pub const PRESENT_RING: usize = 4096;
    /// A slot's `seq` while the agent writes it.
    pub const WRITING: u64 = u64::MAX;
    /// Pixel slots in the file.
    pub const SLOTS: u32 = 8;
    /// Byte alignment of the pixel area and of each pixel slot.
    pub const ALIGN: u64 = 4096;

    /// Pixel formats, as `Header::format`.
    pub mod format {
        /// 8-bit B, G, R, A bytes.
        pub const BGRA8: u32 = 0;
        /// 8-bit R, G, B, A bytes.
        pub const RGBA8: u32 = 1;
        /// Little-endian 32-bit words, R in the low 10 bits, then G, B and 2 bits of alpha.
        pub const RGB10A2: u32 = 2;
    }

    /// `Header::state` values.
    pub mod state {
        pub const RUNNING: u32 = 1;
        pub const STOPPED: u32 = 2;
        /// The frame size or format changed; later frames are not written.
        pub const RESIZED: u32 = 3;
    }

    #[repr(C)]
    pub struct Header {
        pub magic: u32,
        pub version: u32,
        pub width: u32,
        pub height: u32,
        pub format: u32,
        pub slots: u32,
        pub slot_bytes: u64,
        pub pixels_offset: u64,
        /// Frames published so far.
        pub written: u64,
        /// Frames the agent skipped because no copy buffer was free.
        pub dropped: u64,
        /// Frames the game presented while capturing, captured or not.
        pub presents: u64,
        pub state: u32,
        pub reserved: u32,
        /// Present time of present `n` at `n % PRESENT_RING`, in nanoseconds on the agent's
        /// monotonic clock.
        pub present_ns: [u64; PRESENT_RING],
    }

    #[repr(C)]
    pub struct Slot {
        pub seq: u64,
        /// Present time, as in [`Header::present_ns`].
        pub present_ns: u64,
        /// Which present this frame is, counted like [`Header::presents`].
        pub present: u64,
        pub reserved: u64,
    }

    /// Byte offset of the pixel area for `slots` slots.
    pub const fn pixels_offset(slots: u32) -> u64 {
        let end = (size_of::<Header>() + slots as usize * size_of::<Slot>()) as u64;
        end.div_ceil(ALIGN) * ALIGN
    }

    /// Bytes per pixel slot for a `width` × `height` frame.
    pub const fn slot_bytes(width: u32, height: u32) -> u64 {
        (width as u64 * height as u64 * 4).div_ceil(ALIGN) * ALIGN
    }

    /// File size for `slots` slots of `width` × `height` frames.
    pub const fn file_size(width: u32, height: u32, slots: u32) -> u64 {
        pixels_offset(slots) + slots as u64 * slot_bytes(width, height)
    }
}

/// Where a running agent listens.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub port: u16,
    pub token: String,
    pub pid: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrashKind {
    /// A Rust panic in the agent; the agent is built with `panic = "abort"`.
    Panic,
    /// A write through a null pointer.
    AccessViolation,
    /// Unbounded recursion on the agent's thread.
    StackOverflow,
    /// `std::process::abort`.
    Abort,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Must be the first request on a connection.
    Hello {
        token: String,
        protocol: u32,
    },
    Status,
    /// Post a key transition for Windows virtual key `vk` to the game window.
    Key {
        down: bool,
        vk: u8,
        scan: u8,
        extended: bool,
    },
    /// Post one Unicode scalar value as text input.
    Char {
        code: u32,
    },
    /// Add relative mouse motion, in mouse counts, to every later reading.
    Move {
        dx: i64,
        dy: i64,
    },
    /// Add wheel motion, in GameInput units (120 per notch).
    Wheel {
        dx: i64,
        dy: i64,
    },
    /// Hold exactly these [`mouse`] buttons.
    Buttons {
        mask: u32,
    },
    /// Report this absolute pointer position, in client pixels.
    Pointer {
        x: i64,
        y: i64,
    },
    /// Report the real pointer position again.
    PointerRelease,
    /// Release every key, button and pointer override this client holds.
    Reset,
    /// Crash the game on purpose; the reply is sent first.
    Crash {
        kind: CrashKind,
    },
    /// Copy presented frames into a new shared-memory file at `path` (a Windows path), at
    /// most `fps` a second; 0 copies every frame. Present times of all frames are kept.
    /// `game_fps` above 0 also holds the game to that many presents a second until the
    /// capture stops.
    FramesStart {
        path: String,
        fps: u32,
        game_fps: u32,
    },
    /// Stop copying frames; the file stays valid.
    FramesStop,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub protocol: u32,
    pub pid: u32,
    /// Client area of the game window, in physical pixels.
    pub width: i32,
    pub height: i32,
    /// Whether the game window is really the foreground window.
    pub foreground: bool,
    /// Whether the game believes it is active. While a client is connected the agent keeps it
    /// active even when another window is in front.
    pub game_active: bool,
    /// Mouse readings the game has taken; it takes one per rendered frame.
    pub mouse_readings: u64,
    /// IGameInput interface versions the agent hooked.
    pub gameinput_versions: Vec<u32>,
    /// Real absolute pointer position from the last mouse reading.
    pub pointer: [i64; 2],
    pub pointer_override: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    Ok,
    Status(Status),
    Error { message: String },
}

/// One request or reply as a JSON line, newline included.
pub fn encode<T: Serialize>(message: &T) -> String {
    let mut line = serde_json::to_string(message).expect("protocol messages always serialize");
    line.push('\n');
    line
}

pub fn decode<'a, T: Deserialize<'a>>(line: &'a str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line.trim_end_matches(['\r', '\n']))
}

/// The optional unstable layer: `bedrock_unstable.dll` reaches game internals
/// through vtables it finds by RTTI or by string anchors. It runs its own
/// endpoint so a failure there never touches the stable agent; each
/// capability reports its own state.
pub mod unstable {
    use serde::{Deserialize, Serialize};

    /// Bumped whenever an unstable request or reply changes shape.
    pub const PROTOCOL_VERSION: u32 = 7;
    pub const DLL_NAME: &str = "bedrock_unstable.dll";
    pub const ENDPOINT_FILE: &str = "bedrock-unstable.json";
    pub const LOG_FILE: &str = "bedrock-unstable.log";

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "cmd", rename_all = "snake_case")]
    pub enum Request {
        /// Must be the first request on a connection.
        Hello {
            token: String,
            protocol: u32,
        },
        Status,
        /// Content-log entries with `seq` greater than `since`, oldest first.
        ContentLog {
            since: u64,
            limit: u32,
        },
        /// The scene stacks as of the game's last frame.
        Screens,
        /// The UI control tree of the screen on top of each scene stack, read on the game
        /// thread on the next frame.
        UiTree {
            /// Only this stack (a [`SceneStack::id`]); every stack when absent.
            #[serde(default)]
            stack: Option<u64>,
            /// Levels below the root to read; the DLL caps it.
            #[serde(default)]
            max_depth: Option<u32>,
            /// Skip controls the game hides, with their children.
            #[serde(default)]
            visible_only: bool,
        },
        /// The camera: view perspective, field of view and the last rendered view.
        Camera,
        /// HUD text: toasts, title, subtitle, action bar, tip and chat, read on the game
        /// thread on the next frame.
        Hud {
            /// Most chat lines to return, newest last; the DLL caps it.
            #[serde(default)]
            chat_limit: Option<u32>,
        },
        /// Entities on the game's own local server (single player), read on the server
        /// thread at its next tick. The DLL answers with an error when no local server ticks.
        WorldEntities {
            /// Only the entity with this unique id; the other filters are ignored.
            #[serde(default)]
            id: Option<i64>,
            /// The player whose position is the centre; the first player when absent.
            #[serde(default)]
            player: Option<String>,
            /// Only entities within this many blocks of the centre, in its dimension.
            #[serde(default)]
            radius: Option<f64>,
            /// Only entities of this type, e.g. `minecraft:cow`.
            #[serde(default)]
            entity_type: Option<String>,
            /// The nearest this many; the DLL caps it.
            #[serde(default)]
            limit: Option<u32>,
        },
        /// One player on the game's own local server (single player), with the state only a
        /// player has, read on the server thread at its next tick.
        WorldPlayer {
            /// The player whose name tag is this; the only player when absent.
            #[serde(default)]
            name: Option<String>,
        },
        /// Dynamic properties of the world or an entity on the local server, every pack's
        /// collections, read on the server thread at its next tick.
        DynamicProperties {
            target: PropertyTarget,
            /// Only the collection with this name.
            #[serde(default)]
            collection: Option<String>,
        },
        /// The pages of the live Ore UI views, read on the game thread on their next frames.
        OreUi {
            /// Only views whose URL contains this text.
            #[serde(default)]
            url: Option<String>,
        },
        /// The local player's animation components and their attachables, each read on the
        /// thread that evaluates it, when it is next evaluated.
        PlayerRender {
            /// Wait up to this long for each component to be evaluated again; the DLL caps it.
            #[serde(default)]
            wait_ms: Option<u32>,
            /// The local player's actor unique id, when the caller knows it.
            #[serde(default)]
            actor: Option<i64>,
        },
        /// Recent frame and game update timings.
        Perf,
    }

    /// Whose dynamic properties to read.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    pub enum PropertyTarget {
        World,
        /// The player with this name, or the first player.
        Player {
            #[serde(default)]
            name: Option<String>,
        },
        Entity {
            id: i64,
        },
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum CapabilityState {
        /// Resolved, installed and confirmed by a live call.
        Active,
        /// Installed; waiting for the first call that confirms it.
        Pending,
        /// Not attempted on this build (missing class, slot count mismatch).
        Unavailable,
        /// Attempted and rejected by a self-check.
        Failed,
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct Capability {
        pub name: String,
        pub state: CapabilityState,
        pub detail: String,
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct Build {
        /// File version, e.g. `1.26.62.1`, when the image has one.
        pub version: Option<String>,
        /// PE header timestamp and image size: together they identify a build.
        pub timestamp: u32,
        pub image_size: u32,
        /// Classes with RTTI it uses; zero on builds that do not ship it, or when
        /// it is told to find classes by string anchors only.
        pub rtti_classes: usize,
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct Status {
        pub protocol: u32,
        pub pid: u32,
        pub build: Build,
        pub capabilities: Vec<Capability>,
    }

    /// One content-log message as the game passed it to its log endpoints.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct LogEntry {
        pub seq: u64,
        /// The game's `LogArea` and `LogLevel` values.
        pub area: u32,
        pub level: u32,
        pub message: String,
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct LogBatch {
        pub entries: Vec<LogEntry>,
        /// Pass as `since` next time.
        pub next: u64,
        /// Entries older than the buffer that were dropped after `since`.
        pub dropped: u64,
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct SceneStack {
        /// Address of the stack object, to tell stacks apart.
        pub id: u64,
        /// Screen names, bottom first; the last is on top.
        pub screens: Vec<String>,
        /// Game frames since this stack last changed.
        pub frames_since_change: u64,
    }

    /// One UI control. `parent` indexes the same [`UiScreen::nodes`] list, which is in
    /// pre-order: a parent comes before its children.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct UiNode {
        pub parent: Option<u32>,
        pub name: String,
        /// Absolute position and size in UI units: x, y, width, height.
        pub rect: [f32; 4],
        pub alpha: f32,
        /// The control's own visibility flag.
        pub visible: bool,
        pub enabled: bool,
        pub hover: bool,
        /// The control clips its children to its rectangle (scroll view ports do).
        pub clips: bool,
        /// Short class names of the control's components, e.g. `ButtonComponent`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub components: Vec<String>,
        /// What a text component shows.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub text: Option<String>,
        /// A toggle's checked state.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub checked: Option<bool>,
        /// Children the control has, read or not.
        pub child_count: u32,
    }

    /// The UI tree of the screen on top of one scene stack.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct UiScreen {
        /// The [`SceneStack::id`] of its stack.
        pub stack: u64,
        /// Screen name, as in [`SceneStack::screens`].
        pub scene: String,
        /// Pre-order controls from the root; empty when the screen has no control tree
        /// (for example a screen drawn by another UI system).
        pub nodes: Vec<UiNode>,
        /// A bound (depth, nodes, memory checks) cut the tree short.
        pub truncated: bool,
        /// Why the tree is empty or short, when it is.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub detail: Option<String>,
        /// Microseconds the read took on the game thread.
        pub read_us: u64,
        /// Read on the frame that answered this request, not an older one.
        pub fresh: bool,
    }

    /// The player's view perspective option, as the game's `PlayerViewMode`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum Perspective {
        FirstPerson,
        ThirdPersonBack,
        ThirdPersonFront,
    }

    /// The camera the client renders the world with, as last read.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct CameraView {
        /// World position, in blocks.
        pub position: [f32; 3],
        /// Unit vectors of the view: where it looks and which way is up.
        pub forward: [f32; 3],
        pub up: [f32; 3],
        /// Vertical field of view in degrees and the aspect ratio, when the camera holds
        /// plausible ones.
        pub fov: Option<f32>,
        pub aspect_ratio: Option<f32>,
        /// Milliseconds since it was read.
        pub age_ms: u64,
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct CameraState {
        /// The perspective option the camera last read; None until it reads one.
        pub perspective: Option<Perspective>,
        /// Milliseconds since the camera last read the perspective.
        pub perspective_age_ms: Option<u64>,
        /// Field of view in degrees that the camera API last reported.
        pub fov: Option<f32>,
        pub fov_age_ms: Option<u64>,
        pub view: Option<CameraView>,
        /// Why a part is missing, when one is.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub detail: Option<String>,
    }

    /// One toast the game queued, read as it was pushed.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct Toast {
        pub seq: u64,
        /// Unix time in milliseconds when the game pushed it.
        pub at_ms: u64,
        /// The game's `ToastMessageType` number.
        pub kind: u32,
        pub title: String,
        pub subtitle: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        pub message_id: String,
        /// Pushed as the persistent toast.
        pub persistent: bool,
    }

    /// A HUD text and when it last changed.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct HudText {
        pub text: String,
        /// Unix time in milliseconds when the DLL first saw this text; None when it was
        /// already there when the DLL started watching.
        pub changed_ms: Option<u64>,
    }

    /// One line of the HUD's message list (chat, system and command messages).
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct ChatLine {
        /// The game's `GuiMessage::MessageType` number.
        pub kind: u32,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        pub username: String,
        pub message: String,
        /// The line as drawn, with the sender.
        pub full: String,
        /// Seconds the line stays on the HUD; counts down.
        pub duration: f32,
        pub seen: bool,
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct HudState {
        /// Toasts pushed since the DLL started, oldest first, at most the last 32.
        pub toasts: Vec<Toast>,
        /// The `/title` texts; they stay set after they fade.
        pub title: Option<HudText>,
        pub subtitle: Option<HudText>,
        pub actionbar: Option<HudText>,
        /// Fade in, stay and fade out of the title, in game ticks.
        pub title_ticks: Option<[i32; 3]>,
        /// The item-name popup and the tip text.
        pub popup: Option<HudText>,
        pub tip: Option<HudText>,
        pub chat: Vec<ChatLine>,
        /// Whether the player the camera follows is dead, from the camera's damage
        /// parameters; None when the camera did not ask for them in the last second.
        pub dead: Option<bool>,
        /// Game ticks since that player died, with the frame's fraction.
        pub death_ticks: Option<f32>,
        /// Why the text fields are missing, when they are.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub detail: Option<String>,
        /// The text was read on the frame that answered this request.
        pub fresh: bool,
    }

    /// One element of an Ore UI page. `parent` indexes the same [`OreUiView::elements`] list,
    /// which is in document order: a parent comes before its children.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct OreUiElement {
        /// The nearest ancestor that is in the list.
        #[serde(default)]
        pub parent: Option<u32>,
        /// Lower-case tag name.
        pub tag: String,
        /// `data-focusable-id`: the page lets focus navigation (and clicks) reach it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub focus_id: Option<String>,
        /// `data-testid`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub test_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub role: Option<String>,
        /// The `type` of an `input`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub input_type: Option<String>,
        /// The element's own text (its text nodes, not its children's), or its accessible
        /// label when it has none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub text: Option<String>,
        /// x, y, width, height in CSS pixels of the view's viewport.
        pub rect: [f32; 4],
        #[serde(default)]
        pub disabled: bool,
        /// A toggle's checked state.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub checked: Option<bool>,
        /// It scrolls its content and clips it to its rectangle.
        #[serde(default)]
        pub scrolls: bool,
        /// It has the page's focus.
        #[serde(default)]
        pub focused: bool,
    }

    /// One live Ore UI view and its page.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct OreUiView {
        /// The page the view loaded, as the game names it.
        #[serde(default)]
        pub url: String,
        /// The page's `location.href`, with its route.
        #[serde(default)]
        pub href: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        pub title: String,
        /// The game's `OreUI::ViewState` number: 1 active, 2 active and painted.
        #[serde(default)]
        pub state: u8,
        /// `innerWidth` and `innerHeight` in CSS pixels; the elements' rectangles use them.
        #[serde(default)]
        pub viewport: [f32; 2],
        /// Elements that are visible and either interactive, labelled for tests, scrolling
        /// or showing text of their own.
        #[serde(default)]
        pub elements: Vec<OreUiElement>,
        /// A bound (elements scanned or kept) cut the list short.
        #[serde(default)]
        pub truncated: bool,
        /// Why the page is missing, when it is.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub detail: Option<String>,
        /// Microseconds the page's script ran on the game thread.
        #[serde(default)]
        pub read_us: u64,
    }

    /// One animation or animation controller playing on a component.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct AnimationPlayer {
        /// `controller`, `animation` (a skeletal animation) or `unknown`.
        pub kind: String,
        /// The name the entity's or attachable's definition gives it.
        pub name: String,
        /// A controller's current state.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub state: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub state_index: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub last_state_index: Option<i32>,
        /// Seconds the controller has been in its current state.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub state_time: Option<f32>,
        /// The animations the current state plays.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub animations: Vec<String>,
    }

    /// When a component's own hook last saw the game evaluate it.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct Visits {
        pub count: u64,
        pub age_ms: u64,
        /// The Windows thread id of the last call.
        pub thread: u32,
    }

    /// One client animation component.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct AnimationComponent {
        /// Address of the component, to tell components apart.
        pub id: u64,
        /// The game's `AnimationComponentGroupType`: 0 client, 1 client HUD, 2 server,
        /// 3 client map, 4 client remote, 5 client first person.
        pub group: u32,
        /// The game's `AnimationComponentID`: the owning actor's unique id in the low 54 bits,
        /// the attachable slot and sub-client above.
        pub owner: u64,
        /// The animation frame of the component's last update.
        pub last_update_frame: i64,
        pub initialized: bool,
        /// Calls to `visitApplyContext`, where the game applies its animations to the pose.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub applied: Option<Visits>,
        /// Milliseconds since this read.
        pub read_age_ms: u64,
        /// Microseconds the read took on the evaluating thread; zero for a child, whose read
        /// is part of its parent's.
        pub read_us: u64,
        pub players: Vec<AnimationPlayer>,
        /// Child components (attachables), read through this one.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub children: Vec<AnimationComponent>,
        /// Why a part could not be read.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub detail: Option<String>,
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct PlayerRender {
        /// The highest `last_update_frame` seen on any client animation component.
        pub frame: i64,
        /// The local player's components (first-person, third-person and HUD, and attachables
        /// the game evaluates on their own), each with its child attachables.
        pub components: Vec<AnimationComponent>,
        /// Hook calls since load per `group` value (index 0 to 5).
        pub visits_by_group: Vec<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub detail: Option<String>,
    }

    /// Timing samples, oldest first, each `[age_ms, value_ms]`.
    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    pub struct Perf {
        /// Time between the starts of consecutive game frames (`MinecraftGame::update`).
        pub frame: Vec<[f32; 2]>,
        /// Wall time of each `MinecraftGame::update` call.
        pub update: Vec<[f32; 2]>,
        /// The game's own update duration (`getGameUpdateDurationInSeconds`) after each frame.
        pub game_update: Vec<[f32; 2]>,
        /// Time between consecutive `SceneStack::update` calls of one stack.
        pub scene: Vec<[f32; 2]>,
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "reply", rename_all = "snake_case")]
    pub enum Reply {
        Ok,
        Status(Status),
        ContentLog(LogBatch),
        Screens { stacks: Vec<SceneStack> },
        UiTree { screens: Vec<UiScreen> },
        Camera(CameraState),
        Hud(HudState),
        Error { message: String },
        WorldEntities(EntitySnapshot),
        WorldPlayer(NativePlayer),
        DynamicProperties(PropertySnapshot),
        OreUi { views: Vec<OreUiView> },
        PlayerRender(PlayerRender),
        Perf(Perf),
    }

    /// One entity as the local server holds it.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct NativeEntity {
        /// The unique id, as the Script API's `Entity.id` spells it in decimal.
        pub id: i64,
        /// The type identifier, e.g. `minecraft:cow`.
        pub type_id: Option<String>,
        /// The synced name tag; a player's carries the player name.
        pub name_tag: Option<String>,
        pub player: bool,
        /// The dimension number: 0 overworld, 1 nether, 2 the end.
        pub dimension: Option<i32>,
        /// Feet position in blocks.
        pub position: [f32; 3],
        /// Pitch and yaw in degrees, when the entity has a rotation.
        pub rotation: Option<[f32; 2]>,
        /// Current and maximum health, for entities with a health attribute.
        pub health: Option<[f32; 2]>,
        /// Blocks from the centre, when the entity is in the centre's dimension.
        pub distance: Option<f32>,
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct EntitySnapshot {
        /// The server tick that answered.
        pub tick: u64,
        /// The player the distances are measured from, if one was found.
        pub centre: Option<NativeEntity>,
        /// Matching entities, nearest first, without the centre.
        pub entities: Vec<NativeEntity>,
        /// Matching entities before `limit` cut the list.
        pub total: u32,
        /// Entities that failed a layout check and were left out.
        pub skipped: u32,
        /// Microseconds the read took on the server thread.
        pub read_us: u64,
    }

    /// One player as the local server holds it. A field the read could not check is `None`
    /// and `notes` says why.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct NativePlayer {
        /// The server tick that answered.
        pub tick: u64,
        /// What every entity read gives: id, name tag, dimension, feet, rotation, health.
        pub entity: NativeEntity,
        /// `minecraft:player.hunger`: current and maximum.
        pub hunger: Option<[f32; 2]>,
        /// `minecraft:player.saturation`: current and maximum.
        pub saturation: Option<[f32; 2]>,
        /// The selected hotbar slot, 0 to 8.
        pub selected_slot: Option<u8>,
        /// Active effects; `None` when they could not be read.
        pub effects: Option<Vec<NativeEffect>>,
        /// How many players the local server holds.
        pub players: u32,
        /// Why a field is missing.
        pub notes: Vec<String>,
        /// Microseconds the read took on the server thread.
        pub read_us: u64,
    }

    /// One active effect: a `MobEffectInstance`.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct NativeEffect {
        /// The game's numeric effect id (`MobEffectIds`), 1 is speed.
        pub id: u32,
        /// 0 for level I.
        pub amplifier: i32,
        /// Remaining ticks; negative when the effect does not run out.
        pub duration: i32,
    }

    /// A dynamic property value, typed as the game stores it.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "type", content = "value", rename_all = "snake_case")]
    pub enum PropertyValue {
        Double(f64),
        Float(f32),
        Bool(bool),
        String(String),
        Vector([f32; 3]),
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct Property {
        pub key: String,
        pub value: PropertyValue,
    }

    /// One collection: the properties stored under one collection name.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct PropertyCollection {
        /// The name the game keys the collection by.
        pub name: String,
        /// The game's count of the collection's stored bytes.
        pub byte_count: u64,
        pub properties: Vec<Property>,
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct PropertySnapshot {
        pub tick: u64,
        /// The entity read, for entity and player targets.
        pub entity: Option<i64>,
        pub collections: Vec<PropertyCollection>,
        /// A bound or a failed check left properties out.
        pub truncated: bool,
        /// Microseconds the read took on the server thread.
        pub read_us: u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_as_single_lines() {
        let requests = [
            Request::Hello {
                token: "t".into(),
                protocol: PROTOCOL_VERSION,
            },
            Request::Key {
                down: true,
                vk: 0x57,
                scan: 0x11,
                extended: false,
            },
            Request::Pointer { x: -1, y: 2 },
            Request::PointerRelease,
            Request::Crash {
                kind: CrashKind::StackOverflow,
            },
        ];
        for request in requests {
            let line = encode(&request);
            assert_eq!(line.matches('\n').count(), 1);
            assert_eq!(decode::<Request>(&line).unwrap(), request);
        }
    }

    #[test]
    fn unknown_commands_are_rejected() {
        assert!(decode::<Request>(r#"{"cmd":"format_disk"}"#).is_err());
    }

    #[test]
    fn ui_tree_requests_default_their_options() {
        assert_eq!(
            decode::<unstable::Request>(r#"{"cmd":"ui_tree"}"#).unwrap(),
            unstable::Request::UiTree {
                stack: None,
                max_depth: None,
                visible_only: false,
            }
        );
        let reply = unstable::Reply::UiTree {
            screens: vec![unstable::UiScreen {
                stack: 1,
                scene: "pause_screen".into(),
                nodes: vec![unstable::UiNode {
                    parent: None,
                    name: "root".into(),
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
                read_us: 10,
                fresh: true,
            }],
        };
        let line = encode(&reply);
        assert!(!line.contains("components") && !line.contains("detail"));
        assert_eq!(decode::<unstable::Reply>(&line).unwrap(), reply);
    }
}
