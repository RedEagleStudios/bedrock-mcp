//! Ore UI pages from the unstable layer's `oreui` capability, as UI screens: `ui_tree` shows
//! them and `ui_click` clicks through them like JSON UI control trees.
//!
//! A page becomes a [`UiScreen`] whose root is the view's viewport in CSS pixels, so
//! [`crate::ui_tree::Scale`] maps it to logical window pixels by the window's logical size
//! over `innerWidth`/`innerHeight`. Each element gets the JSON UI component that matches its
//! role, so the same rules find buttons, their labels and their scroll views.

use protocol::unstable::{OreUiElement, OreUiView, UiNode, UiScreen};

/// Whether a scene stack's screen name names an Ore UI page (the game names them by URL).
pub fn is_page(scene: &str) -> bool {
    scene.contains("://") && scene.ends_with(".html")
}

/// The file name of a page URL, without its route or query.
fn file(url: &str) -> &str {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.rsplit('/').next().unwrap_or(path)
}

/// The view that draws scene `scene`: the same URL, else the same page file. A page that
/// several views load (split screen) matches the first.
pub fn view_for<'a>(scene: &str, views: &'a [OreUiView]) -> Option<&'a OreUiView> {
    views
        .iter()
        .find(|view| view.url == scene)
        .or_else(|| views.iter().find(|view| file(&view.url) == file(scene)))
}

fn has_role(element: &OreUiElement, roles: &[&str]) -> bool {
    element
        .role
        .as_deref()
        .is_some_and(|role| roles.contains(&role))
}

fn input_type(element: &OreUiElement) -> Option<&str> {
    (element.tag == "input").then(|| element.input_type.as_deref().unwrap_or("text"))
}

/// The JSON UI component that does what the element does, if any.
fn component(element: &OreUiElement) -> Option<&'static str> {
    let input = input_type(element);
    if element.checked.is_some()
        || has_role(
            element,
            &["checkbox", "switch", "radio", "menuitemcheckbox"],
        )
        || matches!(input, Some("checkbox" | "radio"))
    {
        return Some("ToggleComponent");
    }
    if matches!(input, Some("range")) || has_role(element, &["slider"]) {
        return Some("SliderComponent");
    }
    if element.tag == "select" || has_role(element, &["combobox", "listbox"]) {
        return Some("DropdownComponent");
    }
    if element.tag == "textarea"
        || has_role(element, &["textbox", "searchbox"])
        || input.is_some_and(|kind| !matches!(kind, "button" | "submit" | "reset" | "image"))
    {
        return Some("TextEditComponent");
    }
    if element.focus_id.is_some()
        || matches!(element.tag.as_str(), "button" | "a" | "input")
        || has_role(element, &["button", "link", "tab", "menuitem", "option"])
    {
        return Some("ButtonComponent");
    }
    None
}

/// The page of `view` as the screen on top of stack `stack`, named `scene`.
pub fn screen(view: &OreUiView, stack: u64, scene: &str) -> UiScreen {
    let [width, height] = view.viewport;
    let mut nodes = Vec::with_capacity(view.elements.len() + 1);
    if width > 0.0 && height > 0.0 {
        nodes.push(UiNode {
            parent: None,
            name: "viewport".into(),
            rect: [0.0, 0.0, width, height],
            alpha: 1.0,
            visible: true,
            enabled: true,
            hover: false,
            clips: true,
            components: Vec::new(),
            text: None,
            checked: None,
            child_count: 0,
        });
        let count = view.elements.len() as u32;
        for element in &view.elements {
            let mut components: Vec<String> =
                component(element).into_iter().map(Into::into).collect();
            if element.scrolls {
                components.push("ScrollViewComponent".into());
            }
            // Index 0 is the viewport, so element i is node i + 1.
            let parent = element
                .parent
                .filter(|parent| *parent < count)
                .map_or(0, |parent| parent + 1);
            nodes.push(UiNode {
                parent: Some(parent),
                // Test ids describe the element; focus ids are numbers the page hands out.
                name: element
                    .test_id
                    .clone()
                    .or_else(|| element.focus_id.clone())
                    .unwrap_or_else(|| element.tag.clone()),
                rect: element.rect,
                alpha: 1.0,
                visible: true,
                enabled: !element.disabled,
                hover: element.focused,
                clips: element.scrolls,
                components,
                text: element.text.clone(),
                checked: element.checked,
                child_count: 0,
            });
        }
        for index in 1..nodes.len() {
            if let Some(parent) = nodes[index].parent {
                nodes[parent as usize].child_count += 1;
            }
        }
    }
    let detail = view.detail.clone().or_else(|| {
        nodes
            .is_empty()
            .then(|| "the page reported no viewport".to_owned())
    });
    UiScreen {
        stack,
        scene: scene.into(),
        nodes,
        truncated: view.truncated,
        detail,
        read_us: view.read_us,
        fresh: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_tree::{Scale, View, candidates, screen_json};

    fn element(parent: Option<u32>, tag: &str, rect: [f32; 4]) -> OreUiElement {
        OreUiElement {
            parent,
            tag: tag.into(),
            focus_id: None,
            test_id: None,
            role: None,
            input_type: None,
            text: None,
            rect,
            disabled: false,
            checked: None,
            scrolls: false,
            focused: false,
        }
    }

    fn row(parent: u32, id: &str, label: &str, y: f32) -> [OreUiElement; 2] {
        let mut button = element(Some(parent), "div", [100.0, y, 600.0, 60.0]);
        button.focus_id = Some(id.into());
        let mut text = element(None, "span", [120.0, y + 20.0, 200.0, 20.0]);
        text.text = Some(label.into());
        [button, text]
    }

    /// An 800×600 world list: a scrolling list showing y 100..400 with rows 100 apart, the
    /// last one below the list.
    fn worlds() -> OreUiView {
        let mut list = element(None, "div", [100.0, 100.0, 600.0, 300.0]);
        list.scrolls = true;
        list.test_id = Some("world-list".into());
        let mut elements = vec![list];
        for (index, (id, label)) in [
            ("w1", "My World"),
            ("w2", "Creative"),
            ("w3", "Skyblock"),
            ("w4", "Hardcore"),
        ]
        .into_iter()
        .enumerate()
        {
            let [button, mut text] = row(0, id, label, 110.0 + index as f32 * 100.0);
            text.parent = Some(elements.len() as u32);
            elements.push(button);
            elements.push(text);
        }
        OreUiView {
            url: "ui://demo/hbui/index.html".into(),
            href: "coui://demo/hbui/index.html#/worlds".into(),
            title: String::new(),
            state: 2,
            viewport: [800.0, 600.0],
            elements,
            truncated: false,
            detail: None,
            read_us: 900,
        }
    }

    #[test]
    fn scenes_find_their_view_by_url_or_page_file() {
        let views = [worlds()];
        assert!(is_page("ui://demo/hbui/index.html"));
        assert!(!is_page("start_screen"));
        assert!(view_for("ui://demo/hbui/index.html", &views).is_some());
        assert!(view_for("coui://other/hbui/index.html", &views).is_some());
        assert!(view_for("ui://demo/hbui/gameplay.html", &views).is_none());
    }

    #[test]
    fn pages_scale_by_the_viewport_and_label_their_buttons() {
        let screen = screen(&worlds(), 3, "ui://demo/hbui/index.html");
        assert_eq!(screen.nodes[0].rect, [0.0, 0.0, 800.0, 600.0]);
        let scale = Scale::of(&screen, 1600.0, 1200.0).unwrap();
        assert_eq!(scale, Scale { x: 2.0, y: 2.0 });
        let found = candidates(std::slice::from_ref(&screen), "creative", false, |_, _| {
            true
        });
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].label, "Creative");
        assert_eq!(screen.nodes[found[0].node].name, "w2");
        assert!(found[0].shown());
        // The fourth row lies below the list: ui_click scrolls down over the list first.
        let hidden = candidates(std::slice::from_ref(&screen), "Hardcore", true, |_, _| true);
        assert!(!hidden[0].shown());
        assert_eq!(hidden[0].scroll_at(), ([400.0, 250.0], true));
        let json = screen_json(&screen, Some(scale), View::Controls);
        let controls = json["controls"].as_array().unwrap();
        assert_eq!(controls[0]["kind"], "scroll_view");
        assert_eq!(controls[1]["kind"], "button");
        assert_eq!(controls[1]["text"], "My World");
        assert_eq!(
            controls[1]["rect"],
            serde_json::json!([200.0, 220.0, 1200.0, 120.0])
        );
        // Labels inside buttons are not listed again.
        assert_eq!(controls.len(), 5);
    }

    #[test]
    fn toggles_and_disabled_elements_keep_their_state() {
        let mut view = worlds();
        let mut toggle = element(None, "div", [10.0, 10.0, 40.0, 20.0]);
        toggle.focus_id = Some("toggle".into());
        toggle.checked = Some(true);
        toggle.disabled = true;
        view.elements.push(toggle);
        let screen = screen(&view, 1, "ui://demo/hbui/index.html");
        let node = screen.nodes.last().unwrap();
        assert_eq!(node.components, ["ToggleComponent"]);
        assert!(!node.enabled);
        assert_eq!(node.checked, Some(true));
    }

    #[test]
    fn a_page_without_a_viewport_says_why() {
        let mut view = worlds();
        view.viewport = [0.0, 0.0];
        view.elements.clear();
        let screen = screen(&view, 1, "ui://demo/hbui/index.html");
        assert!(screen.nodes.is_empty());
        assert!(screen.detail.is_some());
    }
}
