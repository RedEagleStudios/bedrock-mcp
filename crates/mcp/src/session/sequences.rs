//! Action sequences. The session cannot move to another thread, so every run runs on the
//! request thread right after the call that queued it is answered or parked, and the calls
//! that arrive meanwhile are answered at its wait points. A blocking call parks until its run
//! ends or its blocking limit passes; see [`drivers::Background`].

use std::time::Duration;

use protocol::unstable::Request;
use serde_json::{Value, json};

use super::{Session, world_state::parse_entity_id};
use crate::{
    drivers::{self, Game, NativeRead, SequenceArgs, StatusArgs, StopArgs, Vitals},
    error::{Error, Result, bail},
    events::{EventCursor, HubEvent},
    input::InputRequest,
    rpc::Pump,
    tools::arguments,
    world::World,
};

/// The longest wait at a run's wait point between input lock checks.
const LOCK_SLICE: Duration = Duration::from_millis(250);

impl Session {
    /// Queue a sequence. With `background` return its run id and plan at once; otherwise
    /// park the call ([`Error::Waiting`]): the server calls again with the same arguments,
    /// and gets the whole result once the run ended, or its run id once blocking_limit_ms
    /// passed while it goes on in the background.
    pub(crate) fn action_sequence(&mut self, raw: Value) -> Result<Value> {
        if let Some(answer) = self.background.claimed(&raw) {
            return answer;
        }
        let args: SequenceArgs = arguments(raw.clone())?;
        let (background, limit, verbose) = (args.background, args.blocking_limit(), args.verbose);
        let prepared = drivers::prepare_sequence(self, args)?;
        let reply = self.background.queue(prepared)?;
        if background {
            return Ok(reply);
        }
        self.background.claim(raw, limit, verbose);
        Err(Error::Waiting)
    }

    pub(crate) fn sequence_status(&mut self, args: StatusArgs) -> Result<Value> {
        self.background.status(args)
    }

    pub(crate) fn sequence_stop(&mut self, args: StopArgs) -> Result<Value> {
        self.background.stop(args)
    }

    /// Run the queued background sequence, if any, answering calls at its wait points.
    pub(super) fn run_background(&mut self, pump: &mut dyn Pump) {
        let Some(queued) = self.background.take_queued() else {
            return;
        };
        let mut game = Pumped {
            session: self,
            pump,
        };
        drivers::run_queued(&mut game, queued);
    }

    /// Refuse synthetic input from a call answered while a background run plays the
    /// connected player.
    pub(super) fn input_free(&self) -> Result<()> {
        if self.background.nested
            && let Some(id) = self.background.driving_self()
        {
            bail!(
                "Background run {id} plays the connected player, so this input would fight it. \
                 Wait for it with sequence_status or end it with sequence_stop."
            );
        }
        Ok(())
    }

    /// What names the loaded world: the game process and the number of world loads this
    /// session saw it make, so it changes when the game restarts or a world loads again.
    /// None while nothing shows the player in a world.
    pub(crate) fn world_load_key(&mut self) -> Option<String> {
        let pid = self.install.game_pid()?;
        (self.in_world(&mut Vec::new()).0 == Some(true))
            .then(|| format!("{pid}#{}", self.world_loads))
    }

    pub(crate) fn last_warm_key(&self) -> Option<String> {
        self.background.warm_key.clone()
    }

    pub(crate) fn remember_warm_key(&mut self, key: Option<String>) {
        self.background.warm_key = key;
    }

    /// The local player's attachables from player_render: whether any plays an animation or
    /// controller, and their names; `None` when the capability is not active.
    pub(crate) fn held_attachable(&mut self) -> Result<Option<Value>> {
        let render = match self.player_render_get(arguments(json!({ "wait_ms": 500 }))?) {
            Ok(render) => render,
            Err(error) if error.to_string().contains("player_render capability") => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let mut attachables = 0;
        let mut players = Vec::new();
        fn visit(component: &Value, attachables: &mut usize, players: &mut Vec<Value>) {
            for child in component["children"].as_array().into_iter().flatten() {
                *attachables += 1;
                // A controller counts once it is in a state that plays animations.
                for player in child["players"].as_array().into_iter().flatten() {
                    let playing = player["kind"] == "animation"
                        || player["animations"]
                            .as_array()
                            .is_some_and(|animations| !animations.is_empty());
                    let name = match player["state"].as_str() {
                        Some(state) => json!(format!(
                            "{}:{state}",
                            player["name"].as_str().unwrap_or("?")
                        )),
                        None => player["name"].clone(),
                    };
                    if playing && !players.contains(&name) {
                        players.push(name);
                    }
                }
                visit(child, attachables, players);
            }
        }
        for component in render["components"].as_array().into_iter().flatten() {
            visit(component, &mut attachables, &mut players);
        }
        Ok(Some(json!({
            "animating": !players.is_empty(),
            "players": players,
            "attachables": attachables,
        })))
    }

    /// Entity `id` as the unstable layer's `world_state` capability reads it on the game's
    /// local server (single player), for the drivers' health readbacks.
    pub(crate) fn native_vitals(&mut self, id: &str) -> NativeRead {
        let mut notes = Vec::new();
        if !self.native_world(&mut notes) {
            return NativeRead::Unavailable(if notes.is_empty() {
                "the unstable layer's world_state capability is not active".to_owned()
            } else {
                notes.join("; ")
            });
        }
        let id = match parse_entity_id(id) {
            Ok(id) => id,
            Err(error) => return NativeRead::Unavailable(error.to_string()),
        };
        match self.unstable.world_entities(Request::WorldEntities {
            id: Some(id),
            player: None,
            radius: None,
            entity_type: None,
            limit: Some(1),
        }) {
            Ok(snapshot) => NativeRead::Read {
                tick: snapshot.tick,
                entity: snapshot.entities.first().map(|entity| Vitals {
                    kind: entity.type_id.clone(),
                    health: entity
                        .health
                        .map(|[current, max]| [f64::from(current), f64::from(max)]),
                }),
            },
            Err(error) => {
                NativeRead::Unavailable(format!("the native entity read failed: {error}"))
            }
        }
    }

    /// Add an event derived from native reads to the merged event stream.
    pub(crate) fn push_native_event(&mut self, name: &str, tick: u64, body: Value) {
        self.events.push_native(name, tick, body);
    }
}

/// The session while a background run uses it: waits answer other calls.
struct Pumped<'a> {
    session: &'a mut Session,
    pump: &'a mut dyn Pump,
}

impl Game for Pumped<'_> {
    fn world(&mut self) -> Result<World> {
        Game::world(self.session)
    }

    fn cursor(&mut self) -> Result<EventCursor> {
        Game::cursor(self.session)
    }

    fn wait_event(
        &mut self,
        after: &EventCursor,
        timeout: Duration,
        matches: &dyn Fn(&HubEvent) -> bool,
    ) -> Result<(Option<HubEvent>, Vec<String>)> {
        Game::wait_event(self.session, after, timeout, matches)
    }

    fn input(&mut self, request: InputRequest) -> Result<()> {
        self.session.check_lock();
        Game::input(self.session, request)
    }

    fn pause(&mut self, duration: Duration) {
        // Long waits come in slices, so an F9 take-over stops the run within a slice.
        let mut left = duration;
        loop {
            self.session.check_lock();
            let slice = left.min(LOCK_SLICE);
            self.session.background.nested = true;
            self.pump.pump(self.session, slice);
            self.session.background.nested = false;
            left -= slice;
            if left.is_zero() {
                return;
            }
        }
    }

    fn abandoned(&mut self) -> bool {
        self.session.client_gone()
    }

    fn record_start(&mut self, args: Value) -> Result<Value> {
        Game::record_start(self.session, args)
    }

    fn record_stop(&mut self) -> Result<Value> {
        Game::record_stop(self.session)
    }

    fn mark(&mut self, title: &str) {
        Game::mark(self.session, title);
    }

    fn world_key(&mut self) -> Option<String> {
        Game::world_key(self.session)
    }

    fn warm_key(&mut self) -> Option<String> {
        Game::warm_key(self.session)
    }

    fn set_warm_key(&mut self, key: Option<String>) {
        Game::set_warm_key(self.session, key);
    }

    fn attachable(&mut self) -> Result<Option<Value>> {
        Game::attachable(self.session)
    }

    fn native_entity(&mut self, id: &str) -> NativeRead {
        Game::native_entity(self.session, id)
    }

    fn push_native(&mut self, name: &str, tick: u64, body: Value) {
        Game::push_native(self.session, name, tick, body);
    }

    fn native_player(&mut self) -> Result<protocol::unstable::NativePlayer> {
        Game::native_player(self.session)
    }

    fn native_list(&mut self, radius: f64, entity_type: Option<&str>) -> Result<Vec<Value>> {
        Game::native_list(self.session, radius, entity_type)
    }
}
