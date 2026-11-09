//! UI control trees from the unstable layer's `ui_tree` capability: scaling to logical window
//! pixels, the views `ui_tree` returns, and finding the button `ui_click` wants.

use protocol::unstable::{UiNode, UiScreen};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::screen_text::{Match, Region};

/// Logical window pixels per UI unit. The root control covers the whole window, so the
/// window's logical size over the root's size in UI units gives the GUI scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scale {
    pub x: f64,
    pub y: f64,
}

impl Scale {
    pub fn of(screen: &UiScreen, width: f64, height: f64) -> Option<Self> {
        let [_, _, root_width, root_height] = screen.nodes.first()?.rect;
        (root_width > 0.0 && root_height > 0.0).then(|| Self {
            x: width / f64::from(root_width),
            y: height / f64::from(root_height),
        })
    }

    /// A UI rectangle in logical window pixels, to a tenth of a pixel.
    pub fn rect(self, [x, y, width, height]: [f32; 4]) -> [f64; 4] {
        let round = |value: f64| (value * 10.0).round() / 10.0;
        [
            round(f64::from(x) * self.x),
            round(f64::from(y) * self.y),
            round(f64::from(width) * self.x),
            round(f64::from(height) * self.y),
        ]
    }

    pub fn point(self, [x, y]: [f32; 2]) -> (f64, f64) {
        (f64::from(x) * self.x, f64::from(y) * self.y)
    }
}

/// Text without the game's `§` formatting codes, with runs of white space as one space.
pub fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character == '§' {
            characters.next();
        } else {
            out.push(character);
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn has(node: &UiNode, component: &str) -> bool {
    node.components.iter().any(|name| name == component)
}

fn clickable(node: &UiNode) -> bool {
    has(node, "ButtonComponent") || has(node, "ToggleComponent")
}

/// What a control is for, by its components; None for layout-only controls.
fn kind(node: &UiNode) -> Option<&'static str> {
    [
        ("ToggleComponent", "toggle"),
        ("ButtonComponent", "button"),
        ("TextEditComponent", "text_box"),
        ("SliderComponent", "slider"),
        ("DropdownComponent", "dropdown"),
        ("ScrollViewComponent", "scroll_view"),
    ]
    .into_iter()
    .find(|(component, _)| has(node, component))
    .map(|(_, kind)| kind)
    .or_else(|| node.text.as_ref().map(|_| "text"))
}

fn intersect(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let x = a[0].max(b[0]);
    let y = a[1].max(b[1]);
    let right = (a[0] + a[2]).min(b[0] + b[2]);
    let bottom = (a[1] + a[3]).min(b[1] + b[3]);
    [x, y, (right - x).max(0.0), (bottom - y).max(0.0)]
}

fn centre([x, y, width, height]: [f32; 4]) -> [f32; 2] {
    [x + width / 2.0, y + height / 2.0]
}

fn inside([x, y]: [f32; 2], rect: [f32; 4]) -> bool {
    x >= rect[0] && y >= rect[1] && x <= rect[0] + rect[2] && y <= rect[1] + rect[3]
}

/// The share of a control's height that may lie outside its scroll view's visible part
/// when `ui_click` clicks it: a list that stops scrolling with a button half cut off at the
/// edge would otherwise take the click on its frame or the next row.
const CLICK_MARGIN: f32 = 0.25;

/// Whether a control at `rect` shows enough of itself inside `clip` to be clicked: its
/// centre lies inside and at most [`CLICK_MARGIN`] of its height (or of the view's height,
/// for a control taller than the view) is cut off.
fn shows(rect: [f32; 4], clip: [f32; 4]) -> bool {
    clip[2] > 0.0
        && clip[3] > 0.0
        && inside(centre(rect), clip)
        && intersect(rect, clip)[3] >= (1.0 - CLICK_MARGIN) * rect[3].min(clip[3])
}

/// Derived facts about one screen's nodes, indexed like [`UiScreen::nodes`].
struct Layout {
    /// The part of the screen each node may draw in: its ancestors' clipping rectangles
    /// (scroll view ports clip their content) intersected with the root.
    clip: Vec<[f32; 4]>,
    /// One past the last node of each node's subtree.
    end: Vec<usize>,
    /// The texts of each button or toggle, in order: inside it, or drawn over it (a
    /// settings row draws its label beside the toggle, inside the toggle's rectangle).
    labels: Vec<Vec<String>>,
    /// Text nodes that label a button or toggle.
    owned: Vec<bool>,
}

impl Layout {
    fn of(nodes: &[UiNode]) -> Self {
        let mut depth = Vec::with_capacity(nodes.len());
        let mut clip = Vec::with_capacity(nodes.len());
        for node in nodes {
            match node.parent.map(|parent| parent as usize) {
                Some(parent) if parent < depth.len() => {
                    depth.push(depth[parent] + 1);
                    let inherited: [f32; 4] = clip[parent];
                    clip.push(if nodes[parent].clips {
                        intersect(inherited, nodes[parent].rect)
                    } else {
                        inherited
                    });
                }
                _ => {
                    depth.push(0);
                    clip.push(node.rect);
                }
            }
        }
        let mut end = vec![nodes.len(); nodes.len()];
        let mut open: Vec<usize> = Vec::new();
        for (index, level) in depth.iter().enumerate() {
            while let Some(&last) = open.last()
                && depth[last] >= *level
            {
                end[last] = index;
                open.pop();
            }
            open.push(index);
        }
        let clickables: Vec<usize> = (0..nodes.len())
            .filter(|index| clickable(&nodes[*index]) && area(nodes[*index].rect) > 0.0)
            .collect();
        let mut labels = vec![Vec::new(); nodes.len()];
        let mut owned = vec![false; nodes.len()];
        for (index, node) in nodes.iter().enumerate() {
            let Some(text) = node
                .text
                .as_deref()
                .map(plain)
                .filter(|text| !text.is_empty())
            else {
                continue;
            };
            let ancestor = std::iter::successors(Some(index), |at| {
                nodes[*at].parent.map(|parent| parent as usize)
            })
            .find(|at| clickable(&nodes[*at]));
            // A label beside its toggle shows inside the toggle's rectangle and scrolls with
            // it, so both have the same clip; text from another list or a header that
            // happens to lie over a control does not.
            let point = centre(node.rect);
            let owner = ancestor.or_else(|| {
                clickables
                    .iter()
                    .copied()
                    .filter(|at| inside(point, nodes[*at].rect) && clip[*at] == clip[index])
                    .min_by(|a, b| area(nodes[*a].rect).total_cmp(&area(nodes[*b].rect)))
            });
            if let Some(owner) = owner {
                labels[owner].push(text);
                owned[index] = owner != index;
            }
        }
        Self {
            clip,
            end,
            labels,
            owned,
        }
    }
}

fn area(rect: [f32; 4]) -> f32 {
    rect[2] * rect[3]
}

/// How `ui_tree` shows a screen.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum View {
    /// Buttons, toggles, text boxes, sliders, dropdowns, scroll views and free text.
    #[default]
    Controls,
    /// Every control read, nested.
    Tree,
}

/// One screen as `ui_tree` returns it.
pub fn screen_json(screen: &UiScreen, scale: Option<Scale>, view: View) -> Value {
    let rect = |rect: [f32; 4]| match scale {
        Some(scale) => json!(scale.rect(rect)),
        None => json!(rect),
    };
    let layout = Layout::of(&screen.nodes);
    let nodes = &screen.nodes;
    let body = match view {
        View::Controls => json!(
            (0..nodes.len())
                .filter_map(|index| {
                    let node = &nodes[index];
                    let kind = kind(node)?;
                    if kind == "text" && layout.owned[index] {
                        return None;
                    }
                    if node.rect[2] <= 0.0 || node.rect[3] <= 0.0 {
                        return None;
                    }
                    // A button shows its labels; other controls only their own text.
                    let label = if clickable(node) {
                        layout.labels[index].join(" ")
                    } else {
                        node.text.as_deref().map(plain).unwrap_or_default()
                    };
                    let mut entry = json!({
                        "kind": kind,
                        "name": node.name,
                        "rect": rect(node.rect),
                        "shown": shows(node.rect, layout.clip[index]),
                    });
                    if !label.is_empty() {
                        entry["text"] = json!(label);
                    }
                    if !node.enabled {
                        entry["enabled"] = json!(false);
                    }
                    if node.hover {
                        entry["hover"] = json!(true);
                    }
                    if let Some(checked) = node.checked {
                        entry["checked"] = json!(checked);
                    }
                    Some(entry)
                })
                .collect::<Vec<_>>()
        ),
        View::Tree => tree_json(nodes, &layout, 0, &rect).unwrap_or(Value::Null),
    };
    let mut out = json!({
        "stack": screen.stack,
        "screen": screen.scene,
        "nodes": nodes.len(),
        "truncated": screen.truncated,
        "read_us": screen.read_us,
        "fresh": screen.fresh,
    });
    out[match view {
        View::Controls => "controls",
        View::Tree => "root",
    }] = body;
    if let Some(detail) = &screen.detail {
        out["detail"] = json!(detail);
    }
    if let Some(scale) = scale {
        out["ui_scale"] = json!([scale.x, scale.y]);
    }
    out
}

fn tree_json(
    nodes: &[UiNode],
    layout: &Layout,
    index: usize,
    rect: &dyn Fn([f32; 4]) -> Value,
) -> Option<Value> {
    let node = nodes.get(index)?;
    let mut out = json!({ "name": node.name, "rect": rect(node.rect) });
    if !node.components.is_empty() {
        out["components"] = json!(node.components);
    }
    if let Some(text) = &node.text {
        out["text"] = json!(plain(text));
    }
    if let Some(checked) = node.checked {
        out["checked"] = json!(checked);
    }
    if !node.visible {
        out["visible"] = json!(false);
    }
    if node.alpha < 1.0 {
        out["alpha"] = json!(node.alpha);
    }
    if node.clips {
        out["clips"] = json!(true);
    }
    let children: Vec<Value> = (index + 1..layout.end[index])
        .filter(|child| nodes[*child].parent == Some(index as u32))
        .filter_map(|child| tree_json(nodes, layout, child, rect))
        .collect();
    if children.len() < node.child_count as usize {
        out["children_read"] = json!(children.len());
        out["child_count"] = json!(node.child_count);
    }
    if !children.is_empty() {
        out["children"] = json!(children);
    }
    Some(out)
}

/// A button or toggle whose label matches, found in one of the screens.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub screen: usize,
    pub node: usize,
    pub label: String,
    pub how: Match,
    /// UI units.
    pub rect: [f32; 4],
    /// The part of the screen the control may draw in, in UI units.
    pub clip: [f32; 4],
    pub enabled: bool,
}

impl Target {
    /// Whether the control shows inside its scroll view's visible part, all of it or all
    /// but a margin, so a click on its centre lands on it.
    pub fn shown(&self) -> bool {
        shows(self.rect, self.clip)
    }

    /// Where to wheel to bring it into view, in UI units, and whether it lies below.
    pub fn scroll_at(&self) -> ([f32; 2], bool) {
        (
            centre(self.clip),
            centre(self.rect)[1] > centre(self.clip)[1],
        )
    }
}

/// Visible buttons and toggles labelled `text`, best first: an exact label before one that
/// contains it (unless `exact`), then enabled ones, then the innermost (smallest).
/// `keep` filters by the target's rectangle in UI units.
pub fn candidates(
    screens: &[UiScreen],
    text: &str,
    exact: bool,
    keep: impl Fn(usize, [f32; 4]) -> bool,
) -> Vec<Target> {
    let wanted = plain(text).to_lowercase();
    if wanted.is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    for (screen_index, screen) in screens.iter().enumerate() {
        let nodes = &screen.nodes;
        let layout = Layout::of(nodes);
        for (index, node) in nodes.iter().enumerate() {
            if !clickable(node) || !node.visible || node.rect[2] <= 0.0 || node.rect[3] <= 0.0 {
                continue;
            }
            let mut best: Option<(String, Match)> = None;
            for label in &layout.labels[index] {
                let lower = label.to_lowercase();
                if lower == wanted {
                    best = Some((label.clone(), Match::Exact));
                    break;
                }
                if !exact && best.is_none() && lower.contains(&wanted) {
                    best = Some((label.clone(), Match::Contains));
                }
            }
            let Some((label, how)) = best else {
                continue;
            };
            if !keep(screen_index, node.rect) {
                continue;
            }
            found.push(Target {
                screen: screen_index,
                node: index,
                label,
                how,
                rect: node.rect,
                clip: layout.clip[index],
                enabled: node.enabled,
            });
        }
    }
    found.sort_by(|a, b| {
        (a.how != Match::Exact)
            .cmp(&(b.how != Match::Exact))
            .then((!a.enabled).cmp(&!b.enabled))
            .then((a.rect[2] * a.rect[3]).total_cmp(&(b.rect[2] * b.rect[3])))
    });
    found
}

/// Whether a UI rectangle's centre, scaled, lies in `region` (logical window pixels).
pub fn in_region(region: &Region, scale: Scale, rect: [f32; 4]) -> bool {
    let (x, y) = scale.point(centre(rect));
    x >= region.x && y >= region.y && x <= region.x + region.width && y <= region.y + region.height
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(parent: Option<u32>, name: &str, rect: [f32; 4]) -> UiNode {
        UiNode {
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
        }
    }

    fn button(parent: u32, name: &str, rect: [f32; 4]) -> UiNode {
        UiNode {
            components: vec!["InputComponent".into(), "ButtonComponent".into()],
            child_count: 1,
            ..node(Some(parent), name, rect)
        }
    }

    fn label(parent: u32, text: &str, rect: [f32; 4]) -> UiNode {
        UiNode {
            components: vec!["TextComponent".into()],
            text: Some(text.into()),
            ..node(Some(parent), "label", rect)
        }
    }

    /// A 500×250 screen with a form: a scroll view port that shows y 50..150 and three
    /// buttons stacked 40 units apart, the last one below the port.
    fn form() -> UiScreen {
        let mut port = node(Some(1), "scroll_view_port", [100.0, 50.0, 300.0, 100.0]);
        port.clips = true;
        let mut view = node(Some(0), "scroll_view", [100.0, 50.0, 300.0, 100.0]);
        view.components = vec!["ScrollViewComponent".into()];
        UiScreen {
            stack: 7,
            scene: "server_form".into(),
            nodes: vec![
                node(None, "server_form", [0.0, 0.0, 500.0, 250.0]),
                view,
                port,
                button(2, "villager", [100.0, 60.0, 300.0, 30.0]),
                label(3, "§lVillager", [110.0, 70.0, 50.0, 10.0]),
                button(2, "zombie_villager", [100.0, 100.0, 300.0, 30.0]),
                label(5, "Zombie  Villager", [110.0, 110.0, 50.0, 10.0]),
                button(2, "creeper", [100.0, 180.0, 300.0, 30.0]),
                label(7, "Creeper", [110.0, 190.0, 50.0, 10.0]),
                label(0, "Forms", [200.0, 10.0, 100.0, 10.0]),
            ],
            truncated: false,
            detail: None,
            read_us: 120,
            fresh: true,
        }
    }

    #[test]
    fn plain_text_drops_formatting_codes_and_extra_space() {
        assert_eq!(plain("§l§6Save §r& Quit "), "Save & Quit");
        assert_eq!(plain("a\n  b"), "a b");
    }

    #[test]
    fn scale_comes_from_the_root_and_the_window() {
        let scale = Scale::of(&form(), 1000.0, 500.0).unwrap();
        assert_eq!(scale, Scale { x: 2.0, y: 2.0 });
        assert_eq!(
            scale.rect([100.0, 60.0, 300.0, 30.0]),
            [200.0, 120.0, 600.0, 60.0]
        );
    }

    #[test]
    fn an_exact_label_wins_over_one_that_contains_it() {
        let screens = [form()];
        let found = candidates(&screens, "villager", false, |_, _| true);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].node, 3);
        assert_eq!(found[0].how, Match::Exact);
        assert_eq!(found[1].how, Match::Contains);
        assert!(candidates(&screens, "illag", true, |_, _| true).is_empty());
        assert!(
            candidates(&screens, "Forms", false, |_, _| true).is_empty(),
            "free text is not a button"
        );
    }

    #[test]
    fn a_button_below_its_scroll_view_is_not_shown_and_scrolls_down() {
        let screens = [form()];
        let creeper = &candidates(&screens, "Creeper", false, |_, _| true)[0];
        assert_eq!(creeper.clip, [100.0, 50.0, 300.0, 100.0]);
        assert!(!creeper.shown());
        assert_eq!(creeper.scroll_at(), ([250.0, 100.0], true));
        let villager = &candidates(&screens, "Villager", true, |_, _| true)[0];
        assert!(villager.shown());
    }

    #[test]
    fn a_button_cut_off_by_more_than_the_margin_is_not_shown_and_scrolls_on() {
        let port = [100.0, 50.0, 300.0, 100.0];
        let at = |y: f32| Target {
            screen: 0,
            node: 0,
            label: "Creeper".into(),
            how: Match::Exact,
            rect: [100.0, y, 300.0, 30.0],
            clip: port,
            enabled: true,
        };
        // Centre inside, a third of the button below the port: scroll further.
        assert!(!at(130.0).shown());
        assert!(at(130.0).scroll_at().1);
        // A sixth cut off is within the margin; at the top edge too.
        assert!(at(125.0).shown());
        assert!(at(45.0).shown());
        assert!(!at(40.0).shown());
        // A control taller than the port counts against the port's height.
        let tall = Target {
            rect: [100.0, 40.0, 300.0, 120.0],
            ..at(0.0)
        };
        assert!(tall.shown());
    }

    #[test]
    fn controls_view_lists_buttons_with_their_labels_in_window_pixels() {
        let screen = form();
        let scale = Scale::of(&screen, 1000.0, 500.0);
        let view = screen_json(&screen, scale, View::Controls);
        let controls = view["controls"].as_array().unwrap();
        assert_eq!(controls[0]["kind"], "scroll_view");
        assert_eq!(controls[1]["kind"], "button");
        assert_eq!(controls[1]["text"], "Villager");
        assert_eq!(controls[1]["name"], "villager");
        assert_eq!(controls[1]["rect"], json!([200.0, 120.0, 600.0, 60.0]));
        assert_eq!(controls[1]["shown"], true);
        assert_eq!(controls[3]["shown"], false);
        assert_eq!(controls[4]["kind"], "text");
        assert_eq!(controls[4]["text"], "Forms");
        assert_eq!(controls.len(), 5);
        assert_eq!(view["ui_scale"], json!([2.0, 2.0]));
    }

    #[test]
    fn tree_view_nests_children() {
        let view = screen_json(&form(), None, View::Tree);
        let root = &view["root"];
        assert_eq!(root["name"], "server_form");
        assert_eq!(root["children"][1]["text"], "Forms");
        assert_eq!(
            root["children"][0]["children"][0]["children"][2]["children"][0]["text"],
            "Creeper"
        );
        assert_eq!(root["children"][0]["children"][0]["clips"], true);
    }

    #[test]
    fn a_label_drawn_over_a_toggle_names_it() {
        let mut toggle = button(1, "option_toggle", [10.0, 10.0, 200.0, 20.0]);
        toggle.components = vec!["ToggleComponent".into()];
        toggle.checked = Some(true);
        let screen = UiScreen {
            nodes: vec![
                node(None, "settings", [0.0, 0.0, 500.0, 250.0]),
                node(Some(0), "row", [10.0, 10.0, 200.0, 20.0]),
                toggle,
                label(1, "Mute All Chat", [40.0, 12.0, 100.0, 10.0]),
                label(0, "Settings", [200.0, 0.0, 100.0, 10.0]),
            ],
            ..form()
        };
        let found = candidates(
            std::slice::from_ref(&screen),
            "mute all chat",
            true,
            |_, _| true,
        );
        assert_eq!(found[0].node, 2);
        let view = screen_json(&screen, None, View::Controls);
        let controls = view["controls"].as_array().unwrap();
        assert_eq!(controls.len(), 2);
        assert_eq!(controls[0]["kind"], "toggle");
        assert_eq!(controls[0]["text"], "Mute All Chat");
        assert_eq!(controls[0]["checked"], true);
        assert_eq!(controls[1]["text"], "Settings");
    }

    #[test]
    fn a_header_over_a_scrolled_button_does_not_name_it() {
        let mut screen = form();
        // Scrolled up past the port's top edge, under the "Forms" header.
        screen
            .nodes
            .push(button(2, "hidden_row", [100.0, 0.0, 300.0, 30.0]));
        assert!(candidates(&[screen], "Forms", false, |_, _| true).is_empty());
    }

    #[test]
    fn region_filters_by_the_scaled_centre() {
        let scale = Scale { x: 2.0, y: 2.0 };
        let region = Region {
            x: 0.0,
            y: 0.0,
            width: 1000.0,
            height: 200.0,
        };
        assert!(in_region(&region, scale, [100.0, 60.0, 300.0, 30.0]));
        assert!(!in_region(&region, scale, [100.0, 180.0, 300.0, 30.0]));
    }
}
