//! `player_render_get` and `perf_get`: the unstable layer's player_render and perf
//! capabilities.

use protocol::unstable::{AnimationComponent, Perf};
use serde::Deserialize;
use serde_json::{Value, json};

use super::Session;
use crate::{
    bridge::Actor,
    error::{Result, bail},
};

/// Frames longer than this count as long frames unless the caller sets another bound.
const LONG_FRAME_MS: f64 = 50.0;
const DEFAULT_RECENT: usize = 16;
const MAX_RECENT: usize = 256;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlayerRenderArgs {
    wait_ms: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerfArgs {
    window_ms: Option<f64>,
    recent: Option<usize>,
    long_frame_ms: Option<f64>,
}

impl Session {
    pub(crate) fn player_render_get(&mut self, args: PlayerRenderArgs) -> Result<Value> {
        let mut notes = Vec::new();
        self.connect_running_agent();
        if !self.unstable_capability("player_render", &mut notes) {
            bail!(
                "player_render_get needs the unstable layer's player_render capability: {}",
                notes.join("; ")
            );
        }
        let actor = self.local_actor();
        let render = self.unstable.player_render(args.wait_ms, actor)?;
        Ok(json!({
            "frame": render.frame,
            "components": render
                .components
                .iter()
                .map(|component| component_json(component, render.frame))
                .collect::<Vec<_>>(),
            "visits_by_group": render.visits_by_group,
            "detail": render.detail,
        }))
    }

    pub(crate) fn perf_get(&mut self, args: PerfArgs) -> Result<Value> {
        let mut notes = Vec::new();
        self.connect_running_agent();
        if !self.unstable_capability("perf", &mut notes) {
            bail!(
                "perf_get needs the unstable layer's perf capability: {}",
                notes.join("; ")
            );
        }
        let perf = self.unstable.perf()?;
        Ok(perf_json(&perf, &args))
    }

    /// The local player's unique id when the WebSocket is already connected: from the Bridge
    /// pack, else `/querytarget`. Never connects; the DLL then uses the id it learned itself.
    fn local_actor(&self) -> Option<i64> {
        let world = self.world().ok()?;
        if !world.connected() {
            return None;
        }
        let id = if world.bridge(false).ok()? {
            world.bridge_command("re_mcp:state").ok()?["id"]
                .as_str()?
                .to_owned()
        } else {
            world.actor_target(&Actor::Me).ok()?.unique_id
        };
        id.parse().ok()
    }
}

fn group_name(group: u32) -> &'static str {
    match group {
        0 => "client",
        1 => "client_hud",
        2 => "server",
        3 => "client_map",
        4 => "client_remote",
        5 => "first_person",
        _ => "unknown",
    }
}

/// One component with its group named and how many frames its last update lags `frame`.
fn component_json(component: &AnimationComponent, frame: i64) -> Value {
    let mut value = json!(component);
    value["group"] = json!(group_name(component.group));
    // 64-bit ids lose precision as JSON numbers.
    value["owner"] = json!(format!("{:#018x}", component.owner));
    value["id"] = json!(format!("{:#x}", component.id));
    value["frames_behind"] = json!(frame - component.last_update_frame);
    value["applied"] = json!(component.applied);
    value["children"] = component
        .children
        .iter()
        .map(|child| component_json(child, frame))
        .collect();
    value
}

/// Summary of `values` in milliseconds; null when there are none.
fn stats(values: &[f32], long_frame_ms: Option<f64>) -> Value {
    if values.is_empty() {
        return Value::Null;
    }
    let mut sorted: Vec<f64> = values.iter().map(|value| f64::from(*value)).collect();
    sorted.sort_by(f64::total_cmp);
    let rank = |fraction: f64| {
        let index = ((fraction * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
        sorted[index - 1]
    };
    let mean = sorted.iter().sum::<f64>() / sorted.len() as f64;
    let round = |value: f64| (value * 1000.0).round() / 1000.0;
    let mut summary = json!({
        "count": sorted.len(),
        "mean": round(mean),
        "min": round(sorted[0]),
        "p50": round(rank(0.5)),
        "p90": round(rank(0.9)),
        "p95": round(rank(0.95)),
        "p99": round(rank(0.99)),
        "max": round(sorted[sorted.len() - 1]),
    });
    if let Some(bound) = long_frame_ms {
        summary["fps"] = json!(round(1000.0 / mean));
        summary["long"] = json!(sorted.iter().filter(|value| **value > bound).count());
    }
    summary
}

fn perf_json(perf: &Perf, args: &PerfArgs) -> Value {
    let window = args.window_ms;
    let recent = args.recent.unwrap_or(DEFAULT_RECENT).min(MAX_RECENT);
    let long = args.long_frame_ms.unwrap_or(LONG_FRAME_MS);
    let values = |samples: &[[f32; 2]]| -> Vec<f32> {
        samples
            .iter()
            .filter(|[age, _]| window.is_none_or(|window| f64::from(*age) <= window))
            .map(|[_, value]| *value)
            .collect()
    };
    let series = [
        ("frame_ms", &perf.frame, true),
        ("update_ms", &perf.update, false),
        ("game_update_ms", &perf.game_update, false),
        ("scene_frame_ms", &perf.scene, true),
    ];
    let mut report = json!({
        "window_ms": window,
        "long_frame_ms": long,
        "newest_age_ms": perf.frame.last().or(perf.scene.last()).map(|[age, _]| age),
    });
    let mut latest = serde_json::Map::new();
    for (name, samples, cadence) in series {
        let values = values(samples);
        report[name] = stats(&values, cadence.then_some(long));
        latest.insert(
            name.into(),
            json!(values[values.len().saturating_sub(recent)..]),
        );
    }
    report["recent"] = latest.into();
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perf_stats_use_nearest_rank_percentiles_and_the_window() {
        let perf = Perf {
            frame: (1..=100)
                .map(|value| [(100 - value) as f32, value as f32])
                .collect(),
            ..Perf::default()
        };
        let all = perf_json(
            &perf,
            &PerfArgs {
                window_ms: None,
                recent: Some(3),
                long_frame_ms: Some(95.0),
            },
        );
        assert_eq!(all["frame_ms"]["count"], 100);
        assert_eq!(all["frame_ms"]["p50"], 50.0);
        assert_eq!(all["frame_ms"]["p99"], 99.0);
        assert_eq!(all["frame_ms"]["max"], 100.0);
        assert_eq!(all["frame_ms"]["long"], 5);
        assert_eq!(all["recent"]["frame_ms"], json!([98.0, 99.0, 100.0]));
        assert_eq!(all["game_update_ms"], Value::Null);
        // Ages 0..=9 are the last ten samples, 91 to 100.
        let recent = perf_json(
            &perf,
            &PerfArgs {
                window_ms: Some(9.0),
                recent: None,
                long_frame_ms: None,
            },
        );
        assert_eq!(recent["frame_ms"]["count"], 10);
        assert_eq!(recent["frame_ms"]["min"], 91.0);
    }

    #[test]
    fn components_name_their_group_and_lag() {
        let child = AnimationComponent {
            id: 2,
            last_update_frame: 90,
            ..child_like()
        };
        let parent = AnimationComponent {
            id: 1,
            last_update_frame: 100,
            children: vec![child],
            ..child_like()
        };
        let value = component_json(&parent, 100);
        assert_eq!(value["group"], "first_person");
        assert_eq!(value["frames_behind"], 0);
        assert_eq!(value["children"][0]["frames_behind"], 10);
    }

    fn child_like() -> AnimationComponent {
        AnimationComponent {
            id: 0,
            group: 5,
            owner: 7,
            last_update_frame: 0,
            initialized: true,
            applied: None,
            read_age_ms: 0,
            read_us: 0,
            players: Vec::new(),
            children: Vec::new(),
            detail: None,
        }
    }
}
