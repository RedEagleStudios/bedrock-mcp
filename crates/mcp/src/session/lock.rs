//! The input lock. While the server plays, the agent keeps the player's real keyboard and
//! mouse out of the game, the action bar says so, and F9 gives the player the game back.
//!
//! A call locks when it first sends input, and a sequence that plays the connected player
//! locks for its whole run; the lock ends when the call or the run ends, failed or not. The
//! agent also unlocks by itself when its connection goes away and when the player presses F9.
//! The session sees an F9 take-over in the agent's status (a running sequence asks every
//! [`POLL_EVERY`]), stops the sequence as sequence_stop does, and locks again only once the
//! next tool call starts. The lock and its banner never fail an action: what went wrong is
//! a note in game_status.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::Session;
use crate::{
    agent::{Agent, AgentConnection},
    error::{Error, Result},
    input::Controller,
    world::World,
};

/// `BEDROCK_MCP_INPUT_LOCK=0` turns the input lock off; it is on otherwise.
pub const SETTING: &str = "BEDROCK_MCP_INPUT_LOCK";
const BANNER: &str = "bedrock-mcp is playing - your input is locked - F9 to take over";
/// The action bar fades after a few seconds, so a locked run shows the banner again this often.
const BANNER_EVERY: Duration = Duration::from_secs(2);
const BANNER_TIMEOUT: Duration = Duration::from_secs(1);
/// How often a running sequence asks the agent whether the player took over.
const POLL_EVERY: Duration = Duration::from_millis(250);
const NO_BANNER: &str = "No banner shows that input is locked: Minecraft's WebSocket is not \
                         connected (it needs a world with cheats on). F9 still takes over.";
/// Why input fails for the rest of a call after the player pressed F9.
const TAKEN_OVER: &str = "The player pressed F9 to take over, so the server sent no more input in \
                          this call. Ask the user before you play on; the next tool call locks \
                          the input again.";

pub struct InputLock {
    enabled: bool,
    /// The agent was told to lock, on the current connection, and not told otherwise.
    engaged: bool,
    /// The agent's take-over count when the lock engaged.
    takeovers: u64,
    /// The player took over; no lock until the next tool call starts.
    suspended: bool,
    /// The player took over since the last lock started.
    taken_over: bool,
    polled: Option<Instant>,
    banner_shown: Option<Instant>,
    /// What went wrong with the lock or the banner most recently.
    note: Option<String>,
}

impl InputLock {
    /// The lock as [`SETTING`] sets it.
    pub fn from_env() -> Self {
        Self::new(std::env::var(SETTING).map_or(true, |value| value.trim() != "0"))
    }

    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            engaged: false,
            takeovers: 0,
            suspended: false,
            taken_over: false,
            polled: None,
            banner_shown: None,
            note: None,
        }
    }

    /// A tool call starts: a take-over no longer keeps the lock off.
    pub fn call_started(&mut self) {
        self.suspended = false;
    }

    /// The agent connection was replaced; a new connection starts unlocked.
    pub fn forget(&mut self) {
        self.engaged = false;
    }

    fn wanted(&self) -> bool {
        self.enabled && !self.suspended && !self.engaged
    }

    /// Lock, unless the setting is off, the player took over, or it is locked already. Fails
    /// only when the connection is lost; a refusal becomes a note.
    fn engage(&mut self, agent: &mut dyn Agent, world: Option<&World>, now: Instant) -> Result<()> {
        if !self.wanted() {
            return Ok(());
        }
        match agent.lock_input(true) {
            Ok(status) => {
                self.engaged = true;
                self.takeovers = status.takeovers;
                self.taken_over = false;
                self.note = None;
                self.polled = Some(now);
                self.banner_shown = None;
                self.show_banner(world, now);
                Ok(())
            }
            Err(error @ Error::Disconnected(_)) => Err(error),
            Err(error) => {
                self.note = Some(format!("The input lock is off: {error}"));
                Ok(())
            }
        }
    }

    /// While locked: whether the player took over since the last check, at most every
    /// [`POLL_EVERY`]. Keeps the banner up, and locks again after the agent lost the lock
    /// without a take-over (a new connection).
    fn check(&mut self, agent: &mut dyn Agent, world: Option<&World>, now: Instant) -> bool {
        if !self.engaged || self.polled.is_some_and(|at| now - at < POLL_EVERY) {
            return false;
        }
        self.polled = Some(now);
        // A lost connection fails the run's next input, which reports it.
        let Ok(status) = agent.status() else {
            return false;
        };
        if status.takeovers > self.takeovers {
            self.engaged = false;
            self.suspended = true;
            self.taken_over = true;
            self.clear_banner(world);
            return true;
        }
        if !status.input_locked {
            match agent.lock_input(true) {
                Ok(status) => self.takeovers = status.takeovers,
                Err(error) => {
                    self.note = Some(format!("The input lock could not lock again: {error}"))
                }
            }
        }
        if self.banner_shown.is_some_and(|at| now - at >= BANNER_EVERY) {
            self.show_banner(world, now);
        }
        false
    }

    /// Unlock; whether the player took over since the last check.
    fn release(&mut self, agent: Option<&mut dyn Agent>, world: Option<&World>) -> bool {
        if !std::mem::take(&mut self.engaged) {
            return false;
        }
        // Without a connection the agent has unlocked by itself.
        let taken = agent
            .and_then(|agent| agent.lock_input(false).ok())
            .is_some_and(|status| status.takeovers > self.takeovers);
        self.taken_over |= taken;
        self.clear_banner(world);
        taken
    }

    fn show_banner(&mut self, world: Option<&World>, now: Instant) {
        let Some(world) = world.filter(|world| world.connected()) else {
            self.note = Some(NO_BANNER.to_owned());
            return;
        };
        match command(world, &action_bar(BANNER)) {
            Ok(()) => self.banner_shown = Some(now),
            Err(error) => self.note = Some(format!("The input lock banner failed: {error}")),
        }
    }

    fn clear_banner(&mut self, world: Option<&World>) {
        // The game ignores an empty action bar and `title clear` leaves it up, so a reset
        // code replaces the banner with nothing visible.
        if self.banner_shown.take().is_some()
            && let Some(world) = world
            && let Err(error) = command(world, &action_bar("§r"))
        {
            self.note = Some(format!("The input lock banner did not clear: {error}"));
        }
    }

    /// game_status's view of the lock.
    pub fn report(&self) -> Value {
        json!({
            "enabled": self.enabled,
            "locked": self.engaged,
            "taken_over": self.taken_over,
            "banner": self.banner_shown.is_some(),
            "note": if self.enabled {
                self.note.clone()
            } else {
                Some(format!("{SETTING}=0 turns the input lock off."))
            },
        })
    }
}

fn action_bar(text: &str) -> String {
    format!(
        "titleraw @s actionbar {}",
        json!({ "rawtext": [{ "text": text }] })
    )
}

/// Run a banner command with a short timeout; a nonzero status is an error.
fn command(world: &World, command: &str) -> Result<()> {
    let body = world.socket().command(command, BANNER_TIMEOUT)?;
    match body["statusCode"].as_i64().unwrap_or(0) {
        0 => Ok(()),
        _ => Err(Error::msg(
            body["statusMessage"]
                .as_str()
                .unwrap_or("failed")
                .to_owned(),
        )),
    }
}

impl Session {
    /// Run `action`, which sends input, with the agent under the input lock. Once the player
    /// pressed F9 during this call, the call's remaining input fails, so a foreground action
    /// such as a fight stops instead of playing on under the player.
    pub(super) fn with_input<T>(
        &mut self,
        action: impl FnMut(&mut AgentConnection, &mut Controller) -> Result<T>,
    ) -> Result<T> {
        self.check_lock();
        if self.lock.suspended && self.lock.taken_over {
            return Err(Error::msg(TAKEN_OVER));
        }
        self.lock_input();
        self.with_agent(action)
    }

    /// Lock the player's input out, attaching to the agent first when needed. A failure is
    /// left to the action, which attaches again and reports it.
    pub(super) fn lock_input(&mut self) {
        if !self.lock.wanted() {
            return;
        }
        let world = self.world().ok();
        // A connection found broken is replaced once.
        for _ in 0..2 {
            if self.with_agent(|_, _| Ok(())).is_err() {
                return;
            }
            let agent = self.agent.as_mut().expect("attached above");
            match self.lock.engage(agent, world.as_ref(), self.clock.now()) {
                Err(Error::Disconnected(_)) => {
                    self.agent = None;
                    self.controller.forget();
                }
                _ => return,
            }
        }
    }

    /// At a running sequence's wait point: stop it if the player took over.
    pub(super) fn check_lock(&mut self) {
        let world = self.world().ok();
        let Some(agent) = self.agent.as_mut() else {
            return;
        };
        if self.lock.check(agent, world.as_ref(), self.clock.now())
            && let Some(id) = self.background.driving_self()
        {
            self.background.take_over(&id);
        }
    }

    /// End the lock; whether the player took over since the last check.
    pub(super) fn unlock_input(&mut self) -> bool {
        let world = self.world().ok();
        let agent = self.agent.as_mut().map(|agent| agent as &mut dyn Agent);
        self.lock.release(agent, world.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::{
        agent::fake::{FakeAgent, shared_endpoint},
        clock::SystemClock,
        events::fake::FakeGame,
        install::Install,
        rpc::{Content, Pump, ToolServer},
        testdir::{TestDir, roaming},
        unstable::fake::{FakeHost, Script},
    };

    /// A session whose agent answers as `agent`, in a world on a fake WebSocket.
    struct Fixture {
        session: Session,
        agent: Arc<Mutex<FakeAgent>>,
        game: FakeGame,
        _dir: TestDir,
    }

    fn fixture() -> Fixture {
        let dir = TestDir::new("lock");
        let prefix = dir.path().join("pfx");
        let options =
            roaming(&prefix).join("Minecraft Bedrock/Users/1/games/com.mojang/minecraftpe");
        std::fs::create_dir_all(&options).unwrap();
        std::fs::write(
            options.join("options.txt"),
            "keyboard_type_0_key.forward:87\nkeyboard_type_0_key.back:83\n\
             keyboard_type_0_key.left:65\nkeyboard_type_0_key.right:68\n\
             keyboard_type_0_key.jump:32\nkeyboard_type_0_key.sneak:16\n\
             keyboard_type_0_key.sprint:17\n",
        )
        .unwrap();
        let install = Install {
            profile: dir.path().join("profile"),
            prefix,
        };
        let host = Box::new(FakeHost(Arc::new(Mutex::new(Script::working()))));
        let mut session = Session::with_unstable_host(install, Arc::new(SystemClock), host);
        let agent = Arc::new(Mutex::new(FakeAgent::new()));
        session.agent = Some(AgentConnection::open(&shared_endpoint(Arc::clone(&agent))).unwrap());
        let game = FakeGame::start(false);
        session.world = Ok(game.world.clone());
        Fixture {
            session,
            agent,
            game,
            _dir: dir,
        }
    }

    fn call(session: &mut Session, name: &str, arguments: Value) -> Value {
        let content = session.call(name, arguments).unwrap();
        let [Content::Text(text)] = content.as_slice() else {
            panic!("one text item")
        };
        serde_json::from_str(text).unwrap()
    }

    /// Lets wait points pass as the MCP server would; the player presses F9 at `press_at`.
    struct Player {
        agent: Arc<Mutex<FakeAgent>>,
        press_at: Option<Instant>,
    }

    impl Pump for Player {
        fn pump(&mut self, _server: &mut dyn ToolServer, wait: Duration) {
            std::thread::sleep(wait);
            if self.press_at.is_some_and(|at| Instant::now() >= at) {
                self.agent.lock().unwrap().take_over();
            }
        }
    }

    /// Queue a background walk of `ms` and run it; the run's verbose status.
    fn walk(fixture: &mut Fixture, ms: u64, press_after: Option<Duration>) -> Value {
        let queued = call(
            &mut fixture.session,
            "action_sequence",
            json!({"background": true, "warm_up": false, "steps": [
                {"action": "movement_set", "args": {"forward": 1, "duration_ms": ms}}
            ]}),
        );
        let mut player = Player {
            agent: Arc::clone(&fixture.agent),
            press_at: press_after.map(|after| Instant::now() + after),
        };
        fixture.session.work(&mut player);
        call(
            &mut fixture.session,
            "sequence_status",
            json!({"run_id": queued["run_id"], "verbose": true}),
        )
    }

    fn banners(game: &FakeGame) -> Vec<String> {
        game.commands
            .lock()
            .unwrap()
            .iter()
            .filter(|command| command.starts_with("titleraw"))
            .cloned()
            .collect()
    }

    #[test]
    fn a_sequence_locks_for_its_whole_run_with_a_banner() {
        let mut fixture = fixture();
        let result = walk(&mut fixture, 300, None);
        assert_eq!(result["state"], "finished", "{result}");
        assert!(result.get("taken_over").is_none(), "{result}");
        assert_eq!(fixture.agent.lock().unwrap().locks(), [true, false]);
        let banners = banners(&fixture.game);
        assert!(banners[0].contains(BANNER), "{banners:?}");
        assert_eq!(*banners.last().unwrap(), action_bar("§r"));
        let status = call(&mut fixture.session, "game_status", json!({}));
        assert_eq!(status["input_lock"]["locked"], false, "{status}");
    }

    #[test]
    fn f9_stops_the_sequence_and_the_lock_waits_for_the_next_call() {
        let mut fixture = fixture();
        let started = Instant::now();
        let result = walk(&mut fixture, 5_000, Some(Duration::from_millis(300)));
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(result["state"], "stopped", "{result}");
        assert_eq!(result["stopped"]["by"], "take_over", "{result}");
        assert_eq!(result["taken_over"], true);
        assert!(result["notes"].to_string().contains("F9"), "{result}");
        // The stop action released the walk without locking again.
        assert_eq!(fixture.agent.lock().unwrap().locks(), [true]);
        let status = call(&mut fixture.session, "game_status", json!({}));
        assert_eq!(status["input_lock"]["taken_over"], true, "{status}");
        // The next call that sends input locks again, and unlocks when it ends.
        call(
            &mut fixture.session,
            "input_send",
            json!({"press": ["key.jump"]}),
        );
        assert_eq!(fixture.agent.lock().unwrap().locks(), [true, true, false]);
    }

    #[test]
    fn f9_during_a_foreground_call_stops_its_remaining_input() {
        let mut fixture = fixture();
        let session = &mut fixture.session;
        session.lock.call_started();
        session.with_input(|_, _| Ok(())).unwrap();
        fixture.agent.lock().unwrap().take_over();
        std::thread::sleep(POLL_EVERY + Duration::from_millis(50));
        let refused = session.with_input(|_, _| Ok(())).unwrap_err();
        assert!(refused.to_string().contains("F9"), "{refused}");
        // The same call stays refused; the next call locks and sends again.
        assert!(session.with_input(|_, _| Ok(())).is_err());
        session.lock.call_started();
        session.with_input(|_, _| Ok(())).unwrap();
        assert_eq!(fixture.agent.lock().unwrap().locks(), [true, true]);
    }

    #[test]
    fn closing_the_session_unlocks() {
        let mut fixture = fixture();
        fixture.session.lock_input();
        assert!(fixture.agent.lock().unwrap().locked);
        fixture.session.close();
        assert!(!fixture.agent.lock().unwrap().locked);
    }

    fn locked(agent: &mut FakeAgent) -> (InputLock, Instant) {
        let mut lock = InputLock::new(true);
        let now = Instant::now();
        lock.engage(agent, None, now).unwrap();
        (lock, now)
    }

    #[test]
    fn a_lock_ends_with_its_call_and_says_why_no_banner_shows() {
        let mut agent = FakeAgent::new();
        let (mut lock, now) = locked(&mut agent);
        assert!(agent.locked);
        // A second input in the same call does not lock again.
        lock.engage(&mut agent, None, now).unwrap();
        assert_eq!(agent.locks(), [true]);
        assert_eq!(lock.report()["note"], NO_BANNER);
        assert!(!lock.release(Some(&mut agent), None));
        assert!(!agent.locked);
        assert_eq!(agent.locks(), [true, false]);
        assert_eq!(lock.report()["locked"], false);
    }

    #[test]
    fn the_setting_turns_the_lock_off() {
        let mut agent = FakeAgent::new();
        let mut lock = InputLock::new(false);
        lock.engage(&mut agent, None, Instant::now()).unwrap();
        assert!(!lock.release(Some(&mut agent), None));
        assert!(agent.requests.is_empty());
        assert!(lock.report()["note"].as_str().unwrap().contains(SETTING));
    }

    #[test]
    fn a_take_over_ends_the_lock_until_the_next_call() {
        let mut agent = FakeAgent::new();
        let (mut lock, now) = locked(&mut agent);
        // Checks are spaced out.
        assert!(!lock.check(&mut agent, None, now));
        agent.take_over();
        assert!(lock.check(&mut agent, None, now + POLL_EVERY));
        assert_eq!(lock.report()["taken_over"], true);
        // Input sent while the run winds down does not lock again, and nothing is left to
        // unlock.
        lock.engage(&mut agent, None, now).unwrap();
        assert!(!lock.release(Some(&mut agent), None));
        assert_eq!(agent.locks(), [true]);
        lock.call_started();
        lock.engage(&mut agent, None, now).unwrap();
        assert!(agent.locked);
        assert_eq!(lock.report()["taken_over"], false);
    }

    #[test]
    fn a_take_over_seen_only_when_unlocking_is_reported() {
        let mut agent = FakeAgent::new();
        let (mut lock, _) = locked(&mut agent);
        agent.take_over();
        assert!(lock.release(Some(&mut agent), None));
        assert_eq!(lock.report()["taken_over"], true);
    }

    #[test]
    fn a_new_connection_is_locked_again() {
        let mut agent = FakeAgent::new();
        let (mut lock, now) = locked(&mut agent);
        // The agent dropped the lock with the old connection; no F9 was pressed.
        agent.locked = false;
        assert!(!lock.check(&mut agent, None, now + POLL_EVERY));
        assert!(agent.locked);
    }

    #[test]
    fn without_a_connection_the_agent_has_unlocked_by_itself() {
        let mut agent = FakeAgent::new();
        let (mut lock, _) = locked(&mut agent);
        assert!(!lock.release(None, None));
        assert_eq!(lock.report()["locked"], false);
    }

    #[test]
    fn a_refused_lock_is_a_note_not_a_failure() {
        let mut agent = FakeAgent::new();
        agent.replies.push_back(protocol::Reply::Error {
            message: "unknown command".into(),
        });
        let mut lock = InputLock::new(true);
        lock.engage(&mut agent, None, Instant::now()).unwrap();
        let report = lock.report();
        assert_eq!(report["locked"], false);
        assert!(report["note"].as_str().unwrap().contains("unknown command"));
    }
}
