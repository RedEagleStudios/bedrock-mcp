//! The `media` toolset: screenshots, recordings and scripted cameras.

use serde_json::json;

use super::{
    Handler, Hints, Tool, Toolset, arguments, reply,
    schema::{describe, easing, object, region},
};

pub fn tools() -> Vec<Tool> {
    let tool = |hints: Hints, title: &'static str, run: Handler, definition| {
        Tool::new(Toolset::Media, hints, title, run, definition)
    };
    let keyframe = object(
        json!({
            "t": { "type": "number", "description": "Seconds after the path starts (wall-clock time)" },
            "position": { "type": "array", "items": { "type": "number" }, "minItems": 3, "maxItems": 3 },
            "yaw": { "type": "number", "description": "Bedrock yaw in degrees (0 faces +Z/south, 90 faces -X/west)" },
            "pitch": { "type": "number", "description": "Degrees; positive looks down" },
            "fov": { "type": "number", "description": "Field of view in degrees; omitted keeps the player's" },
            "easing": easing()
        }),
        &["t", "position", "yaw", "pitch"],
    );
    vec![
        tool(
            Hints::READ,
            "Capture a frame",
            |s, a| s.screenshot(arguments(a)?),
            json!({
                "name": "frame_capture",
                "description": concat!(
                    "Use this to see the game screen, or with ocr to read its text.\n\n",
                    "Saves the game window's own buffer as a full-size PNG and returns it inline, scaled to max_width. ",
                    "It works while the window is hidden or covered (not minimized) and never shows other windows.\n\n",
                    "- Captured with grim -T on Hyprland, elsewhere from the newest frame the agent copies from the game.\n",
                    "- OCR matches the game's pixel fonts (Mojangles, Minecraft Seven/Ten/Five) from the installation, so it reads UI text exactly ",
                    "but not world signs, icons, textures, vector-font text (Noto, non-Latin scripts) or other overlays.\n",
                    "- ocr with inline: false costs far fewer tokens than an image; take an image for layout, colours or anything visual.\n",
                    "- Without the game fonts, ocr holds an error and the capture still succeeds.\n\n",
                    "Returns {path, size, logical_size, inline_scale, ocr?} and the image; ",
                    "ocr is {lines: [{text, x, y, width, height, confidence, engine}], ms} in logical pixels, top to bottom."
                ),
                "inputSchema": object(json!({
                    "path": { "type": "string", "description": "Where to save the PNG (default under the agent folder's screenshots/)" },
                    "inline": { "type": "boolean", "description": "Return the image (default true)" },
                    "max_width": { "type": "integer", "minimum": 1, "description": "Widest the inline image may be, in physical pixels (default 1280); the saved PNG stays full size" },
                    "ocr": { "type": "boolean", "description": "Also return the text lines (default false)" },
                    "region": describe(region(), "Only read text inside this rectangle, in logical pixels; needs ocr")
                }), &[])
            }),
        ),
        tool(
            Hints::ACT,
            "Start recording",
            |s, a| reply(s.record_start(arguments(a)?)?),
            json!({
                "name": "video_start",
                "description": concat!(
                    "Use this to record a test you will watch afterwards; for a fight in one action_sequence, pass record: true there instead.\n\n",
                    "Records the game's own frames to MP4, copied out of the game (Direct3D 12) on the GPU. ",
                    "It works while the window is hidden or covered, never shows other windows, and returns once the first frame arrives.\n\n",
                    "- Each video frame is the newest frame the game had presented, so stalls show as repeated frames.\n",
                    "- game_fps equal to fps puts every rendered frame in the video once, for frame-by-frame animation checks; ",
                    "leave the game uncapped to check behaviour that depends on frame rate.\n",
                    "- Linux encodes with VAAPI and PulseAudio; Windows with the first ffmpeg encoder that works (NVENC, AMF, Quick Sync, Media Foundation, x264, x265) and WASAPI loopback.\n",
                    "- On Hyprland it raises misc:render_unfocused_fps for the recording and restores it after.\n",
                    "- Each tool call becomes a chapter titled with the tool, short arguments and game tick, e.g. \"attack -326417514479 (tick 445260)\".\n",
                    "- pause_on_idle_ms cuts frames and audio between calls; a running call or background action_sequence never pauses it.\n",
                    "- Fails when a recording is already running.\n\n",
                    "Returns {recording, path, log, fps, game_fps, size, codec, pause_on_idle_ms, notes}; log is ffmpeg's log file."
                ),
                "inputSchema": object(json!({
                    "path": { "type": "string", "description": "Output .mp4 (default under the agent folder's recordings/)" },
                    "fps": { "type": "integer", "minimum": 1, "maximum": 240, "description": "Video frame rate (default 60)" },
                    "codec": { "enum": ["h264", "hevc"], "description": "Video codec (default h264)" },
                    "audio": { "type": "boolean", "description": "Also record the default audio output into the video (default true)" },
                    "game_fps": { "type": "integer", "minimum": 1, "maximum": 240, "description": "Hold the game to this many frames a second while recording (default: uncapped)" },
                    "pause_on_idle_ms": { "type": "integer", "minimum": 1, "description": "Cut the video while no tool call has run for this many ms, until the next call (default: never cut)" }
                }), &[])
            }),
        ),
        tool(
            Hints::ACT,
            "Stop recording",
            |s, _| reply(s.record_stop()?),
            json!({
                "name": "video_stop",
                "description": concat!(
                    "Use this right after the last recorded action returns.\n\n",
                    "Stops the recording, finishes the MP4 and reports dropped and repeated frames and the game's frame pacing.\n\n",
                    "- Every present time goes to <video>.frames.json.\n",
                    "- A hitch is a frame shown over twice the median frame time.\n",
                    "- Fails when nothing is recording or ffmpeg does not finish the file.\n\n",
                    "Returns {path, log, frame_times, elapsed_seconds, fps, size, captured_frames, missed_frames, encoder_behind_frames, skipped_frames, ",
                    "video_frames, repeated_frames, pacing, chapters, chapters_file, encoded_frames, duration_seconds}: ",
                    "pacing holds frame_ms percentiles and hitches with their time in the video; chapters_file lists the chapters. ",
                    "With pause_on_idle_ms it adds paused (each cut), paused_seconds and paused_frames."
                ),
                "inputSchema": object(json!({}), &[])
            }),
        ),
        tool(
            Hints::READ,
            "Read the session timeline",
            |s, a| reply(s.timeline_read(arguments(a)?)?),
            json!({
                "name": "timeline_read",
                "description": concat!(
                    "Use this to see how long each step of a flow took and where it sits in a recording.\n\n",
                    "Reads the tool calls this server answered, oldest first, from the JSONL file it keeps under the agent folder's timeline/.\n\n",
                    "- Pass next_since back as since to read only newer calls.\n\n",
                    "Returns {path, entries, skipped, next_since}: skipped counts calls limit left out; each entry is ",
                    "{seq, tool, args_summary, started_unix_ms, duration_ms, ok, error_head, tick, recording_offset_s}, ",
                    "with tick and recording_offset_s (where the call starts in the running video) null when unknown."
                ),
                "inputSchema": object(json!({
                    "since": { "type": "integer", "minimum": 0, "description": "Only calls after this seq (default: from the first)" },
                    "limit": { "type": "integer", "minimum": 1, "description": "Most recent entries returned (default 50)" }
                }), &[])
            }),
        ),
        tool(
            Hints::READ,
            "Analyze a recording",
            |_, a| crate::analyze::run(arguments(a)?),
            json!({
                "name": "video_analyze",
                "description": concat!(
                    "Use this after a recording to find animation glitches without watching the whole video.\n\n",
                    "Finds one-frame pops, teleports and snaps: it compares each frame with the next inside region ",
                    "and flags changes far above the typical change of the surrounding quarter second.\n\n",
                    "- Nearby flags form one event; events are ranked by size.\n",
                    "- Intended changes (swings, item switches) are flagged too, so judge each from its sheet.\n",
                    "- Works best with game_fps equal to fps and a still camera.\n",
                    "- The motion graph and every sheet are also saved next to the video.\n\n",
                    "Returns {video, frames, fps, region, threshold, median_change, event_count, events, motion_graph, note}, ",
                    "then the motion graph (red bars flagged) and the top event sheets as images. ",
                    "Each event gives its frames (from 0), jump {from, to, change, typical_change, ratio}, at_seconds and sheet."
                ),
                "inputSchema": object(json!({
                    "path": { "type": "string", "description": "The video, e.g. the path video_stop reported" },
                    "region": describe(object(json!({
                        "x": { "type": "integer", "minimum": 0 },
                        "y": { "type": "integer", "minimum": 0 },
                        "width": { "type": "integer", "minimum": 2 },
                        "height": { "type": "integer", "minimum": 2 }
                    }), &["x", "y", "width", "height"]), "Area to watch, in video pixels (default: the first-person held item, right of centre above the hotbar)"),
                    "threshold": { "type": "number", "exclusiveMinimum": 1, "description": "How many times the typical change a jump must be (default 4); lower finds more" },
                    "max_events": { "type": "integer", "minimum": 1, "description": "Events reported, largest first (default 8)" },
                    "inline": { "type": "integer", "minimum": 0, "description": "Event sheets returned as images (default 3); the rest are only saved" }
                }), &["path"])
            }),
        ),
        tool(
            Hints::ACT,
            "Play a camera path",
            |s, a| reply(s.camera_path(arguments(a)?)?),
            json!({
                "name": "camera_play",
                "description": concat!(
                    "Use this for a scripted camera move in a cinematic or recording.\n\n",
                    "Takes over the camera with /camera free and plays keyframes on wall-clock time, eased by the game.\n\n",
                    "- ease_in, ease_out and ease_in_out are the game's in_cubic, out_cubic and in_out_sine; step cuts at the keyframe.\n",
                    "- A single keyframe holds a fixed camera; a new path replaces a running one.\n",
                    "- Needs a world with cheats.\n\n",
                    "Returns {keyframes, duration_seconds, loop, loops_completed, elapsed_seconds, finished, error}, or {released: true}."
                ),
                "inputSchema": object(json!({
                    "keyframes": { "type": "array", "items": keyframe, "description": "At least one keyframe, played in order of t; the path starts at the first one. Needed unless release is true" },
                    "easing": { "enum": ["linear", "ease_in", "ease_out", "ease_in_out", "step"], "description": "Easing for keyframes that set none (default ease_in_out)" },
                    "loop": { "type": "boolean", "description": "Start the path over each time it ends, until release (default false)" },
                    "hide_hand": { "type": "boolean", "description": "Hide the HUD with /hud while scripted (default true; restored on release)" },
                    "release": { "type": "boolean", "description": "Stop the path and give the camera back to the player; other arguments are ignored (default false)" }
                }), &[])
            }),
        ),
        tool(
            Hints::READ,
            "Read the live camera",
            |s, _| reply(s.camera_get()?),
            json!({
                "name": "camera_get",
                "description": concat!(
                    "Use this to check where the player's camera points and which perspective it shows.\n\n",
                    "Reads the camera through the unstable layer's camera capability (see unstable_status), without the Bridge pack or a WebSocket.\n\n",
                    "- view is the render camera; in third person front it faces the player.\n",
                    "- view.position is null when the camera reports the origin, which may be a camera-relative position rather than a world one; view.fov and view.aspect_ratio may be null too.\n",
                    "- Fails when the camera capability is not active.\n\n",
                    "Returns {perspective, perspective_age_ms, fov, fov_age_ms, view, detail}: perspective is first_person, third_person_back or third_person_front; ",
                    "fov is in degrees as the camera reports it, so it may include effects such as sprinting; view is {position, yaw, pitch, forward, up, fov, aspect_ratio, age_ms}, ",
                    "where yaw 0 faces +Z/south and positive pitch looks down; detail says why a part is missing."
                ),
                "inputSchema": object(json!({}), &[])
            }),
        ),
        tool(
            Hints::ACT,
            "Set the camera perspective",
            |s, a| reply(s.camera_set(arguments(a)?)?),
            json!({
                "name": "camera_set",
                "description": concat!(
                    "Use this to switch between first and third person before a test or recording.\n\n",
                    "Presses the key bound to key.togglePerspective (F5 by default) through the stable agent until the unstable layer's camera ",
                    "capability reads the wanted perspective; two presses reach any of the three.\n\n",
                    "- Fails when the camera capability is not active or has not read the perspective in the last second.\n",
                    "- Fails when a screen covers the HUD, or a press does not change the perspective within 1.5 s.\n\n",
                    "Returns {perspective, previous, presses, camera}, with camera as camera_get reports it."
                ),
                "inputSchema": object(json!({
                    "perspective": { "enum": ["first_person", "third_person_back", "third_person_front"], "description": "The perspective to switch to" }
                }), &["perspective"])
            }),
        ),
    ]
}
