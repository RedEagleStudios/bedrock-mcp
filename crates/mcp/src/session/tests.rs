//! Session behaviour with the unstable layer: live data when it works, the stable
//! behaviour plus a note when it does not.

use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use protocol::unstable::CapabilityState;
use serde_json::{Value, json};

use super::Session;
use crate::{
    agent::{AgentConnection, fake::stable_endpoint},
    clock::fake::FakeClock,
    install::Install,
    rpc::{Content, ToolServer},
    testdir::{TestDir, roaming},
    unstable::fake::{FakeHost, Script, Shared, entry, stack},
};

struct Fixture {
    session: Session,
    script: Shared,
    data: PathBuf,
    _dir: TestDir,
}

/// A session on a scratch prefix with a stable agent attached and a scripted unstable host.
fn setup(script: Script) -> Fixture {
    let dir = TestDir::new("session");
    let prefix = dir.path().join("pfx");
    let data = roaming(&prefix).join("Minecraft Bedrock");
    fs::create_dir_all(data.join("logs")).unwrap();
    let install = Install {
        profile: dir.path().join("profile"),
        prefix,
    };
    let script = Arc::new(Mutex::new(script));
    let host = Box::new(FakeHost(Arc::clone(&script)));
    let mut session = Session::with_unstable_host(install, Arc::new(FakeClock::new()), host);
    session.agent = Some(AgentConnection::open(&stable_endpoint()).unwrap());
    Fixture {
        session,
        script,
        data,
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

fn failing() -> Script {
    let mut script = Script::working();
    script.attach = Err("bedrock_unstable.dll is missing".into());
    script
}

fn notes(reply: &Value) -> String {
    reply["notes"].to_string()
}

#[test]
fn content_log_falls_back_to_the_file_with_a_note() {
    let mut fixture = setup(failing());
    fs::write(
        fixture.data.join("logs/ContentLog__1.txt"),
        "10:00:00[Scripting][error]-boom\n\n",
    )
    .unwrap();
    let reply = call(&mut fixture.session, "content_log_read", json!({}));
    assert_eq!(reply["source"], "file");
    assert_eq!(reply["entries"][0]["message"], "boom");
    assert!(notes(&reply).contains("missing"), "{reply}");
    // A live cursor from an earlier session still answers, from the file's start.
    let reply = call(
        &mut fixture.session,
        "content_log_read",
        json!({"since": "live@9"}),
    );
    assert_eq!(reply["entries"].as_array().unwrap().len(), 1);
    assert!(notes(&reply).contains("live cursor"));
}

#[test]
fn content_log_reads_live_entries_with_live_cursors() {
    let mut script = Script::working();
    script.log = vec![entry(1, 2, "info"), entry(2, 3, "bad")];
    let mut fixture = setup(script);
    fs::write(
        fixture.data.join("logs/ContentLog__1.txt"),
        "10:00:00[A][error]-from file\n\n",
    )
    .unwrap();

    let reply = call(&mut fixture.session, "content_log_read", json!({}));
    assert_eq!(reply["source"], "live");
    assert_eq!(reply["entries"].as_array().unwrap().len(), 2);
    assert_eq!(reply["cursor"], "live@2");
    let reply = call(
        &mut fixture.session,
        "content_log_read",
        json!({"since": "live@1", "levels": ["3"]}),
    );
    assert_eq!(reply["entries"][0]["message"], "bad");
    let reply = call(
        &mut fixture.session,
        "content_log_read",
        json!({"since": "live@2"}),
    );
    assert!(reply["entries"].as_array().unwrap().is_empty());
    assert_eq!(reply["cursor"], "live@2");
    let reply = call(
        &mut fixture.session,
        "content_log_read",
        json!({"since": "live@1", "levels": ["ERROR"]}),
    );
    assert_eq!(reply["entries"][0]["message"], "bad");
    assert_eq!(reply["entries"][0]["level_name"], "error");

    // A file cursor keeps reading the file in auto mode; source file always reads it.
    let reply = call(
        &mut fixture.session,
        "content_log_read",
        json!({"since": "ContentLog__1.txt@0"}),
    );
    assert_eq!(reply["source"], "file");
    let reply = call(
        &mut fixture.session,
        "content_log_read",
        json!({"source": "file"}),
    );
    assert_eq!(reply["entries"][0]["message"], "from file");
    assert!(
        fixture
            .session
            .call(
                "content_log_read",
                json!({"source": "file", "since": "live@1"})
            )
            .is_err()
    );
}

#[test]
fn content_log_survives_a_broken_live_connection() {
    let mut fixture = setup(Script::working());
    fs::write(
        fixture.data.join("logs/ContentLog__1.txt"),
        "10:00:00[A][warning]-kept\n\n",
    )
    .unwrap();
    // Status on load succeeds; the content-log request finds the connection gone.
    fixture.script.lock().unwrap().disconnect_after = Some(1);
    let reply = call(
        &mut fixture.session,
        "content_log_read",
        json!({"source": "live"}),
    );
    assert_eq!(reply["source"], "file");
    assert_eq!(reply["entries"][0]["message"], "kept");
    assert!(notes(&reply).contains("live content log failed"));
    let status = call(&mut fixture.session, "unstable_status", json!({}));
    assert_eq!(status["load"]["state"], "failed");
}

#[test]
fn screen_waits_use_the_top_of_a_stack_and_need_the_screens_capability() {
    let mut script = Script::working();
    script.stacks = vec![stack(&["hud_screen", "pause_screen"])];
    let mut fixture = setup(script);
    let reply = call(
        &mut fixture.session,
        "wait_until",
        json!({"condition": {"screen": {"name": "pause"}}, "timeout_ms": 1000}),
    );
    assert_eq!(reply["detail"]["source"], "unstable");
    assert_eq!(reply["detail"]["screen"], "pause_screen");
    assert!(
        fixture
            .session
            .call(
                "wait_until",
                json!({"condition": {"screen": {"name": "hud"}}, "timeout_ms": 200})
            )
            .is_err()
    );

    // Screens capability not active (in a new game process): the wait fails at once and says
    // why.
    {
        let mut script = fixture.script.lock().unwrap();
        script.capabilities = vec![(
            "screens",
            CapabilityState::Unavailable,
            "no SceneStack class in RTTI",
        )];
        script.pid = Some(1);
    }
    let error = fixture
        .session
        .call(
            "wait_until",
            json!({"condition": {"screen": {"name": "chat"}}, "timeout_ms": 1000}),
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains("no SceneStack class in RTTI"), "{error}");
}

#[test]
fn in_world_waits_say_when_nothing_can_tell() {
    let mut fixture = setup(failing());
    fixture.script.lock().unwrap().pid = Some(1);
    let error = fixture
        .session
        .call(
            "wait_until",
            json!({"condition": "in_world", "timeout_ms": 1000}),
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains("Nothing can tell"), "{error}");
    assert!(error.contains("missing"), "{error}");
}

#[test]
fn content_log_waits_see_live_entries_written_after_the_call() {
    let mut script = Script::working();
    script.log = vec![entry(1, 3, "old")];
    script.arrivals = vec![entry(2, 2, "skip"), entry(3, 3, "new error")];
    let mut fixture = setup(script);
    let reply = call(
        &mut fixture.session,
        "wait_until",
        json!({"condition": {"content_log": {"level": "3"}}, "timeout_ms": 1000}),
    );
    assert_eq!(reply["detail"]["source"], "live");
    assert_eq!(reply["detail"]["entry"]["message"], "new error");
}

#[test]
fn state_reports_screens_when_loaded_and_null_otherwise() {
    let mut script = Script::working();
    script.stacks = vec![stack(&["start_screen"])];
    let mut fixture = setup(script);
    let state = call(&mut fixture.session, "game_status", json!({}));
    assert_eq!(
        state["unstable"]["screens"][0]["screens"],
        json!(["start_screen"])
    );
    assert_eq!(state["unstable"]["capabilities"]["screens"], "active");

    let mut fixture = setup(failing());
    let state = call(&mut fixture.session, "game_status", json!({}));
    assert_eq!(state["unstable"], Value::Null);
}

#[test]
fn unstable_status_explains_the_switch_and_the_load() {
    let mut fixture = setup(Script::working());
    let status = call(&mut fixture.session, "unstable_status", json!({}));
    assert_eq!(status["enabled"], true);
    assert_eq!(status["load"]["state"], "loaded");
    assert_eq!(status["status"]["build"]["version"], "1.0.0.0");

    let mut script = Script::working();
    script.setting = Some("0".into());
    let mut fixture = setup(script);
    let status = call(&mut fixture.session, "unstable_status", json!({}));
    assert_eq!(status["enabled"], false);
    assert_eq!(status["load"]["state"], "not_loaded");
    assert!(
        status["note"]
            .as_str()
            .unwrap()
            .contains("BEDROCK_MCP_UNSTABLE=0")
    );
    assert_eq!(fixture.script.lock().unwrap().attaches, 0);
}

#[test]
fn ui_tree_says_why_the_capability_is_off_before_it_needs_the_game() {
    let mut script = Script::working();
    script.setting = Some("0".into());
    let mut fixture = setup(script);
    let error = fixture
        .session
        .call("ui_tree", json!({}))
        .expect_err("refused")
        .to_string();
    assert!(error.contains("BEDROCK_MCP_UNSTABLE=0"), "{error}");

    let mut fixture = setup(Script::working());
    let error = fixture
        .session
        .call("ui_tree", json!({"view": "tree"}))
        .expect_err("no game")
        .to_string();
    assert!(error.contains("not running"), "{error}");
}

#[test]
fn camera_get_reports_the_live_perspective_and_says_why_it_cannot() {
    let mut fixture = setup(Script::working());
    let reply = call(&mut fixture.session, "camera_get", json!({}));
    assert_eq!(reply["perspective"], "first_person");
    assert_eq!(reply["fov"], 70.0);

    fixture.script.lock().unwrap().capabilities = vec![(
        "camera",
        CapabilityState::Unavailable,
        "VanillaCameraAPI has no RTTI",
    )];
    let mut fixture = setup(fixture.script.lock().unwrap().clone());
    let error = fixture
        .session
        .call("camera_get", json!({}))
        .expect_err("capability off")
        .to_string();
    assert!(error.contains("camera is unavailable"), "{error}");
}

#[test]
fn hud_get_reports_a_death_from_the_camera_with_the_game_message() {
    let mut script = Script::working();
    script.hud.dead = Some(true);
    script.hud.death_ticks = Some(31.5);
    script.hud.chat = vec![protocol::unstable::ChatLine {
        kind: 3,
        username: String::new(),
        message: "Steve died".into(),
        full: "Steve died".into(),
        duration: 7.0,
        seen: true,
    }];
    script.stacks = vec![stack(&[
        "in_game_play_screen",
        "hud_screen",
        "ui://demo/hbui/gameplay.html",
    ])];
    let mut fixture = setup(script);
    let reply = call(&mut fixture.session, "hud_get", json!({}));
    assert_eq!(reply["death"]["shown"], true);
    assert_eq!(reply["death"]["message"], "Steve died");
    assert_eq!(reply["death"]["ticks"], 31.5);
    assert_eq!(reply["death"]["screen"], Value::Null);
    assert_eq!(reply["chat"][0]["kind"], "localized");

    fixture.script.lock().unwrap().hud.dead = Some(false);
    let reply = call(&mut fixture.session, "hud_get", json!({}));
    assert_eq!(reply["death"]["shown"], false);
}

#[test]
fn ocr_arguments_are_checked_before_the_game_is_needed() {
    let mut fixture = setup(Script::working());
    let mut error = |name: &str, arguments: Value| {
        fixture
            .session
            .call(name, arguments)
            .expect_err("refused")
            .to_string()
    };
    let region = json!({"x": 0, "y": 0, "width": 200, "height": 50});
    assert!(error("frame_capture", json!({"region": region})).contains("ocr: true"));
    assert!(error("frame_capture", json!({"ocr": true, "region": {"x": 0}})).contains("region"));
    assert!(
        error(
            "frame_capture",
            json!({"ocr": true, "region": {"x": 0, "y": 0, "width": -5, "height": 5}})
        )
        .contains("positive")
    );
    assert!(error("wait_until", json!({"condition": {"text": {}}})).contains("contains"));
    // Valid arguments get as far as needing the game.
    assert!(
        error(
            "frame_capture",
            json!({"ocr": true, "inline": false, "region": region})
        )
        .contains("not running")
    );
    assert!(
        error(
            "wait_until",
            json!({"condition": {"text": {"contains": "Resume"}}, "timeout_ms": 0})
        )
        .contains("not running")
    );
}

#[test]
fn player_get_scene_save_and_game_status_read_the_player_natively_without_the_bridge() {
    let mut script = Script::working();
    script.world_player = Some(crate::unstable::fake::native_player());
    script.stacks = vec![stack(&["in_game_play_screen", "hud_screen"])];
    let mut fixture = setup(script);

    // No WebSocket: the native read answers alone and says what commands would add.
    let player = call(&mut fixture.session, "player_get", json!({}));
    assert_eq!(player["source"], "native", "{player}");
    assert_eq!(
        player["location"],
        json!({ "x": 1.5, "y": 64.0, "z": -2.5 })
    );
    assert_eq!(player["yaw"], 90.0);
    assert_eq!(player["pitch"], 10.0);
    assert_eq!(player["health"], json!({ "current": 18.0, "max": 20.0 }));
    assert_eq!(player["hunger"], 17.0);
    assert_eq!(player["saturation"], 3.5);
    assert_eq!(player["selectedSlot"], 2);
    assert_eq!(player["effects"][0]["id"], "minecraft:speed");
    assert_eq!(player["dimension"], "minecraft:overworld");
    assert!(notes(&player).contains("velocity"), "{player}");
    assert!(notes(&player).contains("not connected"), "{player}");
    // A named player that is not on the server fails over to commands, which need the game.
    let error = fixture
        .session
        .call("player_get", json!({"actor": "Bob"}))
        .expect_err("no Bob")
        .to_string();
    assert!(error.contains("not running"), "{error}");

    // scene_save without at stores the native feet position.
    let shared = fixture.data.join("Users/Shared/games/com.mojang");
    crate::worlds::fake::world(&shared, "demo", "Demo", None);
    let saved = call(&mut fixture.session, "scene_save", json!({"name": "start"}));
    assert_eq!(saved["source"], "native", "{saved}");
    assert_eq!(
        saved["spot"],
        json!({ "x": 1.5, "y": 64.0, "z": -2.5, "yaw": 90.0, "pitch": 10.0, "dimension": "minecraft:overworld" })
    );

    // game_status's world block (built only while the game runs) gains the native player;
    // a block the Bridge pack answered stays as it is.
    let world = fixture.session.with_native_player(None).unwrap();
    assert_eq!(world["player"]["source"], "native", "{world}");
    assert_eq!(world["player"]["selectedSlot"], 2);
    let bridge = json!({ "bridge": true, "player": { "name": "Steve" } });
    assert_eq!(
        fixture.session.with_native_player(Some(bridge.clone())),
        Some(bridge)
    );
}
