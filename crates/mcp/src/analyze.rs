//! Finding sudden jumps in a recording: one-frame pops, teleports and snaps in animations.
//!
//! The video is decoded small and grey, cropped to a region, and each frame is compared with
//! the next. A transition whose change is far above the typical change of the surrounding
//! frames is flagged; neighbouring flags form one event. Every sudden change is flagged, not
//! only bugs: an attack swing is sudden too. The events are ranked and each is drawn as a sheet
//! of full-resolution frames around it, so a person can judge which one is wrong.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    error::{Error, Result, bail},
    rpc::Content,
};

/// Width the frames are compared at; small enough to ignore texture noise.
const ANALYSIS_WIDTH: u32 = 256;
/// Frames on each side whose median change counts as typical.
const WINDOW: usize = 15;
/// A flagged change is at least this many grey levels on average.
const MIN_CHANGE: f32 = 6.0;
/// The typical change is never taken below this, so a still scene is not over-sensitive.
const TYPICAL_FLOOR: f32 = 0.5;
const DEFAULT_THRESHOLD: f32 = 4.0;
/// Flags this close together are one event.
const EVENT_GAP: usize = 2;
const DEFAULT_EVENTS: usize = 8;
const DEFAULT_INLINE: usize = 3;
/// Frames shown before and after an event's flagged transitions.
const CONTEXT: usize = 2;
const SHEET_COLUMNS: usize = 4;
const SHEET_MAX_FRAMES: usize = 12;
const SHEET_FRAME_WIDTH: u32 = 400;
const GRAPH_HEIGHT: u32 = 200;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalyzeArgs {
    path: PathBuf,
    region: Option<Rect>,
    threshold: Option<f32>,
    max_events: Option<usize>,
    inline: Option<usize>,
}

/// The default region: where a first-person held item is drawn, right of centre and above the
/// hotbar.
fn held_item(width: u32, height: u32) -> Rect {
    let top = height * 35 / 100;
    Rect {
        x: width / 2,
        y: top,
        width: width - width / 2,
        height: height * 88 / 100 - top,
    }
}

/// A run of flagged transitions. Transition `i` goes from frame `i` to frame `i + 1`.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub first: usize,
    pub last: usize,
    /// The transition with the largest change.
    pub peak: usize,
    pub change: f32,
    pub typical: f32,
}

fn median(values: &[f32]) -> f32 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f32::total_cmp);
    sorted[sorted.len() / 2]
}

/// The typical change around each transition: the median of its neighbourhood.
pub fn typical_changes(changes: &[f32]) -> Vec<f32> {
    (0..changes.len())
        .map(|i| {
            let start = i.saturating_sub(WINDOW);
            let end = (i + WINDOW + 1).min(changes.len());
            median(&changes[start..end]).max(TYPICAL_FLOOR)
        })
        .collect()
}

/// Group transitions whose change exceeds `threshold` times the typical change into events,
/// largest change first.
pub fn find_events(changes: &[f32], threshold: f32) -> Vec<Event> {
    let typical = typical_changes(changes);
    let mut events: Vec<Event> = Vec::new();
    for (i, (&change, &usual)) in changes.iter().zip(&typical).enumerate() {
        if change < MIN_CHANGE || change < threshold * usual {
            continue;
        }
        match events.last_mut() {
            Some(event) if i - event.last <= EVENT_GAP => {
                event.last = i;
                if change > event.change {
                    event.peak = i;
                    event.change = change;
                    event.typical = usual;
                }
            }
            _ => events.push(Event {
                first: i,
                last: i,
                peak: i,
                change,
                typical: usual,
            }),
        }
    }
    events.sort_by(|a, b| b.change.total_cmp(&a.change));
    events
}

/// Frames to draw for `event` in a video of `frames` frames: a whole number of sheet rows.
pub fn sheet_frames(event: &Event, frames: usize) -> (usize, usize) {
    let mut start = event.first.saturating_sub(CONTEXT);
    let mut end = (event.last + 1 + CONTEXT).min(frames - 1);
    let mut count = (end - start + 1).min(SHEET_MAX_FRAMES);
    if count < end - start + 1 {
        // A long event: centre the sheet on its peak.
        start = (event.peak + 1).saturating_sub(count / 2);
        end = (start + count - 1).min(frames - 1);
    }
    let rounded = count.div_ceil(SHEET_COLUMNS) * SHEET_COLUMNS;
    while count < rounded && end + 1 < frames {
        end += 1;
        count += 1;
    }
    while count < rounded && start > 0 {
        start -= 1;
        count += 1;
    }
    (start, end)
}

struct VideoInfo {
    width: u32,
    height: u32,
    fps: f64,
}

fn probe(path: &Path) -> Result<VideoInfo> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,r_frame_rate",
            "-of",
            "json",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| Error::msg(format!("Cannot run ffprobe: {error}")))?;
    let info: Value = serde_json::from_slice(&output.stdout).unwrap_or_default();
    let stream = &info["streams"][0];
    let rate = stream["r_frame_rate"].as_str().unwrap_or_default();
    let fps = match rate.split_once('/') {
        Some((n, d)) => n.parse::<f64>().unwrap_or(0.0) / d.parse::<f64>().unwrap_or(1.0),
        None => rate.parse().unwrap_or(0.0),
    };
    match (stream["width"].as_u64(), stream["height"].as_u64()) {
        (Some(width), Some(height)) if fps > 0.0 => Ok(VideoInfo {
            width: width as u32,
            height: height as u32,
            fps,
        }),
        _ => bail!(
            "{} has no readable video stream: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

/// Decode `region` of every frame as small grey images; returns the frames and their size.
fn decode(path: &Path, region: Rect) -> Result<(Vec<u8>, usize)> {
    let width = ANALYSIS_WIDTH.min(region.width);
    let height =
        ((u64::from(region.height) * u64::from(width) / u64::from(region.width)) as u32).max(1);
    let filter = format!(
        "crop={}:{}:{}:{},scale={width}:{height},format=gray",
        region.width, region.height, region.x, region.y
    );
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args([
            "-vf",
            &filter,
            "-fps_mode",
            "passthrough",
            "-f",
            "rawvideo",
            "-",
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| Error::msg(format!("Cannot run ffmpeg: {error}")))?;
    if !output.status.success() {
        bail!(
            "ffmpeg could not decode {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok((output.stdout, (width * height) as usize))
}

/// Mean absolute difference between each frame and the next, in grey levels.
pub fn changes(pixels: &[u8], frame_bytes: usize) -> Vec<f32> {
    let frames: Vec<&[u8]> = pixels.chunks_exact(frame_bytes).collect();
    frames
        .windows(2)
        .map(|pair| {
            let total: u64 = pair[0]
                .iter()
                .zip(pair[1])
                .map(|(a, b)| u64::from(a.abs_diff(*b)))
                .sum();
            total as f32 / frame_bytes as f32
        })
        .collect()
}

/// Draw frames `start..=end` of `region` with their numbers into a PNG sheet at `out`.
fn draw_sheet(video: &Path, region: Rect, (start, end): (usize, usize), out: &Path) -> Result<()> {
    let rows = (end - start + 1).div_ceil(SHEET_COLUMNS);
    let filter = format!(
        "select='between(n\\,{start}\\,{end})',crop={}:{}:{}:{},scale={SHEET_FRAME_WIDTH}:-2,\
         drawtext=text='%{{eif\\:n+{start}\\:d}}':x=6:y=6:fontsize=24:fontcolor=yellow:box=1:boxcolor=black,\
         tile={SHEET_COLUMNS}x{rows}:padding=4:color=white",
        region.width, region.height, region.x, region.y
    );
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y", "-i"])
        .arg(video)
        .args(["-vf", &filter, "-frames:v", "1", "-fps_mode", "passthrough"])
        .arg(out)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| Error::msg(format!("Cannot run ffmpeg: {error}")))?;
    if !output.status.success() || !out.exists() {
        bail!(
            "ffmpeg could not draw frames {start}-{end}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// A bar per transition: flagged ones red, the rest blue, with the flag line in grey.
pub fn graph(changes: &[f32], threshold: f32, events: &[Event]) -> Result<Vec<u8>> {
    // Up to three columns a transition, so short clips stay readable.
    let width = (changes.len() * 3).clamp(1, 1600) as u32;
    let height = GRAPH_HEIGHT;
    let typical = typical_changes(changes);
    let top = changes.iter().copied().fold(MIN_CHANGE * 2.0, f32::max);
    let mut pixels = vec![24u8; (width * height * 3) as usize];
    let flagged = |i: usize| {
        events
            .iter()
            .any(|event| (event.first..=event.last).contains(&i))
    };
    let level = |value: f32| height - ((value / top).min(1.0) * (height - 1) as f32) as u32 - 1;
    for x in 0..width {
        let i = (x as usize * changes.len()) / width as usize;
        let color = if flagged(i) {
            [230, 60, 60]
        } else {
            [80, 150, 230]
        };
        for y in level(changes[i])..height {
            let at = ((y * width + x) * 3) as usize;
            pixels[at..at + 3].copy_from_slice(&color);
        }
        let line = level((threshold * typical[i]).max(MIN_CHANGE));
        let at = ((line * width + x) * 3) as usize;
        pixels[at..at + 3].copy_from_slice(&[170, 170, 170]);
    }
    let mut png = Vec::new();
    let mut encoder = png::Encoder::new(&mut png, width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .and_then(|mut writer| writer.write_image_data(&pixels))
        .map_err(|error| Error::msg(format!("Cannot encode the motion graph: {error}")))?;
    Ok(png)
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// Analyse the recording at `args.path`.
pub fn run(args: AnalyzeArgs) -> Result<Vec<Content>> {
    let info = probe(&args.path)?;
    let region = args
        .region
        .unwrap_or_else(|| held_item(info.width, info.height));
    if region.width < 2
        || region.height < 2
        || region.x + region.width > info.width
        || region.y + region.height > info.height
    {
        bail!(
            "region {}x{} at {},{} is outside the {}x{} video.",
            region.width,
            region.height,
            region.x,
            region.y,
            info.width,
            info.height
        );
    }
    let threshold = args.threshold.unwrap_or(DEFAULT_THRESHOLD);
    if threshold <= 1.0 {
        bail!("threshold must be above 1.");
    }
    let (pixels, frame_bytes) = decode(&args.path, region)?;
    let frames = pixels.len() / frame_bytes;
    if frames < 3 {
        bail!("{} has too few frames to analyse.", args.path.display());
    }
    let changes = changes(&pixels, frame_bytes);
    let all_events = find_events(&changes, threshold);
    let events: Vec<&Event> = all_events
        .iter()
        .take(args.max_events.unwrap_or(DEFAULT_EVENTS))
        .collect();
    let stem = args.path.with_extension("");
    let graph_path = PathBuf::from(format!("{}.motion.png", stem.display()));
    let graph_png = graph(&changes, threshold, &all_events)?;
    fs::write(&graph_path, &graph_png)?;
    let inline = args.inline.unwrap_or(DEFAULT_INLINE);
    let mut images = vec![Content::Png(graph_png)];
    let mut reports = Vec::new();
    for (rank, event) in events.iter().enumerate() {
        let span = sheet_frames(event, frames);
        let sheet = PathBuf::from(format!("{}.event-{}.png", stem.display(), rank + 1));
        let drawn = draw_sheet(&args.path, region, span, &sheet);
        if drawn.is_ok() && rank < inline {
            images.push(Content::Png(fs::read(&sheet)?));
        }
        reports.push(json!({
            "rank": rank + 1,
            "frames": [event.first, event.last + 1],
            "jump": {
                "from": event.peak,
                "to": event.peak + 1,
                "change": round2(f64::from(event.change)),
                "typical_change": round2(f64::from(event.typical)),
                "ratio": round2(f64::from(event.change / event.typical)),
            },
            "at_seconds": round2((event.peak + 1) as f64 / info.fps),
            "sheet": drawn.map(|()| json!(sheet)).unwrap_or_else(|error| json!({ "error": error.to_string() })),
            "sheet_frames": [span.0, span.1],
        }));
    }
    let report = json!({
        "video": args.path,
        "frames": frames,
        "fps": round2(info.fps),
        "region": region_json(region),
        "threshold": threshold,
        "median_change": round2(f64::from(median(&changes))),
        "event_count": all_events.len(),
        "events": reports,
        "motion_graph": graph_path,
        "note": "Every sudden change is flagged, intended ones too (swings, item switches). Frame numbers count from 0; each sheet shows the frames around one event with their numbers. Images: the motion graph (red bars are flagged), then the top events' sheets.",
    });
    let mut content = vec![Content::json(&report)];
    content.extend(images);
    Ok(content)
}

fn region_json(region: Rect) -> Value {
    json!({ "x": region.x, "y": region.y, "width": region.width, "height": region.height })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Quiet motion with small noise.
    fn quiet(frames: usize) -> Vec<f32> {
        (0..frames).map(|i| 1.0 + (i % 3) as f32 * 0.3).collect()
    }

    #[test]
    fn a_single_frame_jump_is_flagged_and_noise_is_not() {
        let mut changes = quiet(120);
        changes[60] = 30.0;
        let events = find_events(&changes, DEFAULT_THRESHOLD);
        assert_eq!(events.len(), 1);
        assert_eq!(
            (events[0].first, events[0].peak, events[0].last),
            (60, 60, 60)
        );
        assert!(find_events(&quiet(120), DEFAULT_THRESHOLD).is_empty());
    }

    #[test]
    fn nearby_jumps_form_one_event_ranked_by_size() {
        let mut changes = quiet(200);
        changes[40] = 12.0;
        changes[42] = 25.0;
        changes[150] = 40.0;
        let events = find_events(&changes, DEFAULT_THRESHOLD);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].peak, 150, "largest first");
        assert_eq!(
            (events[1].first, events[1].last, events[1].peak),
            (40, 42, 42)
        );
    }

    #[test]
    fn small_changes_stay_unflagged_even_in_a_still_scene() {
        let mut changes = vec![0.0; 100];
        changes[50] = MIN_CHANGE - 1.0;
        assert!(find_events(&changes, DEFAULT_THRESHOLD).is_empty());
    }

    #[test]
    fn a_jump_inside_steady_fast_motion_needs_to_stand_out() {
        let mut changes = vec![10.0; 100];
        changes[50] = 30.0;
        assert!(find_events(&changes, DEFAULT_THRESHOLD).is_empty());
        changes[50] = 45.0;
        assert_eq!(find_events(&changes, DEFAULT_THRESHOLD).len(), 1);
    }

    #[test]
    fn sheets_cover_the_event_in_whole_rows_inside_the_video() {
        let event = |first, last, peak| Event {
            first,
            last,
            peak,
            change: 1.0,
            typical: 1.0,
        };
        // Transition 113 -> 114: frames 111..=116, padded to two rows.
        assert_eq!(sheet_frames(&event(113, 113, 113), 300), (111, 118));
        assert_eq!(sheet_frames(&event(0, 0, 0), 300), (0, 3));
        assert_eq!(sheet_frames(&event(298, 298, 298), 300), (296, 299));
        let (start, end) = sheet_frames(&event(10, 60, 40), 300);
        assert_eq!(end - start + 1, SHEET_MAX_FRAMES);
        assert!(
            (start..=end).contains(&41),
            "a long event is centred on its peak"
        );
    }

    #[test]
    fn changes_measure_the_mean_difference_between_neighbours() {
        let pixels = [0u8, 0, 10, 10, 10, 30];
        assert_eq!(changes(&pixels, 2), vec![10.0, 10.0]);
    }

    #[test]
    fn the_default_region_is_the_held_item_area() {
        assert_eq!(
            held_item(2522, 1360),
            Rect {
                x: 1261,
                y: 476,
                width: 1261,
                height: 720
            }
        );
    }
}
