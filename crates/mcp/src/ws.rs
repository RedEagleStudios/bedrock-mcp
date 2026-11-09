//! Bedrock's WebSocket automation protocol, server side. The game connects out to us when
//! the player runs `/connect 127.0.0.1:<port>`; we then run commands as that player and
//! receive the events we subscribed to.

use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    hash::{BuildHasher, Hasher, RandomState},
    io::{ErrorKind, Read},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use polling::{Event as PollEvent, Events, Poller};
use serde_json::{Value, json};
use tungstenite::{Message, WebSocket};

use crate::error::{Result, bail};

/// Command syntax version (`body.version`): 1 is the legacy syntax, and newer values select newer
/// syntax, such as the current `execute` (zh.minecraft.wiki/w/Tutorial:WebSocket). With 100 the
/// public 1.26.62 build runs the current `execute` syntax.
pub const COMMAND_VERSION: u32 = 100;
/// Bedrock drops command requests beyond about 100 outstanding ones.
pub const MAX_IN_FLIGHT: usize = 64;
const EVENT_HISTORY: usize = 512;
/// Subscribed on every connection and never unsubscribed: chat confirmation relies on it.
pub const ALWAYS_SUBSCRIBED: &str = "PlayerMessage";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// The poller key of a connection's socket; the poller watches nothing else.
const SOCKET_KEY: usize = 0;

pub fn command_request(command: &str, request_id: &str) -> Value {
    json!({
        "header": {
            "version": 1,
            "requestId": request_id,
            "messageType": "commandRequest",
            "messagePurpose": "commandRequest",
        },
        "body": {
            "version": COMMAND_VERSION,
            "commandLine": command.trim_start_matches('/'),
            "origin": { "type": "player" },
        },
    })
}

pub fn subscribe_request(event: &str) -> Value {
    subscription_request("subscribe", event)
}

pub fn unsubscribe_request(event: &str) -> Value {
    subscription_request("unsubscribe", event)
}

fn subscription_request(purpose: &str, event: &str) -> Value {
    json!({
        "header": {
            "version": 1,
            "requestId": uuid(),
            "messageType": "commandRequest",
            "messagePurpose": purpose,
        },
        "body": { "eventName": event },
    })
}

/// A random version-4 UUID.
pub fn uuid() -> String {
    let mut bytes = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_err()
    {
        // Each RandomState is seeded from the system's randomness with distinct keys.
        let random = || RandomState::new().build_hasher().finish();
        bytes = (u128::from(random()) << 64 | u128::from(random())).to_le_bytes();
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

#[derive(Clone, Debug)]
pub struct Event {
    pub seq: u64,
    pub name: String,
    pub body: Value,
    /// When the server received it, in Unix milliseconds.
    pub received_at: u64,
}

/// Counters and subscriptions for status reports.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    /// Every subscribed event name, [`ALWAYS_SUBSCRIBED`] included, sorted.
    pub subscriptions: Vec<String>,
    /// Event names received so far, sorted.
    pub observed: Vec<String>,
    pub events_received: u64,
    /// Events pushed out of the history ring by newer ones.
    pub events_dropped: u64,
    /// Commands whose connection closed before the game answered.
    pub commands_lost: u64,
}

enum Pending {
    /// Sent on the link with this id.
    Waiting(u64),
    Done(Value),
    Lost,
}

/// The live game connection's outgoing side.
struct Link {
    id: u64,
    outgoing: mpsc::Sender<String>,
    /// Wakes the connection's I/O thread out of its wait.
    waker: Arc<Poller>,
    closed: Arc<AtomicBool>,
}

impl Link {
    fn wake(&self) {
        let _ = self.waker.notify();
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        self.wake();
    }
}

#[derive(Default)]
struct State {
    link: Option<Link>,
    next_id: u64,
    pending: HashMap<String, Pending>,
    events: VecDeque<Event>,
    event_count: u64,
    events_dropped: u64,
    commands_lost: u64,
    connections: u64,
    /// Subscribed besides [`ALWAYS_SUBSCRIBED`]; sent again on every new connection.
    subscriptions: BTreeSet<String>,
    observed: BTreeSet<String>,
}

impl State {
    /// Queue subscription messages on the live connection, if any.
    fn send_subscriptions<'a>(
        &self,
        purpose: fn(&str) -> Value,
        names: impl IntoIterator<Item = &'a str>,
    ) {
        let Some(link) = &self.link else { return };
        for name in names {
            let _ = link.outgoing.send(purpose(name).to_string());
        }
        link.wake();
    }
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn wait_until<'a>(
        &self,
        guard: MutexGuard<'a, State>,
        deadline: Instant,
    ) -> (MutexGuard<'a, State>, bool) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return (guard, false);
        }
        let (guard, _) = self
            .changed
            .wait_timeout(guard, remaining)
            .unwrap_or_else(|poison| poison.into_inner());
        (guard, true)
    }
}

/// Listens on loopback for the game; one game connection at a time, newest wins.
pub struct GameSocket {
    port: u16,
    shared: Arc<Shared>,
}

impl GameSocket {
    pub fn bind() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let shared = Arc::new(Shared::default());
        let accept_shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("bedrock-ws".into())
            .spawn(move || accept_loop(&listener, &accept_shared))?;
        Ok(Self { port, shared })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn connected(&self) -> bool {
        self.shared.lock().link.is_some()
    }

    /// Connections accepted so far.
    pub fn connections(&self) -> u64 {
        self.shared.lock().connections
    }

    /// Commands sent and not yet answered.
    pub fn in_flight(&self) -> usize {
        self.shared
            .lock()
            .pending
            .values()
            .filter(|pending| matches!(pending, Pending::Waiting(_)))
            .count()
    }

    pub fn wait_connected(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.shared.lock();
        while state.link.is_none() {
            let (guard, waited) = self.shared.wait_until(state, deadline);
            state = guard;
            if !waited {
                return false;
            }
        }
        true
    }

    /// Run `command` as the player and return the response body.
    pub fn command(&self, command: &str, timeout: Duration) -> Result<Value> {
        let deadline = Instant::now() + timeout;
        let request_id = uuid();
        let text = command_request(command, &request_id).to_string();
        let shown = command.trim_start_matches('/');
        let mut state = self.shared.lock();
        while state.pending.len() >= MAX_IN_FLIGHT {
            let (guard, waited) = self.shared.wait_until(state, deadline);
            state = guard;
            if !waited {
                bail!("/{shown}: too many commands in flight");
            }
        }
        let Some(link) = &state.link else {
            bail!("Minecraft's WebSocket is not connected.");
        };
        let link_id = link.id;
        if link.outgoing.send(text).is_err() {
            bail!("/{shown}: the game disconnected");
        }
        link.wake();
        state
            .pending
            .insert(request_id.clone(), Pending::Waiting(link_id));
        loop {
            match state.pending.get(&request_id) {
                Some(Pending::Waiting(_)) => {}
                Some(Pending::Done(_)) => {
                    let Some(Pending::Done(body)) = state.pending.remove(&request_id) else {
                        unreachable!()
                    };
                    self.shared.changed.notify_all();
                    return Ok(body);
                }
                Some(Pending::Lost) | None => {
                    state.pending.remove(&request_id);
                    self.shared.changed.notify_all();
                    bail!("/{shown}: the game disconnected");
                }
            }
            let (guard, waited) = self.shared.wait_until(state, deadline);
            state = guard;
            if !waited {
                state.pending.remove(&request_id);
                self.shared.changed.notify_all();
                bail!("/{shown}: no answer from the game");
            }
        }
    }

    /// Sequence number the next event will get; also the number of events received.
    pub fn event_count(&self) -> u64 {
        self.shared.lock().event_count
    }

    /// Events numbered `after` or later still in the history, at most `limit`, and how many
    /// such events the history already dropped.
    pub fn events_after(&self, after: u64, limit: usize) -> (Vec<Event>, u64) {
        let state = self.shared.lock();
        let oldest = state
            .events
            .front()
            .map_or(state.event_count, |event| event.seq);
        let events = state
            .events
            .iter()
            .filter(|event| event.seq >= after)
            .take(limit)
            .cloned()
            .collect();
        (events, oldest.saturating_sub(after))
    }

    /// Subscribe to legacy events by name, now and on every later connection. Returns the
    /// names that were not subscribed before.
    pub fn subscribe(&self, names: &[String]) -> Vec<String> {
        let mut state = self.shared.lock();
        let added: Vec<String> = names
            .iter()
            .filter(|name| name.as_str() != ALWAYS_SUBSCRIBED)
            .filter(|name| state.subscriptions.insert((*name).clone()))
            .cloned()
            .collect();
        state.send_subscriptions(subscribe_request, added.iter().map(String::as_str));
        added
    }

    /// Stop receiving events by name; their events are ignored from now on even if the game
    /// still sends them. Returns the names that were subscribed.
    /// [`ALWAYS_SUBSCRIBED`] cannot be unsubscribed.
    pub fn unsubscribe(&self, names: &[String]) -> Result<Vec<String>> {
        if names.iter().any(|name| name == ALWAYS_SUBSCRIBED) {
            bail!("{ALWAYS_SUBSCRIBED} stays subscribed: chat confirmation relies on it.");
        }
        let mut state = self.shared.lock();
        let removed: Vec<String> = names
            .iter()
            .filter(|name| state.subscriptions.remove(name.as_str()))
            .cloned()
            .collect();
        state.send_subscriptions(unsubscribe_request, removed.iter().map(String::as_str));
        Ok(removed)
    }

    pub fn stats(&self) -> Stats {
        let state = self.shared.lock();
        let mut subscriptions: Vec<String> = state.subscriptions.iter().cloned().collect();
        subscriptions.push(ALWAYS_SUBSCRIBED.to_owned());
        subscriptions.sort();
        Stats {
            subscriptions,
            observed: state.observed.iter().cloned().collect(),
            events_received: state.event_count,
            events_dropped: state.events_dropped,
            commands_lost: state.commands_lost,
        }
    }

    /// The first event numbered `after` or later that matches, waiting up to `timeout`.
    pub fn wait_event(
        &self,
        after: u64,
        timeout: Duration,
        matches: impl Fn(&Event) -> bool,
    ) -> Option<Event> {
        let deadline = Instant::now() + timeout;
        let mut state = self.shared.lock();
        loop {
            if let Some(event) = state
                .events
                .iter()
                .find(|event| event.seq >= after && matches(event))
            {
                return Some(event.clone());
            }
            let (guard, waited) = self.shared.wait_until(state, deadline);
            state = guard;
            if !waited {
                return None;
            }
        }
    }
}

fn accept_loop(listener: &TcpListener, shared: &Arc<Shared>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let Some(socket) = handshake(stream) else {
            continue;
        };
        let Ok(poller) = Poller::new().map(Arc::new) else {
            continue;
        };
        let (outgoing, incoming) = mpsc::channel();
        let closed = Arc::new(AtomicBool::new(false));
        let id = {
            let mut state = shared.lock();
            let subscriptions = std::iter::once(ALWAYS_SUBSCRIBED)
                .chain(state.subscriptions.iter().map(String::as_str));
            for event in subscriptions {
                let _ = outgoing.send(subscribe_request(event).to_string());
            }
            state.next_id += 1;
            state.connections += 1;
            let id = state.next_id;
            // Replacing the link drops the old one, which closes its connection.
            state.link = Some(Link {
                id,
                outgoing,
                waker: Arc::clone(&poller),
                closed: Arc::clone(&closed),
            });
            shared.changed.notify_all();
            id
        };
        let spawned = thread::Builder::new().name("bedrock-ws-io".into()).spawn({
            let shared = Arc::clone(shared);
            move || {
                serve_connection(socket, &poller, &incoming, &closed, &shared);
                disconnect(&shared, id);
            }
        });
        if spawned.is_err() {
            disconnect(shared, id);
        }
    }
}

fn handshake(stream: TcpStream) -> Option<WebSocket<TcpStream>> {
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT)).ok()?;
    stream.set_nodelay(true).ok()?;
    let socket = tungstenite::accept(stream).ok()?;
    socket.get_ref().set_read_timeout(None).ok()?;
    socket.get_ref().set_nonblocking(true).ok()?;
    Some(socket)
}

fn would_block(error: &tungstenite::Error) -> bool {
    matches!(error, tungstenite::Error::Io(error) if error.kind() == ErrorKind::WouldBlock)
}

/// Pump one non-blocking connection: queue outgoing messages, read what arrived, flush,
/// then sleep in the poller until the socket is ready or the link wakes it.
fn serve_connection(
    mut socket: WebSocket<TcpStream>,
    poller: &Poller,
    incoming: &mpsc::Receiver<String>,
    closed: &AtomicBool,
    shared: &Shared,
) {
    // SAFETY: the socket is removed from the poller below, before it is closed.
    if unsafe { poller.add(socket.get_ref(), PollEvent::none(SOCKET_KEY)) }.is_err() {
        return;
    }
    pump(&mut socket, poller, incoming, closed, shared);
    let _ = poller.delete(socket.get_ref());
}

/// [`serve_connection`]'s loop, until the connection ends.
fn pump(
    socket: &mut WebSocket<TcpStream>,
    poller: &Poller,
    incoming: &mpsc::Receiver<String>,
    closed: &AtomicBool,
    shared: &Shared,
) {
    let mut ready = Events::new();
    loop {
        if closed.load(Ordering::SeqCst) {
            let _ = socket.close(None);
            let _ = socket.flush();
            return;
        }
        loop {
            match incoming.try_recv() {
                Ok(text) => match socket.write(Message::text(text)) {
                    Ok(()) => {}
                    Err(error) if would_block(&error) => {}
                    Err(_) => return,
                },
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }
        loop {
            match socket.read() {
                Ok(Message::Text(text)) => dispatch(shared, text.as_str()),
                Ok(_) => {}
                Err(error) if would_block(&error) => break,
                Err(_) => return,
            }
        }
        let want_write = match socket.flush() {
            Ok(()) => false,
            Err(error) if would_block(&error) => true,
            Err(_) => return,
        };
        let interest = if want_write {
            PollEvent::all(SOCKET_KEY)
        } else {
            PollEvent::readable(SOCKET_KEY)
        };
        // Interest is one-shot: re-arm it before every wait.
        if poller.modify(socket.get_ref(), interest).is_err() {
            return;
        }
        ready.clear();
        let _ = poller.wait(&mut ready, Some(Duration::from_secs(1)));
    }
}

fn dispatch(shared: &Shared, text: &str) {
    let Ok(message) = serde_json::from_str::<Value>(text) else {
        return;
    };
    let header = &message["header"];
    let body = message.get("body").cloned().unwrap_or(Value::Null);
    let mut state = shared.lock();
    match header["messagePurpose"].as_str() {
        Some("event") => {
            // Current versions name the event in the header, old ones in the body.
            let name = header["eventName"]
                .as_str()
                .or_else(|| body["eventName"].as_str())
                .unwrap_or_default()
                .to_owned();
            // Events still in flight after an unsubscribe are ignored.
            if name != ALWAYS_SUBSCRIBED && !state.subscriptions.contains(&name) {
                return;
            }
            let seq = state.event_count;
            state.event_count += 1;
            if !state.observed.contains(&name) {
                state.observed.insert(name.clone());
            }
            let received_at = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |since| since.as_millis() as u64);
            state.events.push_back(Event {
                seq,
                name,
                body,
                received_at,
            });
            if state.events.len() > EVENT_HISTORY {
                state.events.pop_front();
                state.events_dropped += 1;
            }
        }
        Some("commandResponse" | "error") => {
            let Some(id) = header["requestId"].as_str() else {
                return;
            };
            if let Some(pending) = state.pending.get_mut(id) {
                *pending = Pending::Done(body);
            }
        }
        _ => return,
    }
    shared.changed.notify_all();
}

fn disconnect(shared: &Shared, id: u64) {
    let mut state = shared.lock();
    if state.link.as_ref().is_some_and(|link| link.id == id) {
        state.link = None;
    }
    let mut lost = 0;
    for pending in state.pending.values_mut() {
        if matches!(pending, Pending::Waiting(link) if *link == id) {
            *pending = Pending::Lost;
            lost += 1;
        }
    }
    state.commands_lost += lost;
    shared.changed.notify_all();
}

#[cfg(test)]
mod tests {
    use tungstenite::stream::MaybeTlsStream;

    use super::*;

    type Client = WebSocket<MaybeTlsStream<TcpStream>>;

    fn connect(server: &GameSocket) -> Client {
        let (client, response) =
            tungstenite::connect(format!("ws://127.0.0.1:{}", server.port())).unwrap();
        assert_eq!(response.status(), 101);
        client
    }

    fn next_json(client: &mut Client) -> Value {
        loop {
            if let Message::Text(text) = client.read().unwrap() {
                return serde_json::from_str(text.as_str()).unwrap();
            }
        }
    }

    fn send(client: &mut Client, value: &Value) {
        client.send(Message::text(value.to_string())).unwrap();
    }

    fn answer(client: &mut Client, request: &Value, body: Value) {
        let id = request["header"]["requestId"].clone();
        send(
            client,
            &json!({"header": {"messagePurpose": "commandResponse", "requestId": id}, "body": body}),
        );
    }

    #[test]
    fn handshake_subscribes_and_commands_round_trip() {
        let server = GameSocket::bind().unwrap();
        assert!(!server.connected());
        let mut client = connect(&server);
        let subscribe = next_json(&mut client);
        assert_eq!(subscribe["header"]["messagePurpose"], "subscribe");
        assert_eq!(subscribe["body"]["eventName"], "PlayerMessage");
        assert!(server.wait_connected(Duration::from_secs(5)));

        let game = thread::spawn(move || {
            let request = next_json(&mut client);
            assert_eq!(request["header"]["messagePurpose"], "commandRequest");
            assert_eq!(request["body"]["commandLine"], "time query daytime");
            assert_eq!(request["body"]["version"], COMMAND_VERSION);
            assert_eq!(request["body"]["origin"]["type"], "player");
            // An unrelated response must not answer this request.
            send(
                &mut client,
                &json!({"header": {"messagePurpose": "commandResponse", "requestId": "other"}, "body": {}}),
            );
            send(
                &mut client,
                &json!({
                    "header": {"messagePurpose": "event", "eventName": "PlayerMessage"},
                    "body": {"message": "hi", "sender": "Steve", "type": "chat"},
                }),
            );
            answer(
                &mut client,
                &request,
                json!({"statusCode": 0, "statusMessage": "Daytime is 1000", "data": 1000}),
            );
            client
        });
        let body = server
            .command("/time query daytime", Duration::from_secs(5))
            .unwrap();
        assert_eq!(body["data"], 1000);
        let event = server
            .wait_event(0, Duration::from_secs(5), |event| {
                event.name == "PlayerMessage"
            })
            .unwrap();
        assert_eq!(event.body["message"], "hi");
        assert!(
            server
                .wait_event(event.seq + 1, Duration::ZERO, |_| true)
                .is_none()
        );

        let mut client = game.join().unwrap();
        client.close(None).unwrap();
        while client.read().is_ok() {}
        // Whether or not the server has seen the close yet, the command cannot be answered.
        assert!(server.command("say gone", Duration::from_secs(5)).is_err());
    }

    #[test]
    fn a_new_connection_replaces_the_old_one() {
        let server = GameSocket::bind().unwrap();
        let mut first = connect(&server);
        next_json(&mut first);
        let mut second = connect(&server);
        next_json(&mut second);
        // The server closes the first connection.
        while let Ok(message) = first.read() {
            assert!(!matches!(message, Message::Text(_)));
        }
        assert_eq!(server.connections(), 2);
        let game = thread::spawn(move || {
            let request = next_json(&mut second);
            answer(&mut second, &request, json!({"statusCode": 0}));
            second
        });
        assert_eq!(
            server
                .command("testfor @s", Duration::from_secs(5))
                .unwrap()["statusCode"],
            0
        );
        drop(game.join().unwrap());
    }

    fn event(name: &str, body: Value) -> Value {
        json!({"header": {"messagePurpose": "event", "eventName": name}, "body": body})
    }

    /// The (purpose, event name) of the next `count` subscription messages.
    fn subscriptions(client: &mut Client, count: usize) -> Vec<(String, String)> {
        (0..count)
            .map(|_| {
                let message = next_json(client);
                (
                    message["header"]["messagePurpose"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                    message["body"]["eventName"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }

    fn pair(purpose: &str, name: &str) -> (String, String) {
        (purpose.to_owned(), name.to_owned())
    }

    #[test]
    fn subscriptions_persist_across_connections_and_unsubscribe_stops_delivery() {
        let server = GameSocket::bind().unwrap();
        let names = |list: &[&str]| {
            list.iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(server.subscribe(&names(&["BlockBroken"])), ["BlockBroken"]);
        let mut first = connect(&server);
        assert_eq!(
            subscriptions(&mut first, 2),
            [
                pair("subscribe", "PlayerMessage"),
                pair("subscribe", "BlockBroken")
            ]
        );
        assert!(server.wait_connected(Duration::from_secs(5)));
        // Already subscribed names are not sent again.
        assert_eq!(
            server.subscribe(&names(&["ItemUsed", "BlockBroken", "PlayerMessage"])),
            ["ItemUsed"]
        );
        assert_eq!(
            subscriptions(&mut first, 1),
            [pair("subscribe", "ItemUsed")]
        );

        send(&mut first, &event("BlockBroken", json!({"block": "stone"})));
        let broken = server
            .wait_event(0, Duration::from_secs(5), |event| {
                event.name == "BlockBroken"
            })
            .unwrap();
        assert_eq!(broken.body["block"], "stone");
        assert!(broken.received_at > 0);

        assert!(server.unsubscribe(&names(&["PlayerMessage"])).is_err());
        assert_eq!(
            server
                .unsubscribe(&names(&["BlockBroken", "MobKilled"]))
                .unwrap(),
            ["BlockBroken"]
        );
        assert_eq!(
            subscriptions(&mut first, 1),
            [pair("unsubscribe", "BlockBroken")]
        );
        // A late event the game sent before seeing the unsubscribe is not delivered.
        send(&mut first, &event("BlockBroken", json!({"block": "dirt"})));
        send(
            &mut first,
            &event("PlayerMessage", json!({"message": "marker"})),
        );
        let marker = server
            .wait_event(0, Duration::from_secs(5), |event| {
                event.name == "PlayerMessage"
            })
            .unwrap();
        let (after, dropped) = server.events_after(broken.seq + 1, 10);
        assert_eq!(dropped, 0);
        assert_eq!(
            after.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [marker.seq]
        );

        // The next connection gets the remembered subscriptions again.
        let mut second = connect(&server);
        assert_eq!(
            subscriptions(&mut second, 2),
            [
                pair("subscribe", "PlayerMessage"),
                pair("subscribe", "ItemUsed")
            ]
        );
        let stats = server.stats();
        assert_eq!(stats.subscriptions, ["ItemUsed", "PlayerMessage"]);
        assert_eq!(stats.observed, ["BlockBroken", "PlayerMessage"]);
        assert_eq!(stats.events_received, 2);
    }

    #[test]
    fn counters_report_dropped_events_and_lost_commands() {
        let server = GameSocket::bind().unwrap();
        let mut client = connect(&server);
        next_json(&mut client);
        assert!(server.wait_connected(Duration::from_secs(5)));
        let total = EVENT_HISTORY as u64 + 8;
        for index in 0..total {
            send(
                &mut client,
                &event("PlayerMessage", json!({"message": index})),
            );
        }
        assert!(
            server
                .wait_event(total - 1, Duration::from_secs(5), |_| true)
                .is_some()
        );
        let (events, dropped) = server.events_after(0, 4);
        assert_eq!(dropped, 8);
        assert_eq!(events[0].seq, 8);
        assert_eq!(events.len(), 4);
        assert_eq!(server.stats().events_dropped, 8);

        let game = thread::spawn(move || {
            next_json(&mut client);
            // Hang up without answering.
            drop(client);
        });
        assert!(server.command("say lost", Duration::from_secs(5)).is_err());
        game.join().unwrap();
        assert_eq!(server.stats().commands_lost, 1);
    }

    #[test]
    fn uuids_are_version_4() {
        let id = uuid();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert_ne!(id, uuid());
    }
}
