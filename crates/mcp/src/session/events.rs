//! The merged event stream (`crate::events`) as session operations, for tools and for other
//! tools that confirm their actions by events.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use super::Session;
use crate::{
    error::{Result, bail},
    events::{self, EventCursor, Filter, HubEvent},
    ws,
};

const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 1000;
const MAX_WAIT: Duration = Duration::from_secs(300);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventsReadArgs {
    since: Option<String>,
    #[serde(default)]
    names: Vec<String>,
    #[serde(default)]
    sources: Vec<String>,
    limit: Option<usize>,
    #[serde(default)]
    wait_ms: u64,
}

/// What [`Session::wait_event`] found.
pub(crate) struct Waited {
    pub event: Option<HubEvent>,
    pub notes: Vec<String>,
}

impl Session {
    /// The current end of the merged event stream. Take it before acting; events that the
    /// action causes come after it. Connects the WebSocket first if needed.
    pub(crate) fn event_cursor(&mut self) -> Result<EventCursor> {
        let world = self.ensure_world()?;
        self.events.now(&world)
    }

    /// The first event after `after` for which `matches` holds, from either source, waiting
    /// up to `timeout`; `event` is `None` when none arrived in time, and `notes` say why a
    /// source was skipped (no Bridge pack, scripts restarted). WebSocket events are seen at
    /// once, bridge events within about 100 ms (the bridge poll interval).
    pub(crate) fn wait_event(
        &mut self,
        after: &EventCursor,
        timeout: Duration,
        matches: impl Fn(&HubEvent) -> bool,
    ) -> Result<Waited> {
        let world = self.ensure_world()?;
        let batch = self
            .events
            .read(&world, after, &Filter::default(), 1, timeout, &matches)?;
        Ok(Waited {
            event: batch.events.into_iter().next(),
            notes: batch.notes,
        })
    }

    pub(crate) fn events_read(&mut self, args: EventsReadArgs) -> Result<Value> {
        // "now" marks the current end of the stream: read nothing and return its cursor, to
        // take before an action.
        if args.since.as_deref() == Some("now") {
            let cursor = self.event_cursor()?;
            return Ok(json!({
                "source": "event",
                "events": [],
                "cursor": cursor.to_string(),
                "dropped": { "ws": 0, "bridge": 0, "native": 0 },
                "notes": [],
            }));
        }
        let since = args
            .since
            .as_deref()
            .map(EventCursor::parse)
            .transpose()?
            .unwrap_or_default();
        let filter = Filter {
            names: args.names,
            sources: events::parse_sources(&args.sources)?,
        };
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT);
        if limit == 0 || limit > MAX_LIMIT {
            bail!("limit must be between 1 and {MAX_LIMIT}.");
        }
        let wait = Duration::from_millis(args.wait_ms).min(MAX_WAIT);
        let world = self.ensure_world()?;
        let batch = self
            .events
            .read(&world, &since, &filter, limit, wait, &|_| true)?;
        Ok(json!({
            "source": "event",
            "events": batch.events.iter().map(HubEvent::to_json).collect::<Vec<_>>(),
            "cursor": batch.cursor.to_string(),
            "dropped": { "ws": batch.dropped_ws, "bridge": batch.dropped_bridge, "native": batch.dropped_native },
            "notes": batch.notes,
        }))
    }

    pub(crate) fn events_list(&self) -> Result<Value> {
        let stats = self.world()?.socket().stats();
        let list = |known: &[(&str, &str)]| {
            known
                .iter()
                .map(|(name, description)| json!({ "name": name, "description": description }))
                .collect::<Vec<_>>()
        };
        Ok(json!({
            "source": "event",
            "ws": list(events::KNOWN_WS_EVENTS),
            "bridge": list(events::KNOWN_BRIDGE_EVENTS),
            "native": list(events::KNOWN_NATIVE_EVENTS),
            "always_subscribed": ws::ALWAYS_SUBSCRIBED,
            "subscribed": stats.subscriptions,
            "observed": { "ws": stats.observed, "bridge": self.events.observed_bridge() },
            "notes": [
                "Neither the ws nor the bridge list is exhaustive. The WebSocket names come from community lists (mcwss, minecraft-be-websocket-api), and a current game may send nothing for many of them. event_subscribe accepts names outside the list.",
                "Bridge events need the bedrock-mcp Bridge pack in the world and need no subscription.",
                "Native events come only while an action_sequence runs without the Bridge pack and the unstable layer's world_state capability is active (single player): the sequence reads its target entities' health and reports the changes.",
            ],
        }))
    }
}
