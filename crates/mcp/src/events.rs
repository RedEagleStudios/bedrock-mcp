//! One event stream over three sources: the game's legacy WebSocket events (source `ws`), the
//! Bridge pack's script events (source `bridge`, polled with `re_mcp:events <since>`), and
//! events this server derives from the unstable layer's native reads (source `native`, pushed
//! by a running action sequence; see `drivers::native`).
//!
//! An [`EventCursor`] marks a position in every source at once; its text form is opaque to
//! clients. Reading after a cursor returns the WebSocket events first, then the bridge
//! events, then the native ones, each source in its own order; the sources share no clock, so
//! their relative order is only as good as `received_at` (bridge events get the time they
//! were polled).

use std::{
    collections::{BTreeSet, VecDeque},
    fmt,
    time::{Duration, Instant, SystemTime},
};

use serde_json::{Value, json};

use crate::{
    bridge,
    error::{Error, Result, bail},
    world::World,
};

/// `re_mcp:events` answers with at most this many events per call.
const BRIDGE_PAGE: usize = 128;
/// Bridge pages read per scan, so one read cannot stall on a flood of script events.
const BRIDGE_PAGES: usize = 8;
/// How often a wait polls the bridge; WebSocket events wake a wait at once.
const BRIDGE_POLL: Duration = Duration::from_millis(100);
/// The largest `Integer` argument: `re_mcp:events` returns no events after it, only `latest`.
const BRIDGE_NOW: i64 = i32::MAX as i64;
/// Native events the hub keeps; older ones are dropped.
const NATIVE_KEEP: usize = 4096;

/// Legacy WebSocket events (`subscribe` with this `eventName`) as community documentation
/// lists them: github.com/Sandertv/mcwss (`protocol/event`) and
/// github.com/p0t4t0sandwich/minecraft-be-websocket-api (`src/protocol/events`). The body
/// fields and PlayerMessage types follow minecraft-be-websocket-api; the useMethod numbers
/// follow mcwss (`item_used.go`). Not exhaustive, and current Bedrock versions may send
/// nothing for many of these.
pub const KNOWN_WS_EVENTS: &[(&str, &str)] = &[
    (
        "PlayerMessage",
        "Chat, /say, /tell, /me and /tellraw lines: {message, sender, receiver, type} where type names the kind of line, such as chat, say or tell, as community lists document it. Always subscribed.",
    ),
    (
        "ItemUsed",
        "The player used an item: {item: {id, namespace, aux}, player: {name, id, position, dimension, yRot, ...}, count, useMethod} (useMethod as mcwss lists it: 0 worn, 1 eaten, 3 drunk, 4 thrown, 5 released, 6 block entity created, 7 bottle filled, 8 bucket filled, 9 bucket emptied, 10 used on block).",
    ),
    (
        "PlayerTravelled",
        "The player moved: {player: {name, position, dimension, yRot, ...}, metersTravelled, isUnderwater, newBiome, travelMethod}.",
    ),
    (
        "PlayerTransform",
        "Periodic player position and rotation: {player: {...}}. Unconfirmed on current versions.",
    ),
    (
        "PlayerTeleported",
        "The player teleported: {player, cause, metersTravelled}. Unconfirmed on current versions.",
    ),
    (
        "PlayerDied",
        "The player died: {player, cause, killer}. Unconfirmed on current versions.",
    ),
    (
        "PlayerBounced",
        "The player bounced on a slime or bed block: {player, block, bounceHeight}. Unconfirmed on current versions.",
    ),
    (
        "BlockBroken",
        "The player broke a block: {player, block, tool, destructionMethod, count}. Unconfirmed on current versions.",
    ),
    (
        "BlockPlaced",
        "The player placed a block: {player, block, placementMethod, count}. Unconfirmed on current versions.",
    ),
    (
        "ItemAcquired",
        "An item entered the inventory: {player, item, acquisitionMethodId, count}. Unconfirmed on current versions.",
    ),
    (
        "ItemDropped",
        "The player dropped an item: {player, item, count}. Unconfirmed on current versions.",
    ),
    (
        "ItemEquipped",
        "The player equipped armor or an offhand item: {player, item, slot}. Unconfirmed on current versions.",
    ),
    (
        "ItemInteracted",
        "The player used an item on a block or entity: {player, item, method}. Unconfirmed on current versions.",
    ),
    (
        "ItemCrafted",
        "The player crafted an item: {player, item, count, craftedAutomatically, recipeBook}. Unconfirmed on current versions.",
    ),
    (
        "ItemSmelted",
        "The player took a smelted item from a furnace: {player, item, fuelSource}. Unconfirmed on current versions.",
    ),
    (
        "ItemDestroyed",
        "An item in use broke: {player, item}. Unconfirmed on current versions.",
    ),
    (
        "ItemEnchanted",
        "The player enchanted an item: {player, item, enchantments}. Unconfirmed on current versions.",
    ),
    (
        "ItemNamed",
        "The player renamed an item on an anvil: {player, item}. Unconfirmed on current versions.",
    ),
    (
        "MobKilled",
        "The player killed a mob: {player, victim, cause, weapon}. Unconfirmed on current versions.",
    ),
    (
        "MobInteracted",
        "The player interacted with a mob: {player, mob, interactionType}. Unconfirmed on current versions.",
    ),
    (
        "MobBorn",
        "A mob was bred: {player, baby}. Unconfirmed on current versions.",
    ),
    (
        "EntitySpawned",
        "The player spawned an entity, for example with a spawn egg: {player, mob, spawnType}. Unconfirmed on current versions.",
    ),
    (
        "BossKilled",
        "A boss died: {player, boss, partySize}. Unconfirmed on current versions.",
    ),
    (
        "PortalBuilt",
        "The player built a portal: {player, dimension}. Unconfirmed on current versions.",
    ),
    (
        "PortalUsed",
        "The player went through a portal: {player, fromDimension, toDimension}. Unconfirmed on current versions.",
    ),
    (
        "PotionBrewed",
        "The player brewed a potion: {player, potion, count}. Unconfirmed on current versions.",
    ),
    (
        "CameraUsed",
        "The player used the Education Edition camera: {player, isSelfie}. Unconfirmed on current versions.",
    ),
    (
        "AgentCommand",
        "An Education Edition agent ran a command: {result, commandName}. Unconfirmed on current versions.",
    ),
    (
        "AgentCreated",
        "An Education Edition agent was created. Unconfirmed on current versions.",
    ),
    (
        "SlashCommandExecuted",
        "A command ran: {commandName, successCount, outputMessage}. Unconfirmed on current versions.",
    ),
    (
        "ScreenChanged",
        "A UI screen opened: {screenName}. Unconfirmed on current versions.",
    ),
    (
        "VehicleExited",
        "The player left a vehicle: {player, vehicle}. Unconfirmed on current versions.",
    ),
    (
        "TradeCompleted",
        "The player traded with a villager: {player, item}. Unconfirmed on current versions.",
    ),
    (
        "EndOfDay",
        "A day ended in the world. Unconfirmed on current versions.",
    ),
];

/// Script events the Bridge pack records (`mc/bridge/data/scripts/events.ts`). An actor field
/// (player, attacker, source, entity, target, killer) is {id, type, name, location} where
/// name is the player name or name tag, so a confirmation can match a real or simulated
/// player by name.
pub const KNOWN_BRIDGE_EVENTS: &[(&str, &str)] = &[
    (
        "block_break",
        "A player broke a block: {player, block (type id), location, tool}.",
    ),
    (
        "block_place",
        "A player placed a block: {player, block: {type, location, ...}}.",
    ),
    (
        "entity_hit_entity",
        "An entity hit another with a melee attack: {attacker, target}.",
    ),
    (
        "entity_hit_block",
        "An entity hit a block: {attacker, block, location, face}.",
    ),
    (
        "entity_hurt",
        "An entity took damage: {entity, damage, cause, attacker}.",
    ),
    ("entity_die", "An entity died: {entity, cause, killer}."),
    (
        "entity_spawn",
        "An entity appeared: {entity, cause, item} where item is {id, amount, ...} for dropped item entities, else null.",
    ),
    (
        "entity_remove",
        "An entity was removed (despawned, picked up or unloaded): {id, type}.",
    ),
    (
        "player_interact_block",
        "A player used or interacted with a block: {player, block, face, item, first}.",
    ),
    (
        "player_interact_entity",
        "A player interacted with an entity: {player, target, item}.",
    ),
    ("item_use", "A player used an item: {player, item}."),
    (
        "item_start_use",
        "A player started charging an item (bow, food, potion): {player, item, duration}.",
    ),
    (
        "item_stop_use",
        "A player stopped charging an item: {player, item, duration}.",
    ),
    (
        "item_release_use",
        "A player released a charged item (bow, crossbow, trident): {player, item, duration}.",
    ),
    (
        "item_complete_use",
        "A player finished charging an item (ate, drank): {player, item, duration}.",
    ),
    (
        "item_start_use_on",
        "A player started using an item on a block: {player, item, block, face}.",
    ),
    (
        "item_stop_use_on",
        "A player stopped using an item on a block: {player, item, block}.",
    ),
    (
        "inventory_change",
        "A player's inventory slot changed: {player, inventory, slot, before, after}.",
    ),
    (
        "hotbar_select",
        "A player selected another hotbar slot: {player, from, to, item}.",
    ),
    (
        "projectile_hit_block",
        "A projectile hit a block: {projectile, source, location}.",
    ),
    (
        "projectile_hit_entity",
        "A projectile hit an entity: {projectile, source, target}.",
    ),
    ("container_open", "A block container opened: {block}."),
    ("container_close", "A block container closed: {block}."),
    (
        "dimension_change",
        "A player changed dimension: {player, from, to}.",
    ),
    ("player_spawn", "A player spawned: {player, initial}."),
    ("weather_change", "The weather changed: {from, to}."),
    ("game_rule_change", "A game rule changed: {rule, value}."),
];

/// Events this server derives from the unstable layer's native entity reads while an action
/// sequence runs without the Bridge pack (`drivers::native`). Each carries the server tick of
/// the read that showed it; an actor field is {id, type}.
pub const KNOWN_NATIVE_EVENTS: &[(&str, &str)] = &[
    (
        "entity_hurt",
        "A tracked entity's health dropped between two reads: {entity, damage, health, cause: null, attacker: null}.",
    ),
    (
        "entity_die",
        "A tracked entity's health read 0, or it was gone from the local server after a read showed it: {entity, cause: null, killer: null, seen} where seen is health_zero or gone (gone also covers a despawn or unload).",
    ),
];

pub fn known_ws_event(name: &str) -> bool {
    KNOWN_WS_EVENTS.iter().any(|(known, _)| *known == name)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Ws,
    Bridge,
    Native,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ws => "ws",
            Self::Bridge => "bridge",
            Self::Native => "native",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BridgeMark {
    /// The bridge script instance; its sequence numbers restart when scripts reload.
    epoch: String,
    /// The last bridge event read.
    seq: u64,
}

/// A position in the merged stream: events after it are new.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EventCursor {
    /// The next WebSocket event number.
    ws: u64,
    /// The next native event number.
    native: u64,
    /// `None`: no bridge event was seen yet, so every bridge event is new.
    bridge: Option<BridgeMark>,
}

impl fmt::Display for EventCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "w{}", self.ws)?;
        if self.native > 0 {
            write!(f, "-n{}", self.native)?;
        }
        if let Some(mark) = &self.bridge {
            write!(f, "-b{}-{}", mark.epoch, mark.seq)?;
        }
        Ok(())
    }
}

impl EventCursor {
    pub fn parse(text: &str) -> Result<Self> {
        let invalid = || {
            Error::msg(format!(
                "Invalid event cursor {text:?}; pass a cursor an events tool returned."
            ))
        };
        let mut parts = text.split('-').peekable();
        let ws = parts
            .next()
            .and_then(|part| part.strip_prefix('w'))
            .and_then(|number| number.parse().ok())
            .ok_or_else(invalid)?;
        let native = match parts.next_if(|part| part.starts_with('n')) {
            Some(part) => part[1..].parse().map_err(|_| invalid())?,
            None => 0,
        };
        let bridge = match (parts.next(), parts.next(), parts.next()) {
            (None, _, _) => None,
            (Some(epoch), Some(seq), None) => {
                let epoch = epoch
                    .strip_prefix('b')
                    .filter(|epoch| {
                        !epoch.is_empty() && epoch.chars().all(|c| c.is_ascii_alphanumeric())
                    })
                    .ok_or_else(invalid)?;
                Some(BridgeMark {
                    epoch: epoch.to_owned(),
                    seq: seq.parse().map_err(|_| invalid())?,
                })
            }
            _ => return Err(invalid()),
        };
        Ok(Self { ws, native, bridge })
    }
}

/// One event from either source.
#[derive(Clone, Debug)]
pub struct HubEvent {
    /// The position right after this event.
    pub cursor: EventCursor,
    pub source: Source,
    /// WebSocket event name (`PlayerMessage`) or bridge event type (`block_break`).
    pub name: String,
    /// The game tick, for bridge events.
    pub tick: Option<u64>,
    /// When this server received (WebSocket) or polled (bridge) it, in Unix milliseconds.
    pub received_at: u64,
    pub body: Value,
}

impl HubEvent {
    pub fn to_json(&self) -> Value {
        let mut event = json!({
            "cursor": self.cursor.to_string(),
            "source": self.source.as_str(),
            "name": self.name,
            "received_at": self.received_at,
            "body": self.body,
        });
        if let Some(tick) = self.tick {
            event["tick"] = json!(tick);
        }
        event
    }

    /// Whether the body, as JSON text, contains `text`.
    pub fn body_contains(&self, text: &str) -> bool {
        self.body.to_string().contains(text)
    }
}

/// Which events a read returns; empty lists mean everything.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    /// Event names, compared ignoring ASCII case.
    pub names: Vec<String>,
    pub sources: Vec<Source>,
}

impl Filter {
    fn wants_source(&self, source: Source) -> bool {
        self.sources.is_empty() || self.sources.contains(&source)
    }

    fn wants_name(&self, name: &str) -> bool {
        self.names.is_empty()
            || self
                .names
                .iter()
                .any(|wanted| wanted.eq_ignore_ascii_case(name))
    }
}

/// What one read found.
#[derive(Debug, Default)]
pub struct Batch {
    pub events: Vec<HubEvent>,
    /// Where the next read continues: past every event this read looked at.
    pub cursor: EventCursor,
    /// Events after the starting cursor that a source had already dropped.
    pub dropped_ws: u64,
    pub dropped_bridge: u64,
    pub dropped_native: u64,
    /// Why a source was skipped or restarted.
    pub notes: Vec<String>,
}

impl Batch {
    fn note(&mut self, note: String) {
        if !self.notes.contains(&note) {
            self.notes.push(note);
        }
    }
}

/// Session state for the merged stream.
#[derive(Default)]
pub struct EventHub {
    bridge_names: BTreeSet<String>,
    /// The native events kept, oldest first; each one's cursor is right after it.
    native: VecDeque<HubEvent>,
    /// How many native events were ever pushed: the next one's number.
    native_count: u64,
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// Whether bridge events can be read now; a note says why not.
fn bridge_ready(world: &World, batch: &mut Batch) -> Result<bool> {
    if !world.connected() {
        batch.note("The WebSocket is not connected, so bridge events were not polled.".into());
        return Ok(false);
    }
    if world.bridge(false)? {
        return Ok(true);
    }
    batch.note(format!(
        "No bridge events: the Bridge pack did not answer. {}",
        bridge::INSTALL_HINT
    ));
    Ok(false)
}

fn bridge_epoch(reply: &Value) -> Result<String> {
    reply["epoch"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::msg("re_mcp:events answered without an epoch, so the Bridge pack in this world is older than this server. Export the current pack, then run /reload all."))
}

fn bridge_poll(world: &World, since: i64) -> Result<Value> {
    world.bridge_command(&format!("re_mcp:events {since}"))
}

impl EventHub {
    /// Bridge event types seen this session.
    pub fn observed_bridge(&self) -> Vec<String> {
        self.bridge_names.iter().cloned().collect()
    }

    /// Add an event derived from a native read, at the end of the native source.
    pub fn push_native(&mut self, name: &str, tick: u64, body: Value) {
        self.native_count += 1;
        self.native.push_back(HubEvent {
            cursor: EventCursor {
                native: self.native_count,
                ..EventCursor::default()
            },
            source: Source::Native,
            name: name.to_owned(),
            tick: Some(tick),
            received_at: unix_millis(),
            body,
        });
        if self.native.len() > NATIVE_KEEP {
            self.native.pop_front();
        }
    }

    /// The current end of the stream: only events that arrive later are after it.
    pub fn now(&self, world: &World) -> Result<EventCursor> {
        let ws = world.socket().event_count();
        let bridge = if world.connected() && world.bridge(false)? {
            let reply = bridge_poll(world, BRIDGE_NOW)?;
            Some(BridgeMark {
                epoch: bridge_epoch(&reply)?,
                seq: reply["latest"].as_u64().unwrap_or(0),
            })
        } else {
            None
        };
        Ok(EventCursor {
            ws,
            native: self.native_count,
            bridge,
        })
    }

    /// Up to `limit` events after `since` that pass `filter` and `matches`, waiting up to
    /// `wait` for the first one. The returned cursor moves past every event looked at,
    /// including the ones filtered out.
    pub fn read(
        &mut self,
        world: &World,
        since: &EventCursor,
        filter: &Filter,
        limit: usize,
        wait: Duration,
        matches: &dyn Fn(&HubEvent) -> bool,
    ) -> Result<Batch> {
        let deadline = Instant::now() + wait;
        let mut batch = Batch {
            cursor: since.clone(),
            ..Batch::default()
        };
        loop {
            let ws_mark = world.socket().event_count();
            let polled_bridge = self.scan(world, filter, limit.max(1), matches, &mut batch)?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if !batch.events.is_empty() || remaining.is_zero() {
                return Ok(batch);
            }
            let slice = if polled_bridge {
                remaining.min(BRIDGE_POLL)
            } else {
                remaining
            };
            world.socket().wait_event(ws_mark, slice, |_| true);
        }
    }

    /// One pass over every source from `batch.cursor`. Returns whether the bridge was polled.
    fn scan(
        &mut self,
        world: &World,
        filter: &Filter,
        limit: usize,
        matches: &dyn Fn(&HubEvent) -> bool,
        batch: &mut Batch,
    ) -> Result<bool> {
        let mut pending = Vec::new();
        if filter.wants_source(Source::Ws) {
            let (events, dropped) = world.socket().events_after(batch.cursor.ws, usize::MAX);
            batch.dropped_ws += dropped;
            batch.cursor.ws += dropped;
            pending.extend(events.into_iter().map(|event| HubEvent {
                cursor: EventCursor {
                    ws: event.seq + 1,
                    ..EventCursor::default()
                },
                source: Source::Ws,
                name: event.name,
                tick: None,
                received_at: event.received_at,
                body: event.body,
            }));
        }
        let mut bridge_end = None;
        let polled_bridge = filter.wants_source(Source::Bridge) && bridge_ready(world, batch)?;
        if polled_bridge {
            let (events, end) = self.bridge_events(world, batch)?;
            pending.extend(events);
            bridge_end = Some(end);
        }
        if filter.wants_source(Source::Native) {
            let first = self.native_count - self.native.len() as u64;
            if batch.cursor.native < first {
                batch.dropped_native += first - batch.cursor.native;
                batch.cursor.native = first;
            }
            let skip = (batch.cursor.native - first) as usize;
            pending.extend(self.native.iter().skip(skip).cloned());
        }
        for mut event in pending {
            match event.source {
                Source::Ws => batch.cursor.ws = event.cursor.ws,
                Source::Bridge => batch.cursor.bridge = event.cursor.bridge.take(),
                Source::Native => batch.cursor.native = event.cursor.native,
            }
            event.cursor = batch.cursor.clone();
            if filter.wants_name(&event.name) && matches(&event) {
                batch.events.push(event);
                if batch.events.len() >= limit {
                    return Ok(polled_bridge);
                }
            }
        }
        if let Some(end) = bridge_end {
            batch.cursor.bridge = Some(end);
        }
        Ok(polled_bridge)
    }

    /// Bridge events after `batch.cursor`, at most [`BRIDGE_PAGES`] pages, and the mark
    /// after the last page.
    fn bridge_events(
        &mut self,
        world: &World,
        batch: &mut Batch,
    ) -> Result<(Vec<HubEvent>, BridgeMark)> {
        let mut since = batch.cursor.bridge.as_ref().map_or(0, |mark| mark.seq);
        let mut reply = bridge_poll(world, since as i64)?;
        let epoch = bridge_epoch(&reply)?;
        if batch
            .cursor
            .bridge
            .as_ref()
            .is_some_and(|mark| mark.epoch != epoch)
        {
            batch.note(
                "The Bridge scripts restarted since the cursor (world reopened or /reload), so \
                 bridge events are read from the restart on; events from before it are gone."
                    .into(),
            );
            since = 0;
            reply = bridge_poll(world, 0)?;
        }
        let received_at = unix_millis();
        let mut events = Vec::new();
        for page in 0..BRIDGE_PAGES {
            if page > 0 {
                reply = bridge_poll(world, since as i64)?;
                if bridge_epoch(&reply)? != epoch {
                    // Restarted mid-read; the next read starts over.
                    break;
                }
            }
            batch.dropped_bridge += reply["dropped"].as_u64().unwrap_or(0);
            let page_events = reply["events"].as_array().cloned().unwrap_or_default();
            let count = page_events.len();
            for raw in page_events {
                let Value::Object(mut fields) = raw else {
                    continue;
                };
                let Some(seq) = fields.remove("seq").and_then(|seq| seq.as_u64()) else {
                    continue;
                };
                let tick = fields.remove("tick").and_then(|tick| tick.as_u64());
                let name = match fields.remove("type") {
                    Some(Value::String(name)) => name,
                    _ => continue,
                };
                self.bridge_names.insert(name.clone());
                events.push(HubEvent {
                    cursor: EventCursor {
                        bridge: Some(BridgeMark {
                            epoch: epoch.clone(),
                            seq,
                        }),
                        ..EventCursor::default()
                    },
                    source: Source::Bridge,
                    name,
                    tick,
                    received_at,
                    body: Value::Object(fields),
                });
            }
            since = reply["next"].as_u64().unwrap_or(since);
            if count < BRIDGE_PAGE {
                break;
            }
        }
        Ok((events, BridgeMark { epoch, seq: since }))
    }
}

/// Parse a list of source names.
pub fn parse_sources(names: &[String]) -> Result<Vec<Source>> {
    names
        .iter()
        .map(|name| match name.as_str() {
            "ws" => Ok(Source::Ws),
            "bridge" => Ok(Source::Bridge),
            "native" => Ok(Source::Native),
            other => bail!("Unknown event source {other:?}; use ws, bridge or native."),
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod fake {
    //! A game on a real WebSocket: answers commands, sends WebSocket events on request, and
    //! plays the Bridge pack's `re_mcp:ping` and `re_mcp:events`.

    use std::{
        net::TcpStream,
        sync::{Arc, Mutex, mpsc},
        time::Duration,
    };

    use serde_json::{Value, json};
    use tungstenite::{Message, WebSocket, stream::MaybeTlsStream};

    use crate::{world::World, ws::GameSocket};

    #[derive(Default)]
    pub struct Bridge {
        pub present: bool,
        pub epoch: String,
        pub events: Vec<Value>,
        pub seq: u64,
    }

    pub struct FakeGame {
        pub world: World,
        outgoing: mpsc::Sender<Value>,
        pub bridge: Arc<Mutex<Bridge>>,
        pub commands: Arc<Mutex<Vec<String>>>,
    }

    impl FakeGame {
        pub fn start(bridge: bool) -> Self {
            let socket = Arc::new(GameSocket::bind().unwrap());
            let (client, _) =
                tungstenite::connect(format!("ws://127.0.0.1:{}", socket.port())).unwrap();
            if let MaybeTlsStream::Plain(stream) = client.get_ref() {
                stream
                    .set_read_timeout(Some(Duration::from_millis(5)))
                    .unwrap();
            }
            assert!(socket.wait_connected(Duration::from_secs(5)));
            let bridge = Arc::new(Mutex::new(Bridge {
                present: bridge,
                epoch: "first".into(),
                ..Bridge::default()
            }));
            let commands = Arc::new(Mutex::new(Vec::new()));
            let (outgoing, incoming) = mpsc::channel();
            std::thread::spawn({
                let bridge = Arc::clone(&bridge);
                let commands = Arc::clone(&commands);
                move || serve(client, &incoming, &bridge, &commands)
            });
            Self {
                world: World::new(socket),
                outgoing,
                bridge,
                commands,
            }
        }

        /// Send a WebSocket event as the game.
        pub fn send_event(&self, name: &str, body: Value) {
            self.outgoing
                .send(
                    json!({"header": {"messagePurpose": "event", "eventName": name}, "body": body}),
                )
                .unwrap();
        }

        /// Record a bridge script event.
        pub fn push_bridge(&self, kind: &str, details: Value) {
            let mut bridge = self.bridge.lock().unwrap();
            bridge.seq += 1;
            let mut event = json!({"seq": bridge.seq, "tick": 100 + bridge.seq, "type": kind});
            event
                .as_object_mut()
                .unwrap()
                .extend(details.as_object().unwrap().clone());
            bridge.events.push(event);
        }

        /// Reload the bridge scripts: a new epoch and an empty history.
        pub fn restart_bridge(&self) {
            let mut bridge = self.bridge.lock().unwrap();
            bridge.epoch = "second".into();
            bridge.events.clear();
            bridge.seq = 0;
        }
    }

    fn answer(line: &str, bridge: &Mutex<Bridge>) -> Value {
        let bridge = bridge.lock().unwrap();
        let reply = |value: Value| json!({"statusCode": 0, "statusMessage": value.to_string()});
        if line.starts_with("re_mcp:") && !bridge.present {
            return json!({"statusCode": -2147483648, "statusMessage": format!("Syntax error: Unexpected \"{line}\": at \">>{line}<<\"")});
        }
        if line == "re_mcp:ping" {
            return reply(json!({"version": 1, "tick": 5}));
        }
        if let Some(since) = line.strip_prefix("re_mcp:events ") {
            let since: u64 = since.parse().unwrap();
            let newer: Vec<Value> = bridge
                .events
                .iter()
                .filter(|event| event["seq"].as_u64().unwrap() > since)
                .take(128)
                .cloned()
                .collect();
            let next = newer.last().map_or(since.max(bridge.seq), |event| {
                event["seq"].as_u64().unwrap()
            });
            return reply(
                json!({"epoch": bridge.epoch, "latest": bridge.seq, "events": newer, "next": next, "dropped": 0}),
            );
        }
        if line.starts_with("testfor") {
            return json!({"statusCode": 1, "statusMessage": "No targets matched selector"});
        }
        json!({"statusCode": 0, "statusMessage": format!("ran {line}")})
    }

    fn serve(
        mut client: WebSocket<MaybeTlsStream<TcpStream>>,
        incoming: &mpsc::Receiver<Value>,
        bridge: &Mutex<Bridge>,
        commands: &Mutex<Vec<String>>,
    ) {
        loop {
            loop {
                match incoming.try_recv() {
                    Ok(event) => {
                        if client.send(Message::text(event.to_string())).is_err() {
                            return;
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return,
                }
            }
            let text = match client.read() {
                Ok(Message::Text(text)) => text,
                Ok(_) => continue,
                Err(tungstenite::Error::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }
                Err(_) => return,
            };
            let request: Value = serde_json::from_str(text.as_str()).unwrap();
            if request["header"]["messagePurpose"] != "commandRequest" {
                continue;
            }
            let line = request["body"]["commandLine"].as_str().unwrap().to_owned();
            let body = answer(&line, bridge);
            commands.lock().unwrap().push(line);
            let reply = json!({
                "header": {"messagePurpose": "commandResponse", "requestId": request["header"]["requestId"]},
                "body": body,
            });
            if client.send(Message::text(reply.to_string())).is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::{fake::FakeGame, *};

    const NONE: Duration = Duration::ZERO;

    fn all(_: &HubEvent) -> bool {
        true
    }

    fn names(batch: &Batch) -> Vec<(Source, String)> {
        batch
            .events
            .iter()
            .map(|event| (event.source, event.name.clone()))
            .collect()
    }

    /// Wait until the socket has received `count` events in total.
    fn received(game: &FakeGame, count: u64) {
        assert!(
            game.world
                .socket()
                .wait_event(count - 1, Duration::from_secs(5), |_| true)
                .is_some()
        );
    }

    #[test]
    fn cursors_round_trip_as_text_and_reject_garbage() {
        let cursor = EventCursor {
            ws: 12,
            native: 5,
            bridge: Some(BridgeMark {
                epoch: "abc9".into(),
                seq: 34,
            }),
        };
        assert_eq!(EventCursor::parse(&cursor.to_string()).unwrap(), cursor);
        let no_native = EventCursor {
            native: 0,
            ..cursor
        };
        assert_eq!(
            EventCursor::parse(&no_native.to_string()).unwrap(),
            no_native
        );
        assert_eq!(EventCursor::parse("w0").unwrap(), EventCursor::default());
        for bad in [
            "",
            "12",
            "w",
            "wx",
            "w1-n",
            "w1-nx",
            "w1-b-2",
            "w1-bab",
            "w1-bab-x",
            "w1-bab-2-3",
        ] {
            assert!(EventCursor::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn native_events_follow_the_other_sources_and_move_the_cursor() {
        let game = FakeGame::start(false);
        let mut hub = EventHub::default();
        let start = hub.now(&game.world).unwrap();
        hub.push_native("entity_hurt", 7, json!({"entity": {"id": "-5"}}));
        hub.push_native("entity_die", 9, json!({"entity": {"id": "-5"}}));
        let filter = Filter::default();
        let first = hub
            .read(&game.world, &start, &filter, 1, NONE, &all)
            .unwrap();
        assert_eq!(names(&first), [(Source::Native, "entity_hurt".into())]);
        assert_eq!(first.events[0].tick, Some(7));
        let cursor = EventCursor::parse(&first.cursor.to_string()).unwrap();
        let second = hub
            .read(&game.world, &cursor, &filter, 10, NONE, &all)
            .unwrap();
        assert_eq!(names(&second), [(Source::Native, "entity_die".into())]);
        assert_eq!(hub.now(&game.world).unwrap(), second.cursor);
        let only_ws = Filter {
            sources: vec![Source::Ws],
            ..Filter::default()
        };
        let skipped = hub
            .read(&game.world, &start, &only_ws, 10, NONE, &all)
            .unwrap();
        assert!(skipped.events.is_empty());
    }

    #[test]
    fn reads_page_through_both_sources_with_one_cursor() {
        let game = FakeGame::start(true);
        let mut hub = EventHub::default();
        game.send_event("PlayerMessage", json!({"message": "one"}));
        game.send_event("PlayerMessage", json!({"message": "two"}));
        received(&game, 2);
        game.push_bridge("block_break", json!({"block": "minecraft:stone"}));
        game.push_bridge("item_use", json!({"item": {"id": "minecraft:bow"}}));

        let filter = Filter::default();
        let first = hub
            .read(&game.world, &EventCursor::default(), &filter, 3, NONE, &all)
            .unwrap();
        assert_eq!(
            names(&first),
            [
                (Source::Ws, "PlayerMessage".into()),
                (Source::Ws, "PlayerMessage".into()),
                (Source::Bridge, "block_break".into()),
            ]
        );
        assert_eq!(first.events[2].tick, Some(101));
        assert_eq!(first.events[2].body["block"], "minecraft:stone");
        // The cursor after the last returned event continues the page.
        assert_eq!(first.cursor, first.events[2].cursor);
        let cursor = EventCursor::parse(&first.cursor.to_string()).unwrap();

        let second = hub
            .read(&game.world, &cursor, &filter, 3, NONE, &all)
            .unwrap();
        assert_eq!(names(&second), [(Source::Bridge, "item_use".into())]);
        let third = hub
            .read(&game.world, &second.cursor, &filter, 3, NONE, &all)
            .unwrap();
        assert!(third.events.is_empty());
        assert_eq!(third.cursor, second.cursor);

        // New events on both sides come after the cursor.
        game.send_event("PlayerMessage", json!({"message": "three"}));
        received(&game, 3);
        game.push_bridge(
            "entity_spawn",
            json!({"item": {"id": "minecraft:dirt", "amount": 1}}),
        );
        let fourth = hub
            .read(&game.world, &third.cursor, &filter, 10, NONE, &all)
            .unwrap();
        assert_eq!(
            names(&fourth),
            [
                (Source::Ws, "PlayerMessage".into()),
                (Source::Bridge, "entity_spawn".into()),
            ]
        );
        assert_eq!(fourth.events[0].body["message"], "three");
        assert_eq!(
            hub.observed_bridge(),
            ["block_break", "entity_spawn", "item_use"]
        );
    }

    #[test]
    fn filters_skip_events_but_move_the_cursor_past_them() {
        let game = FakeGame::start(true);
        let mut hub = EventHub::default();
        game.send_event("PlayerMessage", json!({"message": "chat"}));
        received(&game, 1);
        game.push_bridge("block_break", json!({}));
        game.push_bridge("item_use", json!({}));
        let only_items = Filter {
            names: vec!["ITEM_USE".into()],
            sources: Vec::new(),
        };
        let batch = hub
            .read(
                &game.world,
                &EventCursor::default(),
                &only_items,
                10,
                NONE,
                &all,
            )
            .unwrap();
        assert_eq!(names(&batch), [(Source::Bridge, "item_use".into())]);
        let rest = hub
            .read(
                &game.world,
                &batch.cursor,
                &Filter::default(),
                10,
                NONE,
                &all,
            )
            .unwrap();
        assert!(rest.events.is_empty());

        let only_ws = Filter {
            names: Vec::new(),
            sources: vec![Source::Ws],
        };
        let batch = hub
            .read(
                &game.world,
                &EventCursor::default(),
                &only_ws,
                10,
                NONE,
                &all,
            )
            .unwrap();
        assert_eq!(names(&batch), [(Source::Ws, "PlayerMessage".into())]);
    }

    #[test]
    fn a_bridge_restart_is_noted_and_read_from_its_start() {
        let game = FakeGame::start(true);
        let mut hub = EventHub::default();
        game.push_bridge("block_break", json!({}));
        let before = hub
            .read(
                &game.world,
                &EventCursor::default(),
                &Filter::default(),
                10,
                NONE,
                &all,
            )
            .unwrap();
        assert_eq!(before.events.len(), 1);
        game.restart_bridge();
        game.push_bridge("block_place", json!({}));
        let after = hub
            .read(
                &game.world,
                &before.cursor,
                &Filter::default(),
                10,
                NONE,
                &all,
            )
            .unwrap();
        assert_eq!(names(&after), [(Source::Bridge, "block_place".into())]);
        assert!(after.notes.iter().any(|note| note.contains("restarted")));
    }

    #[test]
    fn without_the_bridge_reads_ws_events_and_say_why() {
        let game = FakeGame::start(false);
        let mut hub = EventHub::default();
        game.send_event("PlayerMessage", json!({"message": "hi"}));
        received(&game, 1);
        let batch = hub
            .read(
                &game.world,
                &EventCursor::default(),
                &Filter::default(),
                10,
                NONE,
                &all,
            )
            .unwrap();
        assert_eq!(names(&batch), [(Source::Ws, "PlayerMessage".into())]);
        assert!(batch.notes.iter().any(|note| note.contains("rgl run")));
        assert_eq!(hub.now(&game.world).unwrap().to_string(), "w1");
    }

    #[test]
    fn waits_return_a_matching_event_after_the_cursor_or_time_out() {
        let game = FakeGame::start(true);
        let mut hub = EventHub::default();
        // Matching events from before the cursor do not count.
        game.push_bridge("block_break", json!({"player": {"name": "Steve"}}));
        game.send_event("PlayerMessage", json!({"message": "old"}));
        received(&game, 1);
        let now = hub.now(&game.world).unwrap();
        let steve =
            |event: &HubEvent| event.name == "block_break" && event.body_contains("\"Steve\"");
        let timed_out = hub
            .read(
                &game.world,
                &now,
                &Filter::default(),
                1,
                Duration::from_millis(300),
                &steve,
            )
            .unwrap();
        assert!(timed_out.events.is_empty());

        let bridge = std::sync::Arc::clone(&game.bridge);
        let pusher = thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            let mut bridge = bridge.lock().unwrap();
            for (seq, name) in [(2, "Alex"), (3, "Steve")] {
                bridge.seq = seq;
                bridge.events.push(json!({"seq": seq, "tick": seq, "type": "block_break", "player": {"name": name}}));
            }
        });
        let started = Instant::now();
        let found = hub
            .read(
                &game.world,
                &now,
                &Filter::default(),
                1,
                Duration::from_secs(5),
                &steve,
            )
            .unwrap();
        pusher.join().unwrap();
        assert!(started.elapsed() >= Duration::from_millis(100));
        assert_eq!(found.events.len(), 1);
        assert_eq!(found.events[0].tick, Some(3));

        // WebSocket events wake a wait without polling delay.
        let ws_now = hub.now(&game.world).unwrap();
        let world = game.world.clone();
        let waiter = thread::spawn(move || {
            EventHub::default()
                .read(
                    &world,
                    &ws_now,
                    &Filter::default(),
                    1,
                    Duration::from_secs(5),
                    &|event: &HubEvent| event.body_contains("new"),
                )
                .unwrap()
        });
        thread::sleep(Duration::from_millis(50));
        game.send_event("PlayerMessage", json!({"message": "new"}));
        let batch = waiter.join().unwrap();
        assert_eq!(batch.events[0].body["message"], "new");
    }
}
