//! Recording the frames the agent copies out of the game into an MP4.
//!
//! The agent copies presented frames into a shared-memory [`FrameFile`] at up to the requested
//! rate and records every present's time. A worker thread turns them into a constant-rate
//! video: each output tick shows the newest frame presented before it, so a stall appears as
//! repeated frames, exactly as a player would have seen it. `ffmpeg` encodes on the GPU (the
//! platform picks the encoder) while the platform records the default output's audio.
//! Stopping reports the game's frame pacing and writes every present time next to the video.
//!
//! The session tells a recording when tool calls start and end ([`Recording::call_started`]).
//! Each call becomes a chapter at its place in the video, and with a pause threshold the
//! encoder leaves out the frames (and the stop leaves out the audio) of the time no call ran.

use std::{
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, Sender, SyncSender, TrySendError, channel, sync_channel},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use protocol::frames::{PRESENT_RING, format, state};
use serde_json::{Value, json};

use crate::{
    clock::{Clock, poll},
    error::{Error, Result, bail},
    frame_file::{FrameFile, FrameInfo},
    platform::{self, AudioCapture, VideoEncoder},
};

const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
const FINISH_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the worker keeps reading after a stop for the agent's last frames.
const DRAIN: Duration = Duration::from_secs(2);
const IDLE: Duration = Duration::from_millis(2);
/// Hitches listed in a report.
const MAX_HITCHES: usize = 50;

/// The `ffmpeg` arguments that encode a `width` × `height` raw stream in agent `format`
/// with `encoder`.
pub fn video_args(
    encoder: &VideoEncoder,
    shape: (u32, u32, u32),
    fps: u32,
    path: &Path,
) -> Result<Vec<String>> {
    let (width, height, pixel_format) = shape;
    let (input, convert) = match pixel_format {
        format::BGRA8 => ("bgra", ""),
        format::RGBA8 => ("rgba", ""),
        format::RGB10A2 => ("x2bgr10le", "format=bgr0,"),
        other => bail!("The agent sent frames in unknown format {other}."),
    };
    let mut args: Vec<String> = ["-hide_banner", "-loglevel", "warning", "-nostdin", "-y"]
        .map(str::to_owned)
        .into();
    args.extend(encoder.global.iter().cloned());
    args.extend(
        [
            "-f",
            "rawvideo",
            "-pixel_format",
            input,
            "-video_size",
            &format!("{width}x{height}"),
            "-framerate",
            &fps.to_string(),
            "-i",
            "pipe:0",
            "-vf",
            &format!("{convert}{}", encoder.upload),
        ]
        .map(str::to_owned),
    );
    args.extend(encoder.codec.iter().cloned());
    args.push(path.to_string_lossy().into_owned());
    Ok(args)
}

/// The `ffmpeg` arguments that join `video` and `audio` into `path`, the audio through
/// `audio_codec`. `audio_lead` is how long audio capture started before the first video
/// frame; negative when it started after. `cuts` are spans in seconds from the first video
/// frame the video left out; the audio leaves them out too, which needs a re-encode.
pub fn mux_args(
    video: &Path,
    audio: &Path,
    audio_codec: &[&str],
    audio_lead: f64,
    cuts: &[(f64, f64)],
    path: &Path,
) -> Vec<String> {
    let mut args: Vec<String> = [
        "-hide_banner",
        "-loglevel",
        "warning",
        "-nostdin",
        "-y",
        "-i",
    ]
    .map(str::to_owned)
    .into();
    args.push(video.to_string_lossy().into_owned());
    if audio_lead >= 0.0 {
        args.extend(["-ss".to_owned(), format!("{audio_lead:.3}")]);
    } else {
        args.extend(["-itsoffset".to_owned(), format!("{:.3}", -audio_lead)]);
    }
    args.push("-i".into());
    args.push(audio.to_string_lossy().into_owned());
    args.extend(["-map", "0:v", "-map", "1:a"].map(str::to_owned));
    if cuts.is_empty() {
        args.extend(audio_codec.iter().map(|arg| (*arg).to_owned()));
    } else {
        let spans: Vec<String> = cuts
            .iter()
            .map(|(from, to)| format!("between(t,{from:.3},{to:.3})"))
            .collect();
        let filter = format!("aselect='not({})',asetpts=N/SR/TB", spans.join("+"));
        args.extend(
            [
                "-c:v", "copy", "-af", &filter, "-c:a", "aac", "-b:a", "192k",
            ]
            .map(str::to_owned),
        );
    }
    args.extend(["-shortest", "-movflags", "+faststart"].map(str::to_owned));
    args.push(path.to_string_lossy().into_owned());
    args
}

/// Frame pacing over present times `presents` (nanoseconds, ascending); `video_seconds`
/// places a present in the video.
///
/// A hitch is a frame shown more than twice as long as the median frame.
pub fn pacing(presents: &[u64], video_seconds: impl Fn(u64) -> f64) -> Value {
    let intervals: Vec<(u64, f64)> = presents
        .windows(2)
        .map(|pair| (pair[1], pair[1].saturating_sub(pair[0]) as f64 / 1e6))
        .collect();
    if intervals.is_empty() {
        return json!({ "presents": presents.len() });
    }
    let mut sorted: Vec<f64> = intervals.iter().map(|(_, ms)| *ms).collect();
    sorted.sort_by(f64::total_cmp);
    let percentile = |p: f64| sorted[((sorted.len() - 1) as f64 * p).round() as usize];
    let median = percentile(0.5);
    let round = |ms: f64| (ms * 100.0).round() / 100.0;
    let hitches: Vec<(u64, f64)> = intervals
        .iter()
        .copied()
        .filter(|(_, ms)| *ms > median * 2.0)
        .collect();
    let span = (presents[presents.len() - 1] - presents[0]) as f64 / 1e9;
    json!({
        "presents": presents.len(),
        "average_fps": round(intervals.len() as f64 / span.max(1e-9)),
        "frame_ms": {
            "median": round(median),
            "p95": round(percentile(0.95)),
            "p99": round(percentile(0.99)),
            "max": round(sorted[sorted.len() - 1]),
        },
        "hitch_count": hitches.len(),
        "hitches": hitches.iter().take(MAX_HITCHES).map(|(at, ms)| json!({
            "at_seconds": round(video_seconds(*at)),
            "frame_ms": round(*ms),
        })).collect::<Vec<_>>(),
    })
}

/// A stretch of the recording the encoder left out because no tool call ran.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pause {
    /// Where the cut is in the video.
    pub video_seconds: f64,
    pub from: Instant,
    /// None while the pause lasts.
    pub to: Option<Instant>,
    /// The present time of the first frame left out, and of the first one shown again.
    pub present_from: u64,
    pub present_to: Option<u64>,
}

/// Where things land in a video with `pauses` cut out of it.
#[derive(Clone, Debug, Default)]
pub struct VideoClock {
    /// When the first frame reached the encoder, for wall times.
    pub started: Option<Instant>,
    /// The present time of video time 0, for present times.
    pub start_ns: u64,
    pub pauses: Vec<Pause>,
}

impl VideoClock {
    /// Video seconds at wall time `at`; a time inside a pause lands on its cut. None before
    /// the first frame.
    pub fn at(&self, at: Instant) -> Option<f64> {
        let mut anchor = (self.started?, 0.0);
        for pause in &self.pauses {
            if at < pause.from {
                break;
            }
            match pause.to {
                Some(to) if at >= to => anchor = (to, pause.video_seconds),
                _ => return Some(pause.video_seconds),
            }
        }
        Some(anchor.1 + at.saturating_duration_since(anchor.0).as_secs_f64())
    }

    /// Video seconds of the frame presented at `present_ns`.
    pub fn at_present(&self, present_ns: u64) -> f64 {
        let mut anchor = (self.start_ns, 0.0);
        for pause in &self.pauses {
            if present_ns < pause.present_from {
                break;
            }
            match pause.present_to {
                Some(to) if present_ns >= to => anchor = (to, pause.video_seconds),
                _ => return pause.video_seconds,
            }
        }
        anchor.1 + present_ns.saturating_sub(anchor.0) as f64 / 1e9
    }

    /// The pauses in seconds from wall time `origin`, the open one ending at `end`.
    pub fn cuts(&self, origin: Instant, end: Instant) -> Vec<(f64, f64)> {
        let seconds = |at: Instant| at.saturating_duration_since(origin).as_secs_f64();
        self.pauses
            .iter()
            .map(|pause| (seconds(pause.from), seconds(pause.to.unwrap_or(end))))
            .collect()
    }
}

/// Tool-call activity, shared between the session and the encoder.
#[derive(Default)]
struct Activity {
    /// Leave frames out after this long without a tool call.
    pause_after: Option<Duration>,
    /// Tool calls and background runs in progress.
    busy: u32,
    /// When the last call ended (or the recording started).
    last_call: Option<Instant>,
    clock: VideoClock,
}

impl Activity {
    fn idle(&self, now: Instant) -> bool {
        match (self.pause_after, self.last_call) {
            (Some(after), Some(last)) => {
                self.busy == 0 && now.saturating_duration_since(last) >= after
            }
            _ => false,
        }
    }
}

/// A chapter of the finished video.
#[derive(Clone, Debug, PartialEq)]
pub struct Chapter {
    pub start: f64,
    pub end: f64,
    pub title: String,
}

/// Chapters from `marks` (video seconds, title) in order, each lasting until the next; the
/// last ends at `duration`. Marks at or past the end are dropped.
pub fn chapters(marks: &[(f64, String)], duration: f64) -> Vec<Chapter> {
    let kept: Vec<&(f64, String)> = marks.iter().filter(|(at, _)| *at < duration).collect();
    kept.iter()
        .enumerate()
        .map(|(index, (at, title))| Chapter {
            start: at.max(0.0),
            end: kept.get(index + 1).map_or(duration, |next| next.0.max(0.0)),
            title: title.clone(),
        })
        .collect()
}

/// An FFMETADATA file that gives a video `chapters`, in milliseconds.
pub fn ffmetadata(chapters: &[Chapter]) -> String {
    let escape = |text: &str| {
        let mut escaped = String::with_capacity(text.len());
        for character in text.chars() {
            if matches!(character, '=' | ';' | '#' | '\\' | '\n') {
                escaped.push('\\');
            }
            escaped.push(character);
        }
        escaped
    };
    let mut text = String::from(";FFMETADATA1\n");
    for chapter in chapters {
        text.push_str(&format!(
            "[CHAPTER]\nTIMEBASE=1/1000\nSTART={}\nEND={}\ntitle={}\n",
            (chapter.start * 1000.0).round() as u64,
            (chapter.end * 1000.0).round() as u64,
            escape(&chapter.title)
        ));
    }
    text
}

/// The `ffmpeg` arguments that copy `video` into `path` with the chapters in `metadata`.
fn chapter_args(video: &Path, metadata: &Path, path: &Path) -> Vec<String> {
    let mut args: Vec<String> = [
        "-hide_banner",
        "-loglevel",
        "warning",
        "-nostdin",
        "-y",
        "-i",
    ]
    .map(str::to_owned)
    .into();
    args.push(video.to_string_lossy().into_owned());
    args.push("-i".into());
    args.push(metadata.to_string_lossy().into_owned());
    args.extend(
        [
            "-map",
            "0",
            "-map_metadata",
            "1",
            "-map_chapters",
            "1",
            "-c",
            "copy",
            "-movflags",
            "+faststart",
        ]
        .map(str::to_owned),
    );
    args.push(path.to_string_lossy().into_owned());
    args
}

/// Frames copied out of the file and waiting for the encoder.
const QUEUE: usize = 4;

/// What the reader saw.
#[derive(Default)]
struct ReaderTotals {
    /// Frames overwritten in the file before the reader got to them.
    missed: u64,
    /// Frames dropped because the encoder was behind.
    encoder_behind: u64,
    /// Frames the agent skipped because no copy buffer was free.
    skipped: u64,
    presents: Vec<u64>,
}

/// What the encoder wrote.
#[derive(Default)]
struct EncoderTotals {
    captured: u64,
    ticks: u64,
    repeated_ticks: u64,
    start_ns: u64,
    /// When the first video frame reached ffmpeg.
    video_started: Option<Instant>,
    captured_presents: Vec<u64>,
    /// Frames left out while paused.
    paused_frames: u64,
}

struct Frame {
    info: FrameInfo,
    pixels: Vec<u8>,
}

/// Copies frames out of the file in order and never waits for the encoder: when it falls
/// behind it jumps to the newest frame, and when the encoder is behind it drops frames.
struct Reader {
    file: FrameFile,
    stop: Arc<AtomicBool>,
    frames: SyncSender<Frame>,
    free: Receiver<Vec<u8>>,
    spare: Vec<Vec<u8>>,
    allocated: usize,
    totals: ReaderTotals,
}

impl Reader {
    fn buffer(&mut self) -> Option<Vec<u8>> {
        if let Some(buffer) = self.spare.pop().or_else(|| self.free.try_recv().ok()) {
            return Some(buffer);
        }
        // The queue, the encoder's current frame and the one it is replacing.
        (self.allocated < QUEUE + 2).then(|| {
            self.allocated += 1;
            Vec::with_capacity(self.file.frame_bytes())
        })
    }

    fn run(mut self) -> ReaderTotals {
        let mut next = self.file.written().saturating_sub(1);
        let mut presents_read = 0u64;
        let mut stopped_at: Option<Instant> = None;
        loop {
            let written = self.file.written();
            if written > next {
                // The oldest slots are about to be overwritten; skip to the newest frame.
                if written - next > self.file.slots() / 2 {
                    self.totals.missed += written - 1 - next;
                    next = written - 1;
                }
                match self.buffer() {
                    None => self.totals.encoder_behind += 1,
                    Some(mut pixels) => match self.file.read(next, &mut pixels) {
                        None => {
                            self.totals.missed += 1;
                            self.spare.push(pixels);
                        }
                        Some(info) => match self.frames.try_send(Frame { info, pixels }) {
                            Ok(()) => {}
                            Err(TrySendError::Full(frame)) => {
                                self.totals.encoder_behind += 1;
                                self.spare.push(frame.pixels);
                            }
                            Err(TrySendError::Disconnected(_)) => break,
                        },
                    },
                }
                next += 1;
                continue;
            }
            let presents = self.file.presents();
            for n in presents_read.max(presents.saturating_sub(PRESENT_RING as u64))..presents {
                self.totals.presents.push(self.file.present_ns(n));
            }
            presents_read = presents;
            if self.stop.load(Ordering::Acquire) {
                let since = *stopped_at.get_or_insert_with(Instant::now);
                if self.file.state() != state::RUNNING || since.elapsed() > DRAIN {
                    break;
                }
            }
            std::thread::sleep(IDLE);
        }
        self.totals.skipped = self.file.dropped();
        self.totals
    }
}

/// Turns frames into a constant-rate stream: each tick shows the newest frame presented
/// before it.
struct Encoder {
    stdin: File,
    period_ns: u64,
    frames: Receiver<Frame>,
    free: Sender<Vec<u8>>,
    totals: EncoderTotals,
    activity: Arc<Mutex<Activity>>,
}

fn lock(activity: &Mutex<Activity>) -> std::sync::MutexGuard<'_, Activity> {
    activity
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Encoder {
    fn write(&mut self, frame: &Frame, repeated: bool) -> std::io::Result<()> {
        self.stdin.write_all(&frame.pixels)?;
        self.totals.ticks += 1;
        if repeated {
            self.totals.repeated_ticks += 1;
        }
        Ok(())
    }

    fn take(&mut self, frame: &Frame) {
        self.totals.captured += 1;
        self.totals.captured_presents.push(frame.info.present);
    }

    fn run(mut self) -> std::io::Result<EncoderTotals> {
        let Ok(first) = self.frames.recv() else {
            return Ok(self.totals);
        };
        // ffmpeg reads nothing until it has started, so this write waits for it. The frames
        // that queued meanwhile are stale: the timeline starts at the newest of them.
        self.take(&first);
        self.write(&first, false)?;
        self.totals.video_started = Some(Instant::now());
        lock(&self.activity).clock.started = self.totals.video_started;
        let mut current = first;
        while let Ok(frame) = self.frames.try_recv() {
            let _ = self
                .free
                .send(std::mem::replace(&mut current, frame).pixels);
        }
        if current.info.present != self.totals.captured_presents[0] {
            self.take(&current);
        }
        // Ticks fall half a period after the frame they start from. A game presenting at the
        // video rate then lands each frame mid-interval, where present-time jitter cannot make
        // a tick show one frame twice and skip the next.
        let mut tick = current.info.present_ns + self.period_ns / 2;
        self.totals.start_ns = tick.saturating_sub(self.period_ns);
        lock(&self.activity).clock.start_ns = self.totals.start_ns;
        let mut shown = false;
        let mut paused = false;
        while let Ok(frame) = self.frames.recv() {
            let now = Instant::now();
            let idle = {
                let mut activity = lock(&self.activity);
                // Frames queued while ffmpeg started were presented before video_started, so
                // video time 0 lies earlier on the wall clock. Each frame was presented
                // before the encoder got it, so the earliest estimate is the closest.
                if activity.clock.pauses.is_empty() {
                    let into_video = frame.info.present_ns.saturating_sub(self.totals.start_ns);
                    if let Some(anchor) = now.checked_sub(Duration::from_nanos(into_video)) {
                        let started = activity.clock.started.get_or_insert(anchor);
                        *started = (*started).min(anchor);
                    }
                }
                activity.idle(now)
            };
            if !paused {
                while tick < frame.info.present_ns {
                    let repeated = shown;
                    self.write(&current, repeated)?;
                    shown = true;
                    tick += self.period_ns;
                }
            }
            if idle {
                if !paused {
                    // Cut here: the video holds everything presented before this frame.
                    paused = true;
                    let video_seconds = self.totals.ticks as f64 * self.period_ns as f64 / 1e9;
                    lock(&self.activity).clock.pauses.push(Pause {
                        video_seconds,
                        from: now,
                        to: None,
                        present_from: frame.info.present_ns,
                        present_to: None,
                    });
                }
                self.totals.paused_frames += 1;
                let _ = self.free.send(frame.pixels);
                continue;
            }
            if paused {
                paused = false;
                if let Some(pause) = lock(&self.activity).clock.pauses.last_mut() {
                    pause.to = Some(now);
                    pause.present_to = Some(frame.info.present_ns);
                }
                tick = frame.info.present_ns + self.period_ns / 2;
            }
            self.take(&frame);
            let _ = self
                .free
                .send(std::mem::replace(&mut current, frame).pixels);
            shown = false;
        }
        // Show the last frame for one tick.
        self.write(&current, shown)?;
        Ok(self.totals)
    }
}

/// Everything one recording saw.
struct Totals {
    reader: ReaderTotals,
    encoder: EncoderTotals,
}

/// Read and encode until the reader stops, then close ffmpeg's input.
fn record(
    file: FrameFile,
    stdin: File,
    period_ns: u64,
    stop: Arc<AtomicBool>,
    activity: Arc<Mutex<Activity>>,
) -> Result<Totals> {
    let (frames, receiver) = sync_channel(QUEUE);
    let (free, free_receiver) = channel();
    let reader = Reader {
        file,
        stop,
        frames,
        free: free_receiver,
        spare: Vec::new(),
        allocated: 0,
        totals: ReaderTotals::default(),
    };
    let reader = std::thread::Builder::new()
        .name("bedrock-frame-reader".into())
        .spawn(move || reader.run())
        .map_err(|error| Error::msg(format!("Cannot start the frame reader: {error}")))?;
    let encoder = Encoder {
        stdin,
        period_ns,
        frames: receiver,
        free,
        totals: EncoderTotals::default(),
        activity,
    }
    .run();
    let reader = reader
        .join()
        .map_err(|_| Error::msg("The frame reader panicked."))?;
    Ok(Totals {
        reader,
        encoder: encoder?,
    })
}

/// A running recording.
pub struct Recording {
    pub path: PathBuf,
    pub log: PathBuf,
    /// Where the video ffmpeg writes: `path`, or a temporary file when audio is muxed in.
    video: PathBuf,
    frames: PathBuf,
    fps: u32,
    child: Child,
    audio: Option<AudioCapture>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<Totals>>>,
    started: Instant,
    activity: Arc<Mutex<Activity>>,
    /// Chapter starts (wall time) and titles, in order.
    marks: Vec<(Instant, String)>,
    pub shape: (u32, u32),
    /// The hidden-window frame rate to restore when the recording ends (Hyprland only).
    pub restore_unfocused_fps: Option<i64>,
}

/// An `ffmpeg` command with `args` that logs to `log`.
pub fn ffmpeg(args: &[String], log: &Path) -> Result<Command> {
    let mut command = Command::new("ffmpeg");
    command
        .args(args)
        .stdout(Stdio::null())
        .stderr(File::create(log)?);
    Ok(command)
}

impl Recording {
    /// Record the frames the agent writes to `frames` into `path`, once the first one arrives.
    /// With `pause_after`, frames are left out while no tool call ran for that long.
    pub fn start(
        frames: PathBuf,
        path: PathBuf,
        fps: u32,
        codec: &str,
        audio: bool,
        pause_after: Option<Duration>,
        clock: &dyn Clock,
    ) -> Result<Self> {
        let file = poll(
            clock,
            FIRST_FRAME_TIMEOUT,
            Duration::from_millis(20),
            || FrameFile::open(&frames),
        )?
        .ok_or_else(|| {
            Error::msg("The game presented no frames to record within 5 s; is it rendering?")
        })?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let video = if audio {
            path.with_extension("video.mp4")
        } else {
            path.clone()
        };
        let args = video_args(
            &platform::video_encoder(codec)?,
            (file.width, file.height, file.format),
            fps,
            &video,
        )?;
        let log = path.with_extension("log");
        let (mut child, stdin) = platform::spawn_piped(&mut ffmpeg(&args, &log)?)
            .map_err(|error| Error::msg(format!("Cannot run ffmpeg: {error}")))?;
        let audio = if audio {
            match AudioCapture::start(&path) {
                Ok(audio) => Some(audio),
                Err(error) => {
                    let _ = child.kill();
                    return Err(error);
                }
            }
        } else {
            None
        };
        let stop = Arc::new(AtomicBool::new(false));
        let shape = (file.width, file.height);
        let period_ns = 1_000_000_000 / u64::from(fps);
        let reader_stop = Arc::clone(&stop);
        let activity = Arc::new(Mutex::new(Activity {
            pause_after,
            last_call: Some(Instant::now()),
            ..Activity::default()
        }));
        let encoder_activity = Arc::clone(&activity);
        let worker = std::thread::Builder::new()
            .name("bedrock-recorder".into())
            .spawn(move || record(file, stdin, period_ns, reader_stop, encoder_activity))
            .map_err(|error| Error::msg(format!("Cannot start the recorder: {error}")))?;
        Ok(Self {
            path,
            log,
            video,
            frames,
            fps,
            child,
            audio,
            stop,
            worker: Some(worker),
            started: clock.now(),
            activity,
            marks: Vec::new(),
            shape,
            restore_unfocused_fps: None,
        })
    }

    pub fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// The frame file the agent copies into while this records.
    pub fn frames(&self) -> &Path {
        &self.frames
    }

    pub fn status(&mut self, clock: &dyn Clock) -> Value {
        json!({
            "path": self.path,
            "running": self.running(),
            "elapsed_seconds": round2(clock.now().duration_since(self.started).as_secs_f64()),
            "video_seconds": self.video_seconds().map(round2),
            "chapters": self.marks.len(),
        })
    }

    /// A tool call (or background run) started: the recording stops pausing until it ends.
    pub fn call_started(&self) {
        let mut activity = lock(&self.activity);
        activity.busy += 1;
        activity.last_call = Some(Instant::now());
    }

    /// A tool call ended; the idle time before a pause counts from now. A call that started
    /// before the recording did may end here too, so the count never goes below zero.
    pub fn call_finished(&self) {
        let mut activity = lock(&self.activity);
        activity.busy = activity.busy.saturating_sub(1);
        activity.last_call = Some(Instant::now());
    }

    /// Where now falls in the video, in seconds; None before the first frame.
    pub fn video_seconds(&self) -> Option<f64> {
        lock(&self.activity).clock.at(Instant::now())
    }

    /// Start a chapter titled `title` at real time `at` (an [`Instant::now`] reading). Calls
    /// may be marked when they end, so marks need not arrive in order.
    pub fn mark_at(&mut self, at: Instant, title: String) {
        self.marks.push((at, title));
    }

    /// Stop the audio capture and join it to the video at `self.path`, leaving out `cuts`;
    /// on failure keep the video alone and say why.
    fn add_audio(
        &mut self,
        video_started: Option<Instant>,
        cuts: &[(f64, f64)],
        clock: &dyn Clock,
    ) -> Option<String> {
        let mut audio = self.audio.take()?;
        let finished = audio.finish(clock, FINISH_TIMEOUT);
        let lead = video_started
            .map(|video| {
                video
                    .saturating_duration_since(audio.started())
                    .as_secs_f64()
            })
            .unwrap_or_default();
        let failure = if finished {
            let args = mux_args(
                &self.video,
                audio.path(),
                AudioCapture::MUX_CODEC,
                lead,
                cuts,
                &self.path,
            );
            match Command::new("ffmpeg")
                .args(args)
                .stdin(Stdio::null())
                .output()
            {
                Ok(output) if output.status.success() => None,
                Ok(output) => Some(String::from_utf8_lossy(&output.stderr).trim().to_owned()),
                Err(error) => Some(error.to_string()),
            }
        } else {
            Some("the audio capture did not finish".to_owned())
        };
        let _ = fs::remove_file(audio.path());
        match failure {
            None => {
                let _ = fs::remove_file(&self.video);
                None
            }
            Some(reason) => {
                let _ = fs::rename(&self.video, &self.path);
                Some(format!("The video has no audio: {reason}"))
            }
        }
    }

    /// Give the finished video `chapters` (copying its streams into a new file under the
    /// same name) and write them to `<video>.chapters.json`.
    fn add_chapters(&self, chapters: &[Chapter]) -> Result<PathBuf> {
        let listing = self.path.with_extension("chapters.json");
        fs::write(
            &listing,
            serde_json::to_vec_pretty(&json!({
                "chapters": chapters.iter().map(|chapter| json!({
                    "start_seconds": round3(chapter.start),
                    "end_seconds": round3(chapter.end),
                    "title": chapter.title,
                })).collect::<Vec<_>>(),
            }))?,
        )?;
        let metadata = self.path.with_extension("chapters.txt");
        let copy = self.path.with_extension("chapters.mp4");
        fs::write(&metadata, ffmetadata(chapters))?;
        let output = Command::new("ffmpeg")
            .args(chapter_args(&self.path, &metadata, &copy))
            .stdin(Stdio::null())
            .output();
        let _ = fs::remove_file(&metadata);
        let failure = match output {
            Ok(output) if output.status.success() => None,
            Ok(output) => Some(String::from_utf8_lossy(&output.stderr).trim().to_owned()),
            Err(error) => Some(error.to_string()),
        };
        if let Some(reason) = failure {
            let _ = fs::remove_file(&copy);
            bail!(
                "The video has no chapters ({reason}); they are in {}.",
                listing.display()
            );
        }
        fs::rename(&copy, &self.path)?;
        Ok(listing)
    }

    /// Finish the video once the agent has stopped copying frames, and report its pacing.
    pub fn stop(mut self, clock: &dyn Clock) -> Result<Value> {
        let elapsed = clock.now().duration_since(self.started).as_secs_f64();
        self.stop.store(true, Ordering::Release);
        let totals = self
            .worker
            .take()
            .expect("joined only here")
            .join()
            .map_err(|_| Error::msg("The recorder thread panicked."))?;
        // The encoder dropped ffmpeg's stdin, which ends the video.
        let finished = poll(clock, FINISH_TIMEOUT, Duration::from_millis(100), || {
            Ok(self.child.try_wait()?)
        })?;
        let _ = fs::remove_file(&self.frames);
        if finished.is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
            bail!("ffmpeg did not finish the file: {}", log_tail(&self.log));
        }
        let totals = totals
            .map_err(|error| Error::msg(format!("{error}; ffmpeg: {}", log_tail(&self.log))))?;
        let Totals { reader, encoder } = totals;
        let video_clock = lock(&self.activity).clock.clone();
        let cuts = video_clock.cuts(video_clock.started.unwrap_or(self.started), Instant::now());
        // The audio is placed against the moment the first frame reached ffmpeg.
        let audio_cuts = encoder
            .video_started
            .map(|origin| video_clock.cuts(origin, Instant::now()))
            .unwrap_or_default();
        let audio_note = self.add_audio(encoder.video_started, &audio_cuts, clock);
        let start = encoder.start_ns;
        let pacing = pacing(&reader.presents, |present| video_clock.at_present(present));
        let timing = self.path.with_extension("frames.json");
        let ms = |ns: u64| (ns.saturating_sub(start) as f64 / 1e4).round() / 100.0;
        let presents_ms: Vec<f64> = reader.presents.iter().map(|ns| ms(*ns)).collect();
        let paused_ms: Vec<[Option<f64>; 2]> = video_clock
            .pauses
            .iter()
            .map(|pause| [Some(ms(pause.present_from)), pause.present_to.map(ms)])
            .collect();
        fs::write(
            &timing,
            serde_json::to_vec(&json!({
                "fps": self.fps,
                "size": [self.shape.0, self.shape.1],
                "note": "presents_ms: every frame the game presented, in ms from the first recorded frame; captured: which presents (by index) are in the video; paused_ms: [from, to] present times left out while no tool call ran (to is null when the recording stopped paused)",
                "presents_ms": presents_ms,
                "captured": encoder.captured_presents,
                "paused_ms": paused_ms,
            }))?,
        )?;
        let duration = encoder.ticks as f64 / f64::from(self.fps);
        self.marks.sort_by_key(|(at, _)| *at);
        let marks: Vec<(f64, String)> = self
            .marks
            .iter()
            .filter_map(|(at, title)| Some((video_clock.at(*at)?, title.clone())))
            .collect();
        let chapters = chapters(&marks, duration);
        let mut notes: Vec<String> = audio_note.into_iter().collect();
        let chapter_file = if chapters.is_empty() {
            None
        } else {
            self.add_chapters(&chapters)
                .map_err(|error| notes.push(error.to_string()))
                .ok()
        };
        let paused: Vec<Value> = video_clock
            .pauses
            .iter()
            .zip(&cuts)
            .map(|(pause, (from, to))| {
                json!({
                    "at_seconds": round2(pause.video_seconds),
                    "after_seconds": round2(*from),
                    "duration_seconds": round2(to - from),
                })
            })
            .collect();
        let mut reply = json!({
            "path": self.path,
            "log": self.log,
            "frame_times": timing,
            "elapsed_seconds": round2(elapsed),
            "fps": self.fps,
            "size": [self.shape.0, self.shape.1],
            "captured_frames": encoder.captured,
            "missed_frames": reader.missed,
            "encoder_behind_frames": reader.encoder_behind,
            "skipped_frames": reader.skipped,
            "video_frames": encoder.ticks,
            "repeated_frames": encoder.repeated_ticks,
            "pacing": pacing,
            "chapters": chapters.len(),
            "chapters_file": chapter_file,
        });
        if !paused.is_empty() {
            reply["paused"] = json!(paused);
            reply["paused_seconds"] = json!(round2(cuts.iter().map(|(from, to)| to - from).sum()));
            reply["paused_frames"] = json!(encoder.paused_frames);
        }
        if !notes.is_empty() {
            reply["note"] = json!(notes.join(" "));
        }
        match probe(&self.path) {
            Ok((frames, duration)) => {
                reply["encoded_frames"] = json!(frames);
                reply["duration_seconds"] = json!((duration * 1000.0).round() / 1000.0);
            }
            Err(error) => reply["probe_error"] = json!(error.to_string()),
        }
        Ok(reply)
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        if self.worker.is_some() {
            self.stop.store(true, Ordering::Release);
            let _ = self.child.kill();
            let _ = fs::remove_file(&self.frames);
        }
        if let Some(audio) = self.audio.as_mut() {
            audio.kill();
        }
    }
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

fn log_tail(log: &Path) -> String {
    let text = fs::read_to_string(log).unwrap_or_default();
    let start = text.len().saturating_sub(400);
    let start = (start..text.len())
        .find(|&index| text.is_char_boundary(index))
        .unwrap_or(text.len());
    text[start..].trim().to_owned()
}

/// Video frame count and duration from ffprobe.
fn probe(path: &Path) -> Result<(u64, f64)> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-count_packets",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=nb_read_packets:format=duration",
            "-of",
            "json",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| Error::msg(format!("Cannot run ffprobe: {error}")))?;
    let info: Value = serde_json::from_slice(&output.stdout).map_err(|_| {
        Error::msg(format!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    })?;
    let frames = info["streams"][0]["nb_read_packets"]
        .as_str()
        .and_then(|n| n.parse().ok());
    let duration = info["format"]["duration"]
        .as_str()
        .and_then(|d| d.parse().ok());
    match (frames, duration) {
        (Some(frames), Some(duration)) => Ok((frames, duration)),
        _ => bail!(
            "ffprobe reported no video stream: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pacing_flags_frames_longer_than_twice_the_median() {
        let ms = 1_000_000u64;
        // 16 ms frames with one 40 ms stall ending at 80 ms and one 32 ms frame (exactly 2x).
        let presents = [0, 16, 32, 72, 88, 120, 136].map(|t| 1_000 * ms + t * ms);
        let report = pacing(&presents, |present| (present - 1_000 * ms) as f64 / 1e9);
        assert_eq!(report["frame_ms"]["median"], 16.0);
        assert_eq!(report["frame_ms"]["max"], 40.0);
        assert_eq!(report["hitch_count"], 1);
        assert_eq!(
            report["hitches"][0],
            json!({"at_seconds": 0.07, "frame_ms": 40.0})
        );
    }

    #[test]
    fn pacing_needs_two_presents() {
        assert_eq!(pacing(&[5], |_| 0.0), json!({"presents": 1}));
    }

    #[test]
    fn ffmpeg_reads_raw_frames_in_the_agent_format() {
        let encoder = VideoEncoder {
            global: Vec::new(),
            upload: "format=nv12".into(),
            codec: ["-c:v", "libx264"].map(str::to_owned).into(),
        };
        let args = video_args(
            &encoder,
            (1280, 720, format::BGRA8),
            60,
            Path::new("/a.mp4"),
        )
        .unwrap()
        .join(" ");
        assert!(args.contains(
            "-pixel_format bgra -video_size 1280x720 -framerate 60 -i pipe:0 -vf format=nv12 -c:v libx264 /a.mp4"
        ));
        assert!(video_args(&encoder, (1, 1, 9), 60, Path::new("/x")).is_err());
    }

    #[test]
    fn audio_is_aligned_to_the_first_video_frame() {
        let mux = |lead: f64| {
            mux_args(
                Path::new("/v.mp4"),
                Path::new("/a.m4a"),
                &["-c", "copy"],
                lead,
                &[],
                Path::new("/o.mp4"),
            )
            .join(" ")
        };
        // Audio that started first is trimmed; audio that started later is delayed.
        assert!(mux(0.25).contains("-ss 0.250 -i /a.m4a"));
        assert!(mux(-0.1).contains("-itsoffset 0.100 -i /a.m4a"));
        assert!(mux(0.0).contains("-map 1:a -c copy -shortest"));
    }

    #[test]
    fn paused_spans_are_cut_from_the_audio_too() {
        let args = mux_args(
            Path::new("/v.mp4"),
            Path::new("/a.m4a"),
            &["-c", "copy"],
            0.0,
            &[(2.0, 5.5), (8.0, 9.25)],
            Path::new("/o.mp4"),
        )
        .join(" ");
        assert!(args.contains(
            "-c:v copy -af aselect='not(between(t,2.000,5.500)+between(t,8.000,9.250))',asetpts=N/SR/TB -c:a aac"
        ));
    }

    #[test]
    fn times_after_a_pause_continue_from_its_cut() {
        let started = Instant::now();
        let s = |seconds: f64| started + Duration::from_secs_f64(seconds);
        let ns = |seconds: f64| 1_000_000_000 + (seconds * 1e9) as u64;
        let clock = VideoClock {
            started: Some(started),
            start_ns: ns(0.0),
            // The video ran 3 s, then nothing was recorded from 3 s to 10 s.
            pauses: vec![Pause {
                video_seconds: 3.0,
                from: s(3.0),
                to: Some(s(10.0)),
                present_from: ns(3.0),
                present_to: Some(ns(10.0)),
            }],
        };
        let close = |a: Option<f64>, b: f64| assert!((a.unwrap() - b).abs() < 1e-6, "{a:?} != {b}");
        close(clock.at(s(1.5)), 1.5);
        close(clock.at(s(6.0)), 3.0);
        close(clock.at(s(12.0)), 5.0);
        close(Some(clock.at_present(ns(1.5))), 1.5);
        close(Some(clock.at_present(ns(6.0))), 3.0);
        close(Some(clock.at_present(ns(12.0))), 5.0);
        let cuts = clock.cuts(started, s(20.0));
        assert_eq!(cuts.len(), 1);
        close(Some(cuts[0].0), 3.0);
        close(Some(cuts[0].1), 10.0);
        assert_eq!(VideoClock::default().at(started), None);
    }

    #[test]
    fn chapters_run_until_the_next_one_and_are_written_as_ffmetadata() {
        let marks = [
            (-0.2, "video_start".to_owned()),
            (1.5, "attack -326417514479 (tick 445260)".to_owned()),
            (4.0, "say a=b; #1 \\ done".to_owned()),
            (9.0, "video_stop".to_owned()),
        ];
        let chapters = chapters(&marks, 6.0);
        assert_eq!(
            chapters,
            [
                Chapter {
                    start: 0.0,
                    end: 1.5,
                    title: marks[0].1.clone()
                },
                Chapter {
                    start: 1.5,
                    end: 4.0,
                    title: marks[1].1.clone()
                },
                Chapter {
                    start: 4.0,
                    end: 6.0,
                    title: marks[2].1.clone()
                },
            ]
        );
        assert_eq!(
            ffmetadata(&chapters[1..]),
            ";FFMETADATA1\n\
             [CHAPTER]\nTIMEBASE=1/1000\nSTART=1500\nEND=4000\ntitle=attack -326417514479 (tick 445260)\n\
             [CHAPTER]\nTIMEBASE=1/1000\nSTART=4000\nEND=6000\ntitle=say a\\=b\\; \\#1 \\\\ done\n"
        );
        let args = chapter_args(
            Path::new("/v.mp4"),
            Path::new("/m.txt"),
            Path::new("/o.mp4"),
        );
        assert!(
            args.join(" ")
                .contains("-i /v.mp4 -i /m.txt -map 0 -map_metadata 1 -map_chapters 1 -c copy")
        );
    }
}
