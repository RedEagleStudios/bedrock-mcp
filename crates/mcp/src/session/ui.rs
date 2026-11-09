//! The live UI tree: the `ui_tree` tool, and `ui_click` through it. Ore UI pages join the
//! JSON UI control trees through the `oreui` capability.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use super::Session;
use crate::{
    error::{Result, bail},
    oreui,
    screen_text::{Match, Region},
    ui_tree::{self, Scale, View},
};
use protocol::unstable::UiScreen;

/// The unstable capability both use.
pub(super) const CAPABILITY: &str = "ui_tree";
/// The unstable capability that reads Ore UI pages.
pub(super) const OREUI: &str = "oreui";
/// Wheel notches per scroll while `ui_click` brings a control into view, and the wait for
/// the list to settle.
const SCROLL_NOTCHES: f64 = 2.0;
const SCROLL_SETTLE: Duration = Duration::from_millis(400);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiTreeArgs {
    stack: Option<u64>,
    max_depth: Option<u32>,
    #[serde(default = "yes")]
    visible_only: bool,
    #[serde(default)]
    view: View,
}

fn yes() -> bool {
    true
}

/// What `ui_click` asks of the tree.
pub(super) struct TreeClick<'a> {
    pub text: &'a str,
    pub exact: bool,
    pub max_scrolls: u32,
    pub button: &'a str,
    pub region: Option<Region>,
    /// Only controls whose name passes, when given (`world_open` keeps to local world cards).
    pub control: Option<&'a dyn Fn(&str) -> bool>,
}

impl Session {
    pub(crate) fn ui_tree(&mut self, args: UiTreeArgs) -> Result<Value> {
        let mut notes = Vec::new();
        self.connect_running_agent();
        if !self.unstable_requestable(CAPABILITY, &mut notes) {
            bail!(
                "ui_tree needs the unstable layer's ui_tree capability: {}",
                notes.join("; ")
            );
        }
        let pid = self.running_pid()?;
        let window = self.game_window(pid)?;
        let mut screens = self
            .unstable
            .ui_tree(args.stack, args.max_depth, args.visible_only)?;
        let pages = self.read_pages(&mut screens, &mut notes);
        // A stack with no screens (the gameplay stack at the menus) has nothing to show.
        let screens: Vec<Value> = screens
            .iter()
            .zip(&pages)
            .filter(|(screen, _)| !screen.scene.is_empty())
            .map(|(screen, page)| {
                let scale = Scale::of(screen, window.width as f64, window.height as f64);
                let mut json = ui_tree::screen_json(screen, scale, args.view);
                if let Some(href) = page {
                    json["source"] = json!("oreui");
                    json["href"] = json!(href);
                }
                json
            })
            .collect();
        let mut out = json!({ "screens": screens, "window": [window.width, window.height] });
        if !notes.is_empty() {
            out["notes"] = json!(notes);
        }
        Ok(out)
    }

    /// Replace each Ore UI page among `screens` (the game names them by URL and they have no
    /// control tree) with the elements of the live view that draws it. Returns the page's
    /// `location.href` for each screen that now comes from Ore UI. Adds each reason it could
    /// not to `notes` once, so a scrolling `ui_click` does not repeat it.
    pub(super) fn read_pages(
        &mut self,
        screens: &mut [UiScreen],
        notes: &mut Vec<String>,
    ) -> Vec<Option<String>> {
        let mut pages = vec![None; screens.len()];
        let mut reasons = Vec::new();
        let is_page = |screen: &UiScreen| screen.nodes.is_empty() && oreui::is_page(&screen.scene);
        if screens.iter().any(is_page) && self.unstable_requestable(OREUI, &mut reasons) {
            match self.unstable.oreui(None) {
                Ok(views) => {
                    for (screen, page) in screens.iter_mut().zip(&mut pages) {
                        if !is_page(screen) {
                            continue;
                        }
                        match oreui::view_for(&screen.scene, &views) {
                            Some(view) => {
                                *page = Some(view.href.clone());
                                *screen = oreui::screen(view, screen.stack, &screen.scene);
                            }
                            None => {
                                reasons.push(format!("no live Ore UI view draws {}", screen.scene))
                            }
                        }
                    }
                }
                Err(error) => reasons.push(format!("reading the Ore UI pages failed: {error}")),
            }
        }
        for reason in reasons {
            if !notes.contains(&reason) {
                notes.push(reason);
            }
        }
        pages
    }

    /// Click the button or toggle labelled `click.text` through the UI tree, scrolling its
    /// list until it shows. None, with the reason in `notes`, when the tree cannot do it and
    /// OCR should.
    pub(super) fn click_with_tree(
        &mut self,
        click: &TreeClick<'_>,
        notes: &mut Vec<String>,
    ) -> Result<Option<Value>> {
        self.connect_running_agent();
        if !self.unstable_requestable(CAPABILITY, notes) {
            return Ok(None);
        }
        let pid = self.running_pid()?;
        let window = self.game_window(pid)?;
        let (width, height) = (window.width as f64, window.height as f64);
        let mut scrolls = 0;
        let mut previous: Option<[f32; 4]> = None;
        loop {
            let mut screens = match self.unstable.ui_tree(None, None, true) {
                Ok(screens) => screens,
                Err(error) => {
                    notes.push(format!("the UI tree failed ({error}); used OCR"));
                    return Ok(None);
                }
            };
            let pages = self.read_pages(&mut screens, notes);
            let scales: Vec<Option<Scale>> = screens
                .iter()
                .map(|screen| Scale::of(screen, width, height))
                .collect();
            let found =
                ui_tree::candidates(&screens, click.text, click.exact, |screen, rect| {
                    match (scales[screen], &click.region) {
                        (None, _) => false,
                        (Some(_), None) => true,
                        (Some(scale), Some(region)) => ui_tree::in_region(region, scale, rect),
                    }
                });
            let Some(target) = found.into_iter().find(|target| {
                click
                    .control
                    .is_none_or(|keep| keep(&screens[target.screen].nodes[target.node].name))
            }) else {
                let (with_tree, without): (Vec<_>, Vec<_>) = screens
                    .iter()
                    .filter(|screen| !screen.scene.is_empty())
                    .partition(|screen| !screen.nodes.is_empty());
                let names = |screens: Vec<&protocol::unstable::UiScreen>| {
                    screens
                        .iter()
                        .map(|screen| screen.scene.clone())
                        .collect::<Vec<_>>()
                };
                let mut note = format!(
                    "no button or toggle labelled {:?} in the UI trees of {:?}",
                    click.text,
                    names(with_tree)
                );
                if !without.is_empty() {
                    note += &format!("; {:?} have no UI control tree", names(without));
                }
                notes.push(note + "; used OCR");
                return Ok(None);
            };
            let scale = scales[target.screen].expect("filtered to scaled screens");
            if target.shown() {
                let [x, y, w, h] = scale.rect(target.rect);
                let (cx, cy) = (x + w / 2.0, y + h / 2.0);
                self.input(serde_json::from_value(
                    json!({ "pointer": [cx, cy], "press": [click.button] }),
                )?)?;
                self.input(serde_json::from_value(json!({ "pointer": null }))?)?;
                let screen = &screens[target.screen];
                if !target.enabled {
                    notes.push("the control is disabled; the click may do nothing".into());
                }
                return Ok(Some(json!({
                    "clicked": { "text": target.label, "x": x, "y": y, "width": w, "height": h },
                    "match": match target.how {
                        Match::Exact => "exact",
                        Match::Contains => "contains",
                        Match::Fuzzy => "fuzzy",
                    },
                    "button": click.button,
                    "scrolls": scrolls,
                    "source": if pages[target.screen].is_some() { "oreui" } else { "ui_tree" },
                    "control": screen.nodes[target.node].name,
                    "screen": screen.scene,
                    "notes": notes,
                })));
            }
            if scrolls >= click.max_scrolls || previous == Some(target.rect) {
                self.input(serde_json::from_value(json!({ "pointer": null }))?)?;
                notes.push(format!(
                    "{:?} stays outside its list's visible part after {scrolls} scrolls; used OCR",
                    target.label
                ));
                return Ok(None);
            }
            let (at, below) = target.scroll_at();
            let (x, y) = scale.point(at);
            let notches = if below {
                -SCROLL_NOTCHES
            } else {
                SCROLL_NOTCHES
            };
            self.input(serde_json::from_value(
                json!({ "pointer": [x, y], "wheel": { "y": notches } }),
            )?)?;
            self.clock.sleep(SCROLL_SETTLE);
            previous = Some(target.rect);
            scrolls += 1;
        }
    }
}
