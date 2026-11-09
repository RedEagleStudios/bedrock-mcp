//! Scripted cameras: keyframes become `/camera @s set minecraft:free` commands, each segment
//! eased by the game, issued on wall-clock time from a background thread.

use std::{
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    error::{Result, bail},
    world::{World, number},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Easing {
    Linear,
    EaseIn,
    EaseOut,
    #[default]
    EaseInOut,
    Step,
}

impl Easing {
    /// The Bedrock ease type; `None` for step (no ease, issued at the keyframe's time).
    pub fn bedrock(self) -> Option<&'static str> {
        match self {
            Self::Linear => Some("linear"),
            Self::EaseIn => Some("in_cubic"),
            Self::EaseOut => Some("out_cubic"),
            // A smoothstep ease; in_out_sine is Bedrock's closest curve.
            Self::EaseInOut => Some("in_out_sine"),
            Self::Step => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Keyframe {
    pub t: f64,
    pub position: [f64; 3],
    pub yaw: f64,
    pub pitch: f64,
    #[serde(default)]
    pub fov: Option<f64>,
    #[serde(default)]
    pub easing: Option<Easing>,
}

/// Commands to issue `at` seconds after the path (or loop iteration) starts.
#[derive(Clone, Debug, PartialEq)]
pub struct Cue {
    pub at: f64,
    pub commands: Vec<String>,
}

/// The cues for `keyframes` and the path's duration. An eased keyframe is issued when the
/// previous one is reached, easing over the gap so it arrives on time; a step keyframe (and
/// the first) is issued at its own time without easing.
pub fn schedule(keyframes: &[Keyframe], default: Easing) -> Result<(Vec<Cue>, f64)> {
    if keyframes.is_empty() {
        bail!("camera_play needs at least one keyframe.");
    }
    if keyframes
        .iter()
        .any(|frame| !frame.t.is_finite() || frame.t < 0.0)
    {
        bail!("Keyframe times must be finite and not negative.");
    }
    let mut frames = keyframes.to_vec();
    frames.sort_by(|a, b| a.t.total_cmp(&b.t));
    let origin = frames[0].t;
    let mut cues = Vec::with_capacity(frames.len());
    for (index, frame) in frames.iter().enumerate() {
        let previous = index.checked_sub(1).map(|previous| &frames[previous]);
        let gap = previous.map_or(0.0, |previous| frame.t - previous.t);
        let ease = match frame.easing.unwrap_or(default).bedrock() {
            Some(kind) if gap > 0.0 => Some((gap, kind)),
            _ => None,
        };
        let at = match (previous, ease) {
            (Some(previous), Some(_)) => previous.t - origin,
            _ => frame.t - origin,
        };
        let eased = ease
            .map(|(seconds, kind)| format!(" ease {} {kind}", number(seconds)))
            .unwrap_or_default();
        let [x, y, z] = frame.position.map(number);
        let mut commands = vec![format!(
            "camera @s set minecraft:free{eased} pos {x} {y} {z} rot {} {}",
            number(frame.pitch),
            number(frame.yaw)
        )];
        if let Some(fov) = frame.fov {
            let eased = ease
                .map(|(seconds, kind)| format!(" {} {kind}", number(seconds)))
                .unwrap_or_default();
            commands.push(format!("camera @s fov_set {}{eased}", number(fov)));
        }
        cues.push(Cue { at, commands });
    }
    let duration = frames.last().map_or(0.0, |last| last.t - origin);
    Ok((cues, duration))
}

#[derive(Clone, Debug, Default)]
struct Progress {
    finished: bool,
    error: Option<String>,
    loops: u64,
}

/// A running camera path.
pub struct CameraPath {
    stop: mpsc::Sender<()>,
    thread: Option<thread::JoinHandle<()>>,
    progress: Arc<Mutex<Progress>>,
    keyframes: usize,
    duration: f64,
    looping: bool,
    hide_hud: bool,
    uses_fov: bool,
    started: Instant,
}

impl CameraPath {
    pub fn start(
        world: World,
        keyframes: &[Keyframe],
        easing: Easing,
        looping: bool,
        hide_hud: bool,
    ) -> Result<Self> {
        let (cues, duration) = schedule(keyframes, easing)?;
        if hide_hud {
            world.command("hud @s hide")?;
        }
        let (stop, stopped) = mpsc::channel();
        let progress = Arc::new(Mutex::new(Progress::default()));
        let thread_progress = Arc::clone(&progress);
        let thread = thread::Builder::new()
            .name("camera-path".into())
            .spawn(move || {
                let outcome = play(&world, &cues, duration, looping, &stopped, &thread_progress);
                let mut progress = thread_progress
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                progress.finished = true;
                progress.error = outcome.err().map(|error| error.to_string());
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
            progress,
            keyframes: keyframes.len(),
            duration,
            looping,
            hide_hud,
            uses_fov: keyframes.iter().any(|frame| frame.fov.is_some()),
            started: Instant::now(),
        })
    }

    pub fn finished(&self) -> bool {
        self.progress
            .lock()
            .map(|progress| progress.finished)
            .unwrap_or(true)
    }

    pub fn status(&self) -> Value {
        let progress = self
            .progress
            .lock()
            .map(|progress| progress.clone())
            .unwrap_or_default();
        json!({
            "keyframes": self.keyframes,
            "duration_seconds": self.duration,
            "loop": self.looping,
            "loops_completed": progress.loops,
            "elapsed_seconds": (self.started.elapsed().as_secs_f64() * 100.0).round() / 100.0,
            "finished": progress.finished,
            "error": progress.error,
        })
    }

    /// Stop the path and hand the camera back to the player.
    pub fn release(mut self, world: &World) -> Result<()> {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let mut commands = vec!["camera @s clear"];
        if self.uses_fov {
            commands.push("camera @s fov_clear");
        }
        if self.hide_hud {
            commands.push("hud @s reset");
        }
        let mut first_error = None;
        for command in commands {
            if let Err(error) = world.command(command) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

/// Wait until `deadline`; false when asked to stop.
fn wait_until(deadline: Instant, stopped: &mpsc::Receiver<()>) -> bool {
    let remaining = deadline.saturating_duration_since(Instant::now());
    matches!(
        stopped.recv_timeout(remaining),
        Err(mpsc::RecvTimeoutError::Timeout)
    )
}

fn play(
    world: &World,
    cues: &[Cue],
    duration: f64,
    looping: bool,
    stopped: &mpsc::Receiver<()>,
    progress: &Mutex<Progress>,
) -> Result<()> {
    loop {
        let start = Instant::now();
        for cue in cues {
            if !wait_until(start + Duration::from_secs_f64(cue.at), stopped) {
                return Ok(());
            }
            for command in &cue.commands {
                world.command(command)?;
            }
        }
        if !wait_until(start + Duration::from_secs_f64(duration), stopped) {
            return Ok(());
        }
        if let Ok(mut progress) = progress.lock() {
            progress.loops += 1;
        }
        // A zero-length path would spin; it is a fixed camera.
        if !looping || duration <= 0.0 {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(t: f64, x: f64, easing: Option<Easing>, fov: Option<f64>) -> Keyframe {
        Keyframe {
            t,
            position: [x, 64.0, 0.5],
            yaw: 90.0,
            pitch: 10.0,
            fov,
            easing,
        }
    }

    #[test]
    fn eased_keyframes_start_when_the_previous_one_is_reached() {
        let frames = [
            frame(3.0, 2.0, None, Some(50.0)),
            frame(1.0, 0.0, None, None),
            frame(5.0, 4.0, Some(Easing::Linear), None),
        ];
        let (cues, duration) = schedule(&frames, Easing::EaseInOut).unwrap();
        assert_eq!(duration, 4.0);
        assert_eq!(
            cues,
            [
                Cue {
                    at: 0.0,
                    commands: vec!["camera @s set minecraft:free pos 0 64 0.5 rot 10 90".into()]
                },
                Cue {
                    at: 0.0,
                    commands: vec![
                        "camera @s set minecraft:free ease 2 in_out_sine pos 2 64 0.5 rot 10 90"
                            .into(),
                        "camera @s fov_set 50 2 in_out_sine".into(),
                    ],
                },
                Cue {
                    at: 2.0,
                    commands: vec![
                        "camera @s set minecraft:free ease 2 linear pos 4 64 0.5 rot 10 90".into()
                    ]
                },
            ]
        );
    }

    #[test]
    fn step_keyframes_cut_at_their_own_time() {
        let frames = [
            frame(0.0, 0.0, None, None),
            frame(1.5, 1.0, None, Some(30.0)),
        ];
        let (cues, _) = schedule(&frames, Easing::Step).unwrap();
        assert_eq!(cues[1].at, 1.5);
        assert_eq!(
            cues[1].commands,
            [
                "camera @s set minecraft:free pos 1 64 0.5 rot 10 90",
                "camera @s fov_set 30"
            ]
        );
    }

    #[test]
    fn easing_names_map_to_bedrock() {
        let names: Vec<_> = [
            Easing::Linear,
            Easing::EaseIn,
            Easing::EaseOut,
            Easing::EaseInOut,
            Easing::Step,
        ]
        .map(Easing::bedrock)
        .into();
        assert_eq!(
            names,
            [
                Some("linear"),
                Some("in_cubic"),
                Some("out_cubic"),
                Some("in_out_sine"),
                None
            ]
        );
        let parsed: Easing = serde_json::from_value(json!("ease_in_out")).unwrap();
        assert_eq!(parsed, Easing::EaseInOut);
    }

    #[test]
    fn empty_or_negative_paths_are_refused() {
        assert!(schedule(&[], Easing::Linear).is_err());
        assert!(schedule(&[frame(-1.0, 0.0, None, None)], Easing::Linear).is_err());
    }
}
