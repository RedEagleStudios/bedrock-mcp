//! Client worlds: list them, set their packs, and open or leave one through the menus.

use std::{path::Path, time::Duration};

use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    CLOSABLE_SCREENS, Session,
    ui::{self, TreeClick},
};
use crate::clock::{Clock, poll};
use crate::{
    error::{Error, Result, bail},
    game_files,
    packs::{self, Kind, Pack},
    ui_tree,
    unstable::live::{self, Menu},
    worlds::{self, WorldFolder, Written},
};
use protocol::unstable::UiScreen;

/// How long opening a world may take by default, from the click to the player in it.
const DEFAULT_OPEN_TIMEOUT: Duration = Duration::from_secs(120);
/// How long the world list or main menu may take to show.
const MENU_WAIT: Duration = Duration::from_secs(20);
/// How long a menu gets to open after a click.
const MENU_SETTLE: Duration = Duration::from_millis(1500);
/// How long leaving gets to reach the main menu, and how often it looks.
const LEAVE_TIMEOUT: Duration = Duration::from_secs(60);
const LEAVE_POLL: Duration = Duration::from_millis(250);
/// How long a world gets to show it started loading after a click on it reported an error:
/// the game may not present frames while a world loads, so the click's own wait for the
/// release frame can give up although the click landed, and the code polls for the load to
/// start.
const LOADING_START: Duration = Duration::from_secs(10);
/// How long the HUD gets to take the input once the player is in the world.
const HUD_WAIT: Duration = Duration::from_secs(20);
/// The play screen's tab of local worlds, by its label and its control (both in the game's
/// public Ore UI data).
const WORLDS_TAB: &str = "Worlds";
const WORLDS_TAB_CONTROL: &str = "play-screen-tab-bar-all";
/// Wheel scrolls a world list may take to bring the card into view.
const WORLD_SCROLLS: u32 = 10;
/// The world list shows a page of cards at a time, with a button per page number named
/// `pagination-index-<n>` (in the game's public Ore UI scripts). Pages world_open turns at most.
const PAGE_BUTTON: &str = "pagination-index-";
const WORLD_PAGES: u32 = 100;

/// Look for something on each page of a paged list: the page showing first, then pages 1, 2,
/// … up to `max_pages`, each opened by `open`, which returns false when the list has no such
/// page and so ends the search.
fn search_pages<S, T>(
    max_pages: u32,
    mut find: impl FnMut(&mut S) -> Result<Option<T>>,
    mut open: impl FnMut(&mut S, u32) -> Result<bool>,
    state: &mut S,
) -> Result<Option<T>> {
    if let Some(found) = find(state)? {
        return Ok(Some(found));
    }
    for page in 1..=max_pages {
        if !open(state, page)? {
            break;
        }
        if let Some(found) = find(state)? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// Whether a play screen control is a local world's card: the game's public Ore UI scripts
/// name the main button of a local world's card `world-card-primary-action-<n>` or
/// `multiplayer-world-card-primary-action-<n>`, unless the card brings its own name, as other
/// cards in the list do.
fn local_card(control: &str) -> bool {
    control
        .strip_prefix("multiplayer-")
        .unwrap_or(control)
        .strip_prefix("world-card-primary-action-")
        .is_some_and(|index| !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Which menu `world_open` sees.
enum Shown {
    Main,
    /// The world list (an Ore UI page over the main menu).
    List,
    /// A world is loading or open.
    Loading,
    /// Another screen is on top.
    Other(String),
    /// Nothing tells: no screens capability.
    Unknown,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldOpenArgs {
    name: String,
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldPacksArgs {
    world: String,
    behavior: Option<Vec<String>>,
    resource: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldsListArgs {
    #[serde(default)]
    verbose: bool,
    world: Option<String>,
}

/// A world's applied packs of one kind, with the name of each where it is installed.
fn applied(shared: &std::path::Path, world: &WorldFolder, kind: Kind) -> Result<Vec<Value>> {
    let available = worlds::available_packs(shared, world, kind);
    let list = packs::read_pack_list(&world.path.join(kind.world_list()))?;
    Ok(list
        .iter()
        .map(|entry| {
            let pack: Option<&Pack> = available.iter().find(|pack| pack.uuid == entry.pack_id);
            json!({
                "pack_id": entry.pack_id,
                "version": entry.version,
                "name": pack.map(|pack| pack.name.as_str()),
                "location": pack.map(|pack| pack.location.as_str()),
                "folder": pack.map(|pack| pack.folder.as_str()),
            })
        })
        .collect())
}

/// A world with every applied pack in full.
fn world_json(shared: &std::path::Path, world: &WorldFolder) -> Result<Value> {
    Ok(json!({
        "name": world.name,
        "folder": world.folder,
        "last_played": world.last_played_utc(),
        "behavior_packs": applied(shared, world, Kind::Behavior)?,
        "resource_packs": applied(shared, world, Kind::Resource)?,
    }))
}

/// A world with only the name of each applied pack, or its uuid when it is not installed.
fn world_summary(shared: &std::path::Path, world: &WorldFolder) -> Result<Value> {
    let names = |kind| -> Result<Vec<Value>> {
        Ok(applied(shared, world, kind)?
            .into_iter()
            .map(|pack| match &pack["name"] {
                Value::Null => pack["pack_id"].clone(),
                name => name.clone(),
            })
            .collect())
    };
    Ok(json!({
        "name": world.name,
        "folder": world.folder,
        "last_played": world.last_played_utc(),
        "behavior_packs": names(Kind::Behavior)?,
        "resource_packs": names(Kind::Resource)?,
    }))
}

/// The `worlds_list` reply for the worlds under `shared`: every world in short, or with
/// `verbose` or a `world` filter each in full.
fn worlds_reply(shared: &Path, args: &WorldsListArgs) -> Result<Value> {
    let (worlds, full) = match &args.world {
        Some(key) => (worlds::matching(shared, key)?, true),
        None => (worlds::list(shared), args.verbose),
    };
    let worlds = worlds
        .iter()
        .map(|world| {
            if full {
                world_json(shared, world)
            } else {
                world_summary(shared, world)
            }
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({ "worlds": worlds }))
}

impl Session {
    pub(crate) fn worlds_list(&mut self, args: WorldsListArgs) -> Result<Value> {
        worlds_reply(&game_files::shared_dir(&self.install.prefix)?, &args)
    }

    pub(crate) fn world_packs_set(&mut self, args: WorldPacksArgs) -> Result<Value> {
        if args.behavior.is_none() && args.resource.is_none() {
            bail!(
                "Give behavior, resource or both: the full list of packs the world should apply."
            );
        }
        let shared = game_files::shared_dir(&self.install.prefix)?;
        let world = worlds::find(&shared, &args.world)?;
        let mut notes = Vec::new();
        if self.install.game_pid().is_some() {
            match self.in_world(&mut notes).0 {
                Some(true) => bail!(
                    "The game is in a world, and the game may rewrite the open world's pack \
                     lists when it closes it, so world_packs_set refuses; call world_leave first."
                ),
                Some(false) => notes.clear(),
                None => notes.push(
                    "Nothing told whether the game is in a world; if it is, the game may rewrite \
                     the open world's pack lists when it closes it, so leave it and set them again."
                        .to_owned(),
                ),
            }
        }
        for (kind, references) in [
            (Kind::Behavior, &args.behavior),
            (Kind::Resource, &args.resource),
        ] {
            let Some(references) = references else {
                continue;
            };
            let available = worlds::available_packs(&shared, &world, kind);
            let mut list = Vec::new();
            for reference in references {
                let entry = worlds::entry(worlds::resolve(&available, reference)?);
                if !list.contains(&entry) {
                    list.push(entry);
                }
            }
            packs::write_pack_list(&world.path.join(kind.world_list()), &list)?;
        }
        let mut reply = world_json(&shared, &world)?;
        reply["note"] = json!(
            "The game reads a world's packs when it opens the world; experiments such as Beta \
             APIs live in the world settings."
        );
        reply["notes"] = json!(notes);
        Ok(reply)
    }

    pub(crate) fn world_open(&mut self, args: WorldOpenArgs) -> Result<Value> {
        let shared = game_files::shared_dir(&self.install.prefix)?;
        let world = worlds::find(&shared, &args.name)?;
        self.running_pid()?;
        if self.in_world(&mut Vec::new()).0 == Some(true) {
            bail!("The game is already in a world; call world_leave first.");
        }
        let timeout = args
            .timeout_ms
            .map_or(DEFAULT_OPEN_TIMEOUT, Duration::from_millis);
        // The menus show no folder id. The code assumes worlds that share a name show a card
        // each, most recently played first, and picks this world's card by its place among
        // them; it checks which folder loaded and leaves on a mismatch.
        let namesakes = worlds::namesakes(&worlds::list(&shared), &world.name);
        let card = (namesakes.len() > 1).then(|| {
            namesakes
                .iter()
                .position(|other| other.folder == world.folder)
                .unwrap_or(0)
        });
        let mut clicked = Vec::new();
        let mut notes = Vec::new();
        // The code clicks Play only on the main menu; on the world list it clicks this
        // world's card.
        let clock = self.clock.clone();
        let deadline = clock.now() + MENU_WAIT;
        let mut seen = String::new();
        let mut failed = None;
        // The world's files as they were before the click on it, to tell whether the click
        // loads that world.
        let mut before = None;
        loop {
            match self.menu_now() {
                // A click on a menu that is still building can fail; the next look decides.
                Shown::Main => match self.click_label("Play", false, false) {
                    Ok(play) => clicked.push(play),
                    Err(error) => failed = Some(error.to_string()),
                },
                Shown::List => {
                    let (entry, written) =
                        self.click_world(&shared, &world.name, card, &mut notes)?;
                    clicked.push(entry);
                    before = Some(written);
                    break;
                }
                Shown::Loading => {
                    notes.push(format!(
                        "A world was already loading when world_open looked, so nothing was \
                         clicked; it waited for that world, which may not be {:?}.",
                        world.name
                    ));
                    break;
                }
                Shown::Unknown => {
                    // Without the screens capability: the world list may be up, else the
                    // main menu's Play button opens it.
                    let written = Written::now(&shared, &world.name);
                    if let Ok(entry) =
                        self.click_world_card(&world.name, card, false, Duration::ZERO)
                    {
                        clicked.push(entry);
                        before = Some(written);
                        break;
                    }
                    if let Ok(play) = self.click_label("Play", false, false) {
                        clicked.push(play);
                        clock.sleep(MENU_SETTLE);
                        let (entry, written) =
                            self.click_world(&shared, &world.name, card, &mut notes)?;
                        clicked.push(entry);
                        before = Some(written);
                        break;
                    }
                }
                Shown::Other(screen) => seen = screen,
            }
            if clock.now() >= deadline {
                let showing = if seen.is_empty() {
                    String::new()
                } else {
                    format!(" ({seen} was on top)")
                };
                let click = failed.map_or_else(String::new, |error| {
                    format!(" The last click on Play failed: {error}")
                });
                bail!(
                    "Neither the world list nor the main menu showed within {} s{showing}; \
                     check frame_capture.{click}",
                    MENU_WAIT.as_secs()
                );
            }
            clock.sleep(MENU_SETTLE);
        }
        if let Some(before) = &before {
            self.confirm_local(before, &world.name, &mut notes)?;
        }
        let mut waited = self.wait_in_world(timeout, &mut notes)?;
        // The code expects loading a local world to write its files, and treats files left
        // unwritten as another host's game, whatever the screens showed.
        if let Some(before) = &before
            && !before.changed()
        {
            return Err(self.refuse_join(
                &world.name,
                "the world's files stayed unwritten, so the game is not this local world",
                true,
            ));
        }
        // Worlds that share the name: the click must have loaded this folder, not another.
        if let Some(before) = &before
            && card.is_some()
            && !before.changed_paths().any(|path| path == world.path)
        {
            let opened: Vec<String> = before
                .changed_paths()
                .filter_map(|path| path.file_name())
                .map(|folder| folder.to_string_lossy().into_owned())
                .collect();
            let left = match self.world_leave() {
                Ok(_) => "left it again".to_owned(),
                Err(error) => format!("could not leave it ({error}); call world_leave"),
            };
            bail!(
                "Several worlds are named {:?} and the click opened folder {} instead of {}, so \
                 world_open {left}. The world list did not order them by last played; rename \
                 one of them in the game to tell them apart.",
                world.name,
                opened.join(", "),
                world.folder
            );
        }
        // The player can be in the world while the loading screen still closes; tools that
        // type into chat need gameplay on screen (the HUD, or a pause or chat screen that
        // Escape closes), so wait for it.
        if waited["satisfied"] == true {
            let hud = self.clock.now();
            let clock = self.clock.clone();
            let mut last = None;
            let shown = poll(&*clock, HUD_WAIT, LEAVE_POLL, || {
                last = self.covering_screen();
                let ready = last.as_deref().is_none_or(|screen| {
                    CLOSABLE_SCREENS
                        .iter()
                        .any(|closable| screen.contains(closable))
                });
                Ok(ready.then_some(()))
            })?;
            waited["hud_ms"] = ((self.clock.now() - hud).as_millis() as u64).into();
            if shown.is_none() {
                notes.push(format!(
                    "Gameplay did not come up within {} s of the player entering the world{}; \
                     close what covers it before automation_connect.",
                    HUD_WAIT.as_secs(),
                    last.map_or_else(String::new, |screen| format!(" ({screen} was on top)")),
                ));
            }
        }
        Ok(json!({
            "world": { "name": world.name, "folder": world.folder },
            "clicked": clicked,
            "waited": waited,
            "notes": notes,
            "next": "automation_connect, to run commands and use the Bridge pack",
        }))
    }

    /// Show the local worlds, then click the card of the world named exactly `name`: the
    /// `card`-th of the cards so labelled when several worlds share the name. Returns the click
    /// and the world files as they were before it.
    fn click_world(
        &mut self,
        shared: &Path,
        name: &str,
        card: Option<usize>,
        notes: &mut Vec<String>,
    ) -> Result<(Value, Written)> {
        self.select_worlds_tab(notes);
        let before = Written::now(shared, name);
        let entry = self.click_world_card(name, card, true, MENU_WAIT);
        Ok((self.loading_after(entry, notes)?, before))
    }

    /// Wait up to `timeout` for the player to be in the world: by the screens capability or
    /// the WebSocket, else, as a last resort, until OCR reads no menu or loading text.
    fn wait_in_world(&mut self, timeout: Duration, notes: &mut Vec<String>) -> Result<Value> {
        let mut reasons = Vec::new();
        if self.in_world(&mut reasons).1.is_some() {
            return self.wait_for(serde_json::from_value(json!({
                "condition": "in_world",
                "timeout_ms": timeout.as_millis() as u64,
            }))?);
        }
        let started = self.clock.now();
        if let Err(why) = self.wait_for_gameplay_text(timeout) {
            bail!(
                "The player did not seem to enter the world within {} s: {why}. Check \
                 frame_capture.",
                timeout.as_secs()
            );
        }
        notes.push(format!(
            "Nothing could read the screens or the WebSocket ({}), so world_open took the \
             first screen without menu or loading text as the player being in the world; \
             check frame_capture before the next tool.",
            reasons.join("; ")
        ));
        Ok(json!({
            "satisfied": true,
            "waited_ms": (self.clock.now() - started).as_millis() as u64,
            "detail": { "in_world": true, "source": "ocr" },
        }))
    }

    /// Whether the UI tree reads the Ore UI play screen, which names each card.
    fn reads_play_screen(&mut self, notes: &mut Vec<String>) -> bool {
        self.unstable_requestable(ui::CAPABILITY, notes)
            && self.unstable_requestable(ui::OREUI, notes)
    }

    /// The play screen's Worlds tab, which holds the local worlds; Realms and Servers list
    /// other hosts' games. Looks again for up to [`MENU_WAIT`] while the page builds. Without
    /// the UI tree the list stays as it is.
    fn select_worlds_tab(&mut self, notes: &mut Vec<String>) {
        let mut reasons = Vec::new();
        let tab = TreeClick {
            text: WORLDS_TAB,
            exact: false,
            max_scrolls: 0,
            button: "MouseLeft",
            region: None,
            control: Some(&|control| control == WORLDS_TAB_CONTROL),
        };
        let clock = self.clock.clone();
        let deadline = clock.now() + MENU_WAIT;
        while self.reads_play_screen(&mut reasons) {
            reasons.clear();
            if self
                .click_with_tree(&tab, &mut reasons)
                .is_ok_and(|reply| reply.is_some())
            {
                clock.sleep(MENU_SETTLE);
                return;
            }
            if clock.now() >= deadline {
                break;
            }
            clock.sleep(LEAVE_POLL);
        }
        notes.push(format!(
            "world_open could not select the {WORLDS_TAB} tab ({}); it looked in the list that \
             was showing.",
            reasons.join("; ")
        ));
    }

    /// Click the local world card labelled exactly `name`, or with `card` the card-th of the
    /// cards so labelled (several worlds share the name). Waits up to `wait` for the list to
    /// build (a page that just opened has no control tree yet); with `scroll` it scrolls each
    /// page of the list and then turns the pages by their number buttons until the card
    /// shows. With the UI tree it never falls back to OCR, which cannot tell a friend's world
    /// card from a local one.
    fn click_world_card(
        &mut self,
        name: &str,
        card: Option<usize>,
        scroll: bool,
        wait: Duration,
    ) -> Result<Value> {
        let mut notes = Vec::new();
        if !self.reads_play_screen(&mut notes) {
            if card.is_some_and(|index| index > 0) {
                bail!(
                    "Several worlds are named {name:?} and only the UI tree tells their cards \
                     apart ({}); a click on the text would open the most recently played one.",
                    notes.join("; ")
                );
            }
            return self.click_label(name, scroll, true);
        }
        if let Some(index) = card {
            return self.click_namesake_card(name, index, scroll, wait);
        }
        let card = TreeClick {
            text: name,
            exact: true,
            max_scrolls: if scroll { WORLD_SCROLLS } else { 0 },
            button: "MouseLeft",
            region: None,
            control: Some(&local_card),
        };
        let clock = self.clock.clone();
        let deadline = clock.now() + wait;
        loop {
            notes.clear();
            if let Some(reply) = self.click_with_tree(&card, &mut notes)? {
                return Ok(reply);
            }
            if self.world_list_ready() || clock.now() >= deadline {
                break;
            }
            clock.sleep(LEAVE_POLL);
        }
        let found = if scroll {
            let mut tried = Vec::new();
            search_pages(
                WORLD_PAGES,
                |this: &mut Self| {
                    tried.clear();
                    this.click_with_tree(&card, &mut tried)
                },
                |this: &mut Self, page| this.turn_page(page),
                self,
            )?
        } else {
            None
        };
        match found {
            Some(reply) => Ok(reply),
            None => bail!(
                "No local world card is labelled exactly {name:?} on any page of the play \
                 screen's list ({}); world_open clicks nothing else, such as a friend's world.",
                notes.join("; ")
            ),
        }
    }

    /// Whether the play screen's world list has built: it shows a local card or page buttons.
    fn world_list_ready(&mut self) -> bool {
        let Ok(mut screens) = self.unstable.ui_tree(None, None, true) else {
            return false;
        };
        self.read_pages(&mut screens, &mut Vec::new());
        screens
            .iter()
            .flat_map(|screen| &screen.nodes)
            .any(|node| local_card(&node.name) || node.name.starts_with(PAGE_BUTTON))
    }

    /// Open page `page` of the world list by its number button; false when it has none.
    fn turn_page(&mut self, page: u32) -> Result<bool> {
        let number = page.to_string();
        let button = TreeClick {
            text: &number,
            exact: true,
            max_scrolls: WORLD_SCROLLS,
            button: "MouseLeft",
            region: None,
            control: Some(&|control| control.starts_with(PAGE_BUTTON)),
        };
        let turned = self.click_with_tree(&button, &mut Vec::new())?.is_some();
        if turned {
            self.clock.sleep(MENU_SETTLE);
        }
        Ok(turned)
    }

    /// Click the `index`-th local card labelled exactly `name` (0 the first), counted in list
    /// order from the first page: several worlds share the name, and the code assumes the
    /// list shows them most recently played first. Without `scroll` it looks only at the page
    /// showing.
    fn click_namesake_card(
        &mut self,
        name: &str,
        index: usize,
        scroll: bool,
        wait: Duration,
    ) -> Result<Value> {
        let clock = self.clock.clone();
        let deadline = clock.now() + wait;
        while !self.world_list_ready() && clock.now() < deadline {
            clock.sleep(LEAVE_POLL);
        }
        if scroll {
            // A list of one page has no page buttons; it shows page 1 anyway.
            self.turn_page(1)?;
        }
        let (mut skip, mut page, mut seen) = (index, 1, 0);
        loop {
            let cards = match self.unstable.ui_tree(None, None, true) {
                Ok(mut screens) => {
                    self.read_pages(&mut screens, &mut Vec::new());
                    cards_named(&screens, name)
                }
                Err(_) => Vec::new(),
            };
            seen += cards.len();
            if let Some(control) = cards.get(skip) {
                let keep = |other: &str| other == control;
                let click = TreeClick {
                    text: name,
                    exact: true,
                    max_scrolls: if scroll { WORLD_SCROLLS } else { 0 },
                    button: "MouseLeft",
                    region: None,
                    control: Some(&keep),
                };
                let mut notes = Vec::new();
                return match self.click_with_tree(&click, &mut notes)? {
                    Some(reply) => Ok(reply),
                    None => bail!(
                        "Could not click card {} of the local world cards labelled {name:?} ({}).",
                        index + 1,
                        notes.join("; ")
                    ),
                };
            }
            skip -= cards.len();
            page += 1;
            if !scroll || page > WORLD_PAGES || !self.turn_page(page)? {
                break;
            }
        }
        bail!(
            "Several worlds are named {name:?}, and this one is number {} of them by last \
             played, but the play screen's list showed {seen} local world cards so labelled; \
             world_open clicks nothing else, so it could not tell which card is this world.",
            index + 1
        )
    }

    /// Make sure the click loads a local world: its loading screen names a local job or the
    /// world's files change. Another host's game (a friend's world, a Realm or a server) is
    /// cancelled and fails the call: an agent must never join someone else's game by accident.
    fn confirm_local(
        &mut self,
        before: &Written,
        name: &str,
        notes: &mut Vec<String>,
    ) -> Result<()> {
        let clock = self.clock.clone();
        let joined = poll(&*clock, LOADING_START, LEAVE_POLL, || {
            Ok(self.join_seen(before))
        })?;
        match joined {
            Some(None) => Ok(()),
            Some(Some(job)) => Err(self.refuse_join(
                name,
                &format!("its loading screen showed the job {job}"),
                false,
            )),
            None => {
                notes.push(format!(
                    "Neither a loading screen nor the world's files showed within {} s that the \
                     click loads the local world; world_open checked again once the player was in.",
                    LOADING_START.as_secs()
                ));
                Ok(())
            }
        }
    }

    /// What the game started loading after the click: Some(None) for the local world (its
    /// files changed), Some(Some(job)) when a loading screen shows another host's game, None
    /// before either shows.
    fn join_seen(&mut self, before: &Written) -> Option<Option<String>> {
        if self.unstable_capability("screens", &mut Vec::new())
            && let Ok(stacks) = self.unstable.screens()
            && let Some(Some(job)) = live::loading_job(&stacks)
        {
            return Some(Some(job.to_owned()));
        }
        before.changed().then_some(None)
    }

    /// Cancel joining another host's game, or leave it when the player already `entered` it
    /// (or the screens say so), and say what happened.
    fn refuse_join(&mut self, name: &str, what: &str, entered: bool) -> Error {
        let outcome = match self.cancel_join(entered) {
            Ok(outcome) => outcome,
            Err(error) => format!("could not cancel it ({error}); check frame_capture"),
        };
        Error::msg(format!(
            "The click on {name:?} started joining another host's game ({what}) instead of \
             loading the local world, so world_open {outcome}. world_open opens only local \
             worlds: check that the play screen's Worlds tab lists {name:?} (worlds_list shows \
             the local names)."
        ))
    }

    fn cancel_join(&mut self, entered: bool) -> Result<String> {
        if entered || self.in_world(&mut Vec::new()).0 == Some(true) {
            self.world_leave()?;
            return Ok("left that game again".to_owned());
        }
        if self.click_label("Cancel", false, true).is_err() {
            self.input(serde_json::from_value(json!({ "press": ["Escape"] }))?)?;
        }
        if !self.unstable_capability("screens", &mut Vec::new()) {
            return Ok(match self.wait_for_menu_text(LEAVE_TIMEOUT) {
                Ok(_) => "cancelled the join (OCR read the menu again)".to_owned(),
                Err(why) => format!(
                    "pressed Cancel, but nothing confirmed the menu came back ({why}); check \
                     frame_capture"
                ),
            });
        }
        let clock = self.clock.clone();
        let settled = poll(&*clock, LEAVE_TIMEOUT, LEAVE_POLL, || {
            if self.in_world(&mut Vec::new()).0 == Some(true) {
                return Ok(Some(true));
            }
            Ok((!matches!(self.menu_now(), Shown::Loading)).then_some(false))
        })?;
        Ok(match settled {
            Some(true) => {
                self.world_leave()?;
                "left that game again".to_owned()
            }
            Some(false) => "cancelled the join".to_owned(),
            None => format!(
                "pressed Cancel, but the loading screen stayed for {} s; check frame_capture",
                LEAVE_TIMEOUT.as_secs()
            ),
        })
    }

    /// The click on a world's name, or, when it failed but a world started loading (loading
    /// stopped the frames the click waited for, or the click landed while the list scrolled),
    /// a note saying so.
    fn loading_after(&mut self, entry: Result<Value>, notes: &mut Vec<String>) -> Result<Value> {
        let error = match entry {
            Ok(entry) => return Ok(entry),
            Err(error) => error,
        };
        let clock = self.clock.clone();
        if !loading_starts(&*clock, || self.menu_now())? {
            return Err(error);
        }
        notes.push(format!(
            "The click on the world's name reported an error ({error}), but a world started \
             loading, so world_open waited for it."
        ));
        Ok(Value::Null)
    }

    /// Which menu is up, from the screen stacks; Unknown without the screens capability.
    fn menu_now(&mut self) -> Shown {
        let mut notes = Vec::new();
        if self.unstable_capability("screens", &mut notes)
            && let Ok(stacks) = self.unstable.screens()
        {
            return match live::menu(&stacks) {
                Menu::InWorld | Menu::Loading => Shown::Loading,
                Menu::Main => Shown::Main,
                Menu::Page => Shown::List,
                Menu::Other(top) => Shown::Other(top.to_owned()),
            };
        }
        Shown::Unknown
    }

    pub(crate) fn world_leave(&mut self) -> Result<Value> {
        self.running_pid()?;
        let mut notes = Vec::new();
        if self.in_world(&mut Vec::new()).0 == Some(false) {
            bail!("The game is not in a world.");
        }
        let screens = self.unstable_capability("screens", &mut notes);
        let mut clicked = Vec::new();
        // Save & Quit is on the pause menu. Without the screens capability nothing tells
        // whether it is open, so look for the button before pressing Escape.
        let first = if screens && self.covering_screen().is_none() {
            None
        } else {
            self.click_label("Save & Quit", false, false).ok()
        };
        match first {
            Some(quit) => clicked.push(quit),
            None => {
                self.input(serde_json::from_value(json!({ "press": ["Escape"] }))?)?;
                self.clock.sleep(MENU_SETTLE);
                clicked.push(self.click_label("Save & Quit", false, false)?);
            }
        }
        let clock = self.clock.clone();
        let started = clock.now();
        if screens {
            // Saving shows a screen over the menus after the gameplay stack is gone; wait for
            // the main menu or the world list, so world_open can follow at once.
            let left = poll(&*clock, LEAVE_TIMEOUT, LEAVE_POLL, || {
                Ok(matches!(self.menu_now(), Shown::Main | Shown::List).then_some(()))
            })?;
            if left.is_none() {
                if self.in_world(&mut Vec::new()).0 != Some(false) {
                    bail!(
                        "Clicked Save & Quit, but the game was still in the world after {} s; \
                         check frame_capture.",
                        LEAVE_TIMEOUT.as_secs()
                    );
                }
                notes.push(format!(
                    "The world closed, but neither the main menu nor the world list showed \
                     within {} s; check frame_capture.",
                    LEAVE_TIMEOUT.as_secs()
                ));
            }
        } else if let Err(why) = self.wait_for_menu_text(LEAVE_TIMEOUT) {
            bail!("Clicked Save & Quit, but the menu did not show: {why}. Check frame_capture.");
        } else {
            notes.push(
                "Without the screens capability, OCR reading the menu confirmed the world closed."
                    .to_owned(),
            );
        }
        Ok(json!({
            "clicked": clicked,
            "waited_ms": (clock.now() - started).as_millis() as u64,
            "notes": notes,
        }))
    }

    /// Click the on-screen text `label`, scrolling the list when asked. `exact` takes only a
    /// label equal to it (ignoring case), never one that contains it or looks like it.
    fn click_label(&mut self, label: &str, scroll: bool, exact: bool) -> Result<Value> {
        self.click_text(serde_json::from_value(
            json!({ "text": label, "scroll": scroll, "exact": exact }),
        )?)
    }
}

/// Whether `menu` shows a world loading within [`LOADING_START`].
fn loading_starts(clock: &dyn Clock, mut menu: impl FnMut() -> Shown) -> Result<bool> {
    let loading = poll(clock, LOADING_START, LEAVE_POLL, || {
        Ok(matches!(menu(), Shown::Loading).then_some(()))
    })?;
    Ok(loading.is_some())
}

/// The controls of the local world cards labelled exactly `name` (ignoring case), in the
/// order the list shows them: top to bottom, then left to right.
fn cards_named(screens: &[UiScreen], name: &str) -> Vec<String> {
    let mut cards: Vec<ui_tree::Target> = ui_tree::candidates(screens, name, true, |_, _| true)
        .into_iter()
        .filter(|target| local_card(&screens[target.screen].nodes[target.node].name))
        .collect();
    cards.sort_by(|a, b| {
        a.screen
            .cmp(&b.screen)
            .then(a.rect[1].total_cmp(&b.rect[1]))
            .then(a.rect[0].total_cmp(&b.rect[0]))
    });
    cards
        .into_iter()
        .map(|target| screens[target.screen].nodes[target.node].name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{clock::fake::FakeClock, packs::GlobalPack, testdir::TestDir, worlds::fake};
    use protocol::unstable::UiNode;
    use std::fs;

    /// A shared folder with `count` worlds, each applying `behavior` installed behavior packs
    /// and one resource pack that is not installed.
    fn many_worlds(dir: &Path, count: usize, behavior: usize) {
        for index in 0..behavior {
            let pack = dir.join(format!("development_behavior_packs/pack_{index}"));
            fs::create_dir_all(&pack).unwrap();
            fs::write(
                pack.join("manifest.json"),
                format!(
                    r#"{{"header": {{"name": "Pack {index}", "uuid": "00000000-0000-0000-0000-00000000000{index}", "version": [1, 0, 0]}}, "modules": [{{"type": "data"}}]}}"#
                ),
            )
            .unwrap();
        }
        for world in 0..count {
            let path = fake::world(
                dir,
                &format!("folder{world:03}="),
                &format!("World {world}"),
                Some(1_762_000_000 + world as i64),
            );
            let list = |uuids: Vec<String>| -> Vec<GlobalPack> {
                uuids
                    .into_iter()
                    .map(|pack_id| GlobalPack {
                        pack_id,
                        version: json!([1, 0, 0]),
                    })
                    .collect()
            };
            packs::write_pack_list(
                &path.join(Kind::Behavior.world_list()),
                &list(
                    (0..behavior)
                        .map(|index| format!("00000000-0000-0000-0000-00000000000{index}"))
                        .collect(),
                ),
            )
            .unwrap();
            packs::write_pack_list(
                &path.join(Kind::Resource.world_list()),
                &list(vec!["ffffffff-0000-0000-0000-000000000000".to_owned()]),
            )
            .unwrap();
        }
    }

    fn list_args(arguments: Value) -> WorldsListArgs {
        serde_json::from_value(arguments).unwrap()
    }

    #[test]
    fn worlds_list_is_short_by_default_and_full_when_verbose_or_filtered() {
        let dir = TestDir::new("worlds-list");
        many_worlds(dir.path(), 60, 4);
        let short = worlds_reply(dir.path(), &list_args(json!({}))).unwrap();
        let full = worlds_reply(dir.path(), &list_args(json!({"verbose": true}))).unwrap();
        let (short_size, full_size) = (short.to_string().len(), full.to_string().len());
        println!("worlds_list on 60 worlds: {short_size} bytes by default, {full_size} verbose");
        assert!(short_size * 3 < full_size, "{short_size} vs {full_size}");

        let world = &short["worlds"][0];
        assert_eq!(world["name"], "World 0");
        assert_eq!(world["folder"], "folder000=");
        assert_eq!(world["last_played"], "2025-11-01 12:26 UTC");
        assert_eq!(world["behavior_packs"][3], "Pack 3");
        // A pack the world applies but nothing installs shows by its uuid.
        assert_eq!(
            world["resource_packs"],
            json!(["ffffffff-0000-0000-0000-000000000000"])
        );

        let world = &full["worlds"][0];
        assert_eq!(world["last_played"], "2025-11-01 12:26 UTC");
        assert_eq!(
            world["behavior_packs"][3],
            json!({
                "pack_id": "00000000-0000-0000-0000-000000000003",
                "version": [1, 0, 0],
                "name": "Pack 3",
                "location": "development_behavior_packs",
                "folder": "pack_3",
            })
        );
        assert_eq!(world["resource_packs"][0]["name"], Value::Null);

        // One world, by folder id or name, comes in full.
        let one = worlds_reply(dir.path(), &list_args(json!({"world": "folder007="}))).unwrap();
        assert_eq!(one["worlds"].as_array().unwrap().len(), 1);
        assert_eq!(one["worlds"][0]["behavior_packs"][0]["folder"], "pack_0");
        let named = worlds_reply(dir.path(), &list_args(json!({"world": "world 7"}))).unwrap();
        assert_eq!(named, one);
        assert!(worlds_reply(dir.path(), &list_args(json!({"world": "Nope"}))).is_err());
    }

    #[test]
    fn shared_names_list_every_world_by_folder() {
        let dir = TestDir::new("worlds-list-twins");
        fake::world(dir.path(), "old=", "My World", Some(100));
        fake::world(dir.path(), "new=", "My World", Some(200));
        let reply = worlds_reply(dir.path(), &list_args(json!({"world": "My World"}))).unwrap();
        let folders: Vec<&Value> = reply["worlds"]
            .as_array()
            .unwrap()
            .iter()
            .map(|world| &world["folder"])
            .collect();
        assert_eq!(folders, ["new=", "old="]);
    }

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

    /// A local world card at `y` (and `x`) labelled `label`; its label node follows it.
    fn card(nodes: &mut Vec<UiNode>, control: &str, label: &str, x: f32, y: f32) {
        let parent = nodes.len() as u32;
        nodes.push(UiNode {
            components: vec!["InputComponent".into(), "ButtonComponent".into()],
            child_count: 1,
            ..node(Some(0), control, [x, y, 100.0, 40.0])
        });
        nodes.push(UiNode {
            components: vec!["TextComponent".into()],
            text: Some(label.into()),
            ..node(Some(parent), "label", [x + 5.0, y + 5.0, 50.0, 10.0])
        });
    }

    #[test]
    fn namesake_cards_come_in_list_order() {
        let mut nodes = vec![node(None, "play_screen", [0.0, 0.0, 500.0, 500.0])];
        // Tree order differs from the order on screen.
        card(
            &mut nodes,
            "world-card-primary-action-2",
            "My World",
            0.0,
            100.0,
        );
        card(
            &mut nodes,
            "world-card-primary-action-0",
            "My World",
            0.0,
            10.0,
        );
        card(
            &mut nodes,
            "world-card-primary-action-1",
            "Other",
            0.0,
            55.0,
        );
        card(
            &mut nodes,
            "world-card-primary-action-3",
            "my world",
            120.0,
            100.0,
        );
        card(&mut nodes, "demo-joinable-card-0", "My World", 0.0, 150.0);
        let screens = [UiScreen {
            stack: 1,
            scene: "play".into(),
            nodes,
            truncated: false,
            detail: None,
            read_us: 0,
            fresh: true,
        }];
        assert_eq!(
            cards_named(&screens, "My World"),
            [
                "world-card-primary-action-0",
                "world-card-primary-action-2",
                "world-card-primary-action-3",
            ]
        );
    }

    #[test]
    fn a_failed_click_counts_when_loading_shows_later() {
        // Loading stalls the frames, so the screens still show the world list for a moment.
        let clock = FakeClock::new();
        let mut looks = 0;
        let started = loading_starts(&clock, || {
            looks += 1;
            if looks < 5 {
                Shown::List
            } else {
                Shown::Loading
            }
        });
        assert!(started.unwrap());
        assert_eq!(looks, 5);
        assert!(clock.elapsed() < LOADING_START);
    }

    #[test]
    fn a_failed_click_fails_when_nothing_loads() {
        let clock = FakeClock::new();
        assert!(!loading_starts(&clock, || Shown::List).unwrap());
        assert_eq!(clock.elapsed(), LOADING_START);
    }

    #[test]
    fn world_clicks_keep_to_local_cards() {
        assert!(local_card("world-card-primary-action-3"));
        assert!(local_card("multiplayer-world-card-primary-action-1"));
        assert!(!local_card("demo-joinable-card-0"));
        assert!(!local_card("world-card-primary-action-"));
        assert!(!local_card("world-card-primary-action-2-menu"));
    }

    #[test]
    fn card_search_turns_pages_until_the_card_or_the_last_page() {
        // Pages of a three-page list; the card is on page 3, and page 2 is showing.
        let pages = [vec!["a", "b"], vec!["c"], vec!["d", "Demo"]];
        let find = |showing: &mut usize| Ok(pages[*showing].contains(&"Demo").then_some(*showing));
        let open = |showing: &mut usize, page: u32| {
            let index = page as usize - 1;
            Ok((index < pages.len()).then(|| *showing = index).is_some())
        };
        let mut showing = 1;
        assert_eq!(search_pages(10, find, open, &mut showing).unwrap(), Some(2));
        // Already on the card's page: no page is turned.
        let mut showing = 2;
        let mut turned = 0;
        let counting = |showing: &mut usize, page: u32| {
            turned += 1;
            open(showing, page)
        };
        assert_eq!(
            search_pages(10, find, counting, &mut showing).unwrap(),
            Some(2)
        );
        assert_eq!(turned, 0);
        // Missing everywhere: the search stops after the last page, or at the cap.
        let missing = |_: &mut usize| Ok(None::<usize>);
        let mut opened = Vec::new();
        let record = |showing: &mut usize, page: u32| {
            opened.push(page);
            open(showing, page)
        };
        assert_eq!(search_pages(10, missing, record, &mut 0).unwrap(), None);
        assert_eq!(opened, [1, 2, 3, 4]);
        let mut capped = 0;
        let endless = |_: &mut usize, _: u32| {
            capped += 1;
            Ok(true)
        };
        assert_eq!(search_pages(5, missing, endless, &mut 0).unwrap(), None);
        assert_eq!(capped, 5);
    }
}
