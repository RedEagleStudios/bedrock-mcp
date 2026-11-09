//! UI trees: the controls of the screen on top of each `SceneStack`.
//!
//! The `screens` hook on `SceneStack::update` calls [`frame`] on the game thread after each
//! update of each stack. Each stack updates at least once per frame; the code answers a
//! request with the first update after it. It reads nothing unless a request is waiting or
//! the top scene changed, so a quiet update costs one atomic load. Every read happens on the
//! game thread, between the game's own updates, and only reads: it never frees, locks or
//! writes game memory. Each pointer is checked against committed, readable pages before it
//! is followed, and each object against a link back to itself, so a layout that moved stops
//! the walk instead of crashing the game.
//!
//! The path from a scene to its controls, with layouts from LeviLamina's headers under
//! `src-client/mc/client/gui/`:
//! - `screens/UIScene.h`: a `UIScene` (a JSON UI screen) owns a `ScreenView`
//!   (`mScreenView`, after `AbstractScene` and `BaseScreen` in `screens/AbstractScene.h` and
//!   `screens/BaseScreen.h`).
//! - `screens/ScreenView.h`: `mVisualTree`, a `VisualTree` (`controls/VisualTree.h`) whose
//!   `mRootControl` is the root `UIControl`.
//! - `controls/UIControl.h`: position, size, alpha, flags, name, `mChildren` and
//!   `mComponents`. Its `enable_shared_from_this` base points back at the control itself.
//! - `controls/UIComponent.h`: each component's `mOwner` points back at its control; its
//!   vtable names its class through RTTI.
//! - `controls/RenderableComponent.h` and `controls/TextComponent.h`: `mLabel`, the text.
//! - `controls/ToggleComponent.h`: the `mChecked` bit.

use std::{
    collections::HashMap,
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use protocol::unstable::{CapabilityState, UiNode, UiScreen};

use crate::{capability, memory::Memory, vtable::Resolver};

const NAME: &str = "ui_tree";
const SCENE_CLASS: &str = "UIScene";
const SCENE_SLOTS: usize = 93;
const VISUAL_TREE_CLASS: &str = "VisualTree";
const VISUAL_TREE_SLOTS: usize = 15;
/// The components `components` reads fields of, or tools act on, with their
/// slot counts: found by string anchors on a build without RTTI for them.
const ANCHORED_COMPONENTS: [(&str, usize); 4] = [
    ("TextComponent", 18),
    ("ToggleComponent", 16),
    ("ButtonComponent", 16),
    ("ScrollViewComponent", 16),
];

/// `UIScene::mScreenView`, `ScreenView::mVisualTree`, `VisualTree::mRootControl`.
const SCENE_SCREEN_VIEW: usize = 64;
const SCREEN_VIEW_VISUAL_TREE: usize = 80;
const VISUAL_TREE_ROOT: usize = 8;

/// `UIControl` fields. The code reads `mCachedPosition` as the control's absolute position
/// while the position-dirty flag is clear, a best-effort reading of the field names.
const CONTROL_SELF: usize = 0;
const CONTROL_CACHED_POSITION: usize = 16;
const CONTROL_FLAGS: usize = 24;
const CONTROL_NAME: usize = 32;
const CONTROL_RELATIVE_POSITION: usize = 64;
const CONTROL_SIZE: usize = 72;
const CONTROL_ALPHA: usize = 96;
const CONTROL_CHILDREN: usize = 152;
const CONTROL_COMPONENTS: usize = 184;
/// Bytes read from a control: through `mComponents`.
const CONTROL_READ: usize = 208;

/// Bits of the 16 flag bits at [`CONTROL_FLAGS`], in declaration order.
const FLAG_POSITION_DIRTY: u16 = 1 << 0;
const FLAG_HOVER: u16 = 1 << 1;
/// The `Visible` bit of the two-bit `mVisible` flag; the other one is `VisibleInStackPanel`.
const FLAG_VISIBLE: u16 = 1 << 2;
const FLAG_ENABLED: u16 = 1 << 5;
const FLAG_CLIPS_CHILDREN: u16 = 1 << 8;

const COMPONENT_OWNER: usize = 8;
/// `TextComponent::mLabel` and `mLOCCachedText`. The code reads `mLOCCachedText` as the
/// localized text the component drew, a best-effort reading of the field name.
const TEXT_LABEL: usize = 328;
const TEXT_LOCALIZED: usize = 544;
const TOGGLE_FLAGS: usize = 144;

/// Hard bounds of one read.
const MAX_NODES: usize = 2000;
const MAX_DEPTH: u32 = 64;
const DEFAULT_DEPTH: u32 = 48;
const MAX_CHILDREN: usize = 4096;
const MAX_COMPONENTS: usize = 64;
const MAX_NAME: usize = 128;
const MAX_TEXT: usize = 512;
/// Larger than any UI the game lays out, in UI units.
const MAX_COORDINATE: f32 = 1.0e6;

/// How long a request waits for the game thread.
const WAIT: Duration = Duration::from_secs(1);

/// Vtables the walk recognizes, located at install.
struct Classes {
    scene: usize,
    visual_tree: usize,
    /// Primary vtable address → short class name, for every class named `*Component`.
    components: HashMap<usize, Arc<str>>,
}

static CLASSES: OnceLock<Classes> = OnceLock::new();

#[derive(Clone, Copy)]
struct Params {
    stack: Option<u64>,
    max_depth: u32,
    visible_only: bool,
}

const DEFAULT_PARAMS: Params = Params {
    stack: None,
    max_depth: DEFAULT_DEPTH,
    visible_only: true,
};

struct Wanted {
    generation: u64,
    params: Params,
}

/// The generation the latest request wants; zero until the first request.
static GENERATION: AtomicU64 = AtomicU64::new(0);
static WANTED: Mutex<Wanted> = Mutex::new(Wanted {
    generation: 0,
    params: DEFAULT_PARAMS,
});

struct Read {
    stack: u64,
    /// The request generation this read answered; zero for a read on a screen change.
    generation: u64,
    screen: UiScreen,
}

static READS: Mutex<Vec<Read>> = Mutex::new(Vec::new());
static READ_DONE: Condvar = Condvar::new();

pub fn install(resolver: &Resolver<'_>, hooked: Result<(), String>) {
    let mut note = None;
    let result = hooked
        .map_err(|reason| format!("needs the SceneStack::update hook: {reason}"))
        .and_then(|()| {
            let scene = resolver.table(SCENE_CLASS, 0, SCENE_SLOTS)?;
            let visual_tree = resolver.table(VISUAL_TREE_CLASS, 0, VISUAL_TREE_SLOTS)?;
            let mut found = vec![
                (SCENE_CLASS, scene.found),
                (VISUAL_TREE_CLASS, visual_tree.found),
            ];
            let mut components = HashMap::new();
            for (decorated, tables) in resolver.classes {
                let Some(short) = decorated
                    .strip_prefix(".?AV")
                    .and_then(|rest| rest.strip_suffix("@@"))
                    .filter(|short| short.ends_with("Component") && !short.contains('@'))
                else {
                    continue;
                };
                let short: Arc<str> = short.into();
                for table in tables.iter().filter(|table| table.offset == 0) {
                    components.insert(
                        resolver.image.base as usize + table.rva as usize,
                        Arc::clone(&short),
                    );
                }
            }
            // Without RTTI for components, name only the ones read or acted on,
            // through their anchors.
            let mut missing = Vec::new();
            for (class, slots) in ANCHORED_COMPONENTS {
                if components.values().any(|name| &**name == class) {
                    continue;
                }
                match resolver.table(class, 0, slots) {
                    Ok(table) => {
                        found.push((class, table.found));
                        components.insert(table.address, class.into());
                    }
                    Err(reason) => missing.push(reason),
                }
            }
            if components.len() <= ANCHORED_COMPONENTS.len() {
                missing.insert(
                    0,
                    "other component names need RTTI and read as unknown".into(),
                );
            }
            if !missing.is_empty() {
                note = Some(missing.join("; "));
            }
            capability::found(NAME, &found);
            CLASSES
                .set(Classes {
                    scene: scene.address,
                    visual_tree: visual_tree.address,
                    components,
                })
                .map_err(|_| "installed twice".to_owned())
        });
    match result {
        Ok(()) => capability::set(
            NAME,
            CapabilityState::Pending,
            match note {
                Some(note) => format!("waiting for a screen with a control tree; {note}"),
                None => "waiting for a screen with a control tree".to_owned(),
            },
        ),
        Err(reason) => capability::set(NAME, CapabilityState::Unavailable, reason),
    }
}

/// Called by the `screens` hook on the game thread after `SceneStack::update` for `stack`,
/// whose top scene is `top`. `name` gives the top screen's name.
pub fn frame(stack: usize, top: usize, top_changed: bool, name: impl FnOnce() -> String) {
    let wanted = GENERATION.load(Ordering::Acquire);
    if wanted == 0 && !top_changed {
        return;
    }
    let Some(classes) = CLASSES.get() else {
        return;
    };
    let stack = stack as u64;
    let pending = wanted != 0
        && READS.lock().is_ok_and(|reads| {
            !reads
                .iter()
                .any(|read| read.stack == stack && read.generation >= wanted)
        });
    let request = pending
        .then(|| {
            WANTED
                .lock()
                .ok()
                .map(|want| (want.generation, want.params))
        })
        .flatten()
        .filter(|(_, params)| params.stack.is_none_or(|only| only == stack));
    // A read on a screen change answers no request: generation zero.
    let (generation, params) = match request {
        Some(request) => request,
        None if top_changed => (0, DEFAULT_PARAMS),
        None => return,
    };
    let start = Instant::now();
    let mut screen = read_scene(classes, top, &params);
    screen.stack = stack;
    screen.scene = name();
    screen.read_us = start.elapsed().as_micros() as u64;
    report(&screen);
    let Ok(mut reads) = READS.lock() else {
        return;
    };
    let read = Read {
        stack,
        generation,
        screen,
    };
    match reads.iter_mut().find(|read| read.stack == stack) {
        Some(slot) => *slot = read,
        None => reads.push(read),
    }
    drop(reads);
    READ_DONE.notify_all();
}

/// Set the capability from a finished read.
fn report(screen: &UiScreen) {
    let Some(root) = screen.nodes.first() else {
        if let Some(detail) = &screen.detail
            && detail.starts_with("layout")
        {
            capability::set(NAME, CapabilityState::Failed, detail.clone());
        }
        return;
    };
    let [_, _, width, height] = root.rect;
    if width > 0.0 && height > 0.0 {
        capability::set(
            NAME,
            CapabilityState::Active,
            "read a control tree with a plausible root",
        );
    }
}

fn empty(detail: impl Into<String>) -> UiScreen {
    UiScreen {
        stack: 0,
        scene: String::new(),
        nodes: Vec::new(),
        truncated: false,
        detail: Some(detail.into()),
        read_us: 0,
        fresh: false,
    }
}

/// The control tree of scene `top`, or an empty screen with the reason.
fn read_scene(classes: &Classes, top: usize, params: &Params) -> UiScreen {
    let mut memory = Memory::default();
    if top == 0 {
        return empty("the stack is empty");
    }
    if !memory.object(top, SCENE_SCREEN_VIEW + 8) {
        return empty("layout: the top scene is not readable");
    }
    if memory.read::<usize>(top) != Some(classes.scene) {
        return empty("the top screen has no control tree (it is not a UIScene)");
    }
    let Some(view) = memory.read::<usize>(top + SCENE_SCREEN_VIEW) else {
        return empty("layout: UIScene::mScreenView is not readable");
    };
    if view == 0 {
        return empty("the screen has no view yet");
    }
    if !memory.object(view, SCREEN_VIEW_VISUAL_TREE + 8) {
        return empty("layout: UIScene::mScreenView does not point at readable memory");
    }
    let tree = memory
        .read::<usize>(view + SCREEN_VIEW_VISUAL_TREE)
        .unwrap_or(0);
    if tree == 0 {
        return empty("the screen has no control tree yet");
    }
    if !memory.object(tree, VISUAL_TREE_ROOT + 8)
        || memory.read::<usize>(tree) != Some(classes.visual_tree)
    {
        return empty("layout: ScreenView::mVisualTree is not a VisualTree");
    }
    let root = memory.read::<usize>(tree + VISUAL_TREE_ROOT).unwrap_or(0);
    if root == 0 {
        return empty("the control tree has no root yet");
    }
    let mut screen = empty("");
    screen.detail = None;
    walk(classes, &mut memory, root, params, &mut screen);
    if screen.nodes.is_empty() {
        screen.detail = Some("layout: the root control failed its checks".into());
    }
    screen
}

/// One control's fields, checked.
struct Control {
    cached: [f32; 2],
    relative: [f32; 2],
    size: [f32; 2],
    alpha: f32,
    flags: u16,
    name: String,
    children: (usize, usize),
    components: (usize, usize),
}

fn control(memory: &mut Memory, address: usize) -> Option<Control> {
    if !memory.object(address, CONTROL_READ)
        || memory.read::<usize>(address + CONTROL_SELF) != Some(address)
    {
        return None;
    }
    let cached = memory.read::<[f32; 2]>(address + CONTROL_CACHED_POSITION)?;
    let relative = memory.read::<[f32; 2]>(address + CONTROL_RELATIVE_POSITION)?;
    let size = memory.read::<[f32; 2]>(address + CONTROL_SIZE)?;
    let alpha = memory.read::<f32>(address + CONTROL_ALPHA)?;
    let plausible = |value: f32| value.is_finite() && value.abs() < MAX_COORDINATE;
    if !cached
        .into_iter()
        .chain(relative)
        .chain(size)
        .all(plausible)
        || size.iter().any(|side| *side < 0.0)
        || !alpha.is_finite()
    {
        return None;
    }
    Some(Control {
        cached,
        relative,
        size,
        alpha,
        flags: memory.read::<u16>(address + CONTROL_FLAGS)?,
        name: memory.string(address + CONTROL_NAME, MAX_NAME)?,
        children: memory.vector(address + CONTROL_CHILDREN, 16, MAX_CHILDREN)?,
        components: memory.vector(address + CONTROL_COMPONENTS, 8, MAX_COMPONENTS)?,
    })
}

/// Read the tree under `root` into `screen.nodes`, depth first in pre-order.
fn walk(
    classes: &Classes,
    memory: &mut Memory,
    root: usize,
    params: &Params,
    screen: &mut UiScreen,
) {
    let max_depth = params.max_depth.min(MAX_DEPTH);
    // (control, parent index, depth, parent's absolute position)
    let mut pending: Vec<(usize, Option<u32>, u32, [f32; 2])> = vec![(root, None, 0, [0.0; 2])];
    while let Some((address, parent, depth, origin)) = pending.pop() {
        if screen.nodes.len() >= MAX_NODES {
            screen.truncated = true;
            break;
        }
        let Some(control) = control(memory, address) else {
            screen.truncated = true;
            continue;
        };
        let visible = control.flags & FLAG_VISIBLE != 0;
        if params.visible_only && !visible {
            continue;
        }
        let position = if control.flags & FLAG_POSITION_DIRTY == 0 {
            control.cached
        } else {
            [
                origin[0] + control.relative[0],
                origin[1] + control.relative[1],
            ]
        };
        let mut node = UiNode {
            parent,
            name: control.name,
            rect: [position[0], position[1], control.size[0], control.size[1]],
            alpha: control.alpha,
            visible,
            enabled: control.flags & FLAG_ENABLED != 0,
            hover: control.flags & FLAG_HOVER != 0,
            clips: control.flags & FLAG_CLIPS_CHILDREN != 0,
            components: Vec::new(),
            text: None,
            checked: None,
            child_count: control.children.1 as u32,
        };
        components(classes, memory, address, control.components, &mut node);
        let index = screen.nodes.len() as u32;
        screen.nodes.push(node);
        let (first, count) = control.children;
        if count == 0 {
            continue;
        }
        if depth >= max_depth {
            screen.truncated = true;
            continue;
        }
        // Reverse, so the first child pops first.
        for child in (0..count).rev() {
            match memory.read::<usize>(first + child * 16) {
                Some(child) if child != 0 => {
                    pending.push((child, Some(index), depth + 1, position));
                }
                _ => screen.truncated = true,
            }
        }
    }
}

fn components(
    classes: &Classes,
    memory: &mut Memory,
    owner: usize,
    (first, count): (usize, usize),
    node: &mut UiNode,
) {
    for index in 0..count {
        let Some(component) = memory.read::<usize>(first + index * 8) else {
            continue;
        };
        if !memory.object(component, COMPONENT_OWNER + 8)
            || memory.read::<usize>(component + COMPONENT_OWNER) != Some(owner)
        {
            continue;
        }
        let Some(name) = memory
            .read::<usize>(component)
            .and_then(|table| classes.components.get(&table).cloned())
        else {
            node.components.push("unknown".into());
            continue;
        };
        node.components.push(name.to_string());
        match &*name {
            "TextComponent" => {
                // The code takes the localized text once drawn, else the label (a key or
                // literal text). In JSON UI a `#name` is a binding (see the `ui` folder of
                // Mojang's bedrock-samples), so a label that starts with `#` is no text yet.
                node.text = [TEXT_LOCALIZED, TEXT_LABEL]
                    .into_iter()
                    .filter_map(|offset| memory.string(component + offset, MAX_TEXT))
                    .find(|text| !text.is_empty())
                    .filter(|text| !text.starts_with('#'));
            }
            "ToggleComponent" => {
                node.checked = memory
                    .read::<u8>(component + TOGGLE_FLAGS)
                    .map(|bits| bits & 1 != 0);
            }
            _ => {}
        }
    }
}

/// Ask the game thread for the trees of `stacks` and wait for them; screens it did not
/// answer in time come from the last read, marked not fresh.
pub fn read(
    stacks: &[u64],
    stack: Option<u64>,
    max_depth: Option<u32>,
    visible_only: bool,
) -> Vec<UiScreen> {
    let targets: Vec<u64> = stacks
        .iter()
        .copied()
        .filter(|id| stack.is_none_or(|only| only == *id))
        .collect();
    let generation = {
        let Ok(mut want) = WANTED.lock() else {
            return Vec::new();
        };
        want.generation += 1;
        want.params = Params {
            stack,
            max_depth: max_depth.unwrap_or(DEFAULT_DEPTH).min(MAX_DEPTH),
            visible_only,
        };
        GENERATION.store(want.generation, Ordering::Release);
        want.generation
    };
    let deadline = Instant::now() + WAIT;
    let Ok(mut reads) = READS.lock() else {
        return Vec::new();
    };
    loop {
        let answered = targets.iter().all(|id| {
            reads
                .iter()
                .any(|read| read.stack == *id && read.generation >= generation)
        });
        let now = Instant::now();
        if answered || now >= deadline {
            break;
        }
        reads = match READ_DONE.wait_timeout(reads, deadline - now) {
            Ok((reads, _)) => reads,
            Err(_) => return Vec::new(),
        };
    }
    targets
        .iter()
        .filter_map(|id| reads.iter().find(|read| read.stack == *id))
        .map(|read| UiScreen {
            fresh: read.generation >= generation,
            ..read.screen.clone()
        })
        .collect()
}
