//! Input sequencing against a fake agent and clock.

use serde_json::json;

use super::*;
use crate::{agent::fake::FakeAgent, clock::fake::FakeClock, keys::bindings};

fn test_bindings() -> Bindings {
    let options = [
        ("keyboard_type_0_key.forward", "87"),
        ("keyboard_type_0_key.back", "83"),
        ("keyboard_type_0_key.left", "65"),
        ("keyboard_type_0_key.right", "68"),
        ("keyboard_type_0_key.jump", "32"),
        ("keyboard_type_0_key.sneak", "16"),
        ("keyboard_type_0_key.chat", "84,13"),
        ("keyboard_type_0_key.hotbar.3", "51"),
        ("keyboard_type_0_key.attack", "-99"),
    ]
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .into_iter()
    .collect();
    bindings(&options)
}

fn apply(
    controller: &mut Controller,
    agent: &mut FakeAgent,
    request: serde_json::Value,
) -> Result<()> {
    apply_timed(controller, agent, request).map(|_| ())
}

/// Apply a request; returns how long it slept on the fake clock.
fn apply_timed(
    controller: &mut Controller,
    agent: &mut FakeAgent,
    request: serde_json::Value,
) -> Result<Duration> {
    let bindings = test_bindings();
    let clock = FakeClock::new();
    let scale = || Ok(1.5);
    let env = InputEnv {
        bindings: &bindings,
        scale: &scale,
        world: None,
        clock: &clock,
    };
    controller.apply(agent, &env, &serde_json::from_value(request).unwrap())?;
    Ok(clock.elapsed())
}

#[test]
fn pointer_clicks_hover_before_the_button_goes_down() {
    let (mut controller, mut agent) = (Controller::default(), FakeAgent::new());
    let slept = apply_timed(
        &mut controller,
        &mut agent,
        json!({"pointer": {"x": 10, "y": 10}, "press": ["MouseLeft"]}),
    )
    .unwrap();
    assert!(slept >= HOVER_DELAY);
    let held = apply_timed(
        &mut controller,
        &mut agent,
        json!({"cursor": [10, 10], "hold": ["key.attack"]}),
    )
    .unwrap();
    assert!(held >= HOVER_DELAY);
    controller.release_all(&mut agent).unwrap();
    for request in [
        json!({"pointer": [10, 10]}),
        json!({"pointer": [10, 10], "press": ["KeyE"]}),
        json!({"pointer": null, "press": ["MouseLeft"]}),
        json!({"press": ["MouseLeft"]}),
    ] {
        let slept = apply_timed(&mut controller, &mut agent, request.clone()).unwrap();
        assert!(slept < HOVER_DELAY, "{request}");
    }
}

fn key(down: bool, vk: u8, scan: u8) -> Request {
    Request::Key {
        down,
        vk,
        scan,
        extended: false,
    }
}

#[test]
fn requests_apply_in_the_documented_order() {
    let (mut controller, mut agent) = (Controller::default(), FakeAgent::new());
    apply(
        &mut controller,
        &mut agent,
        json!({
            "text": "hi", "press": ["MouseLeft"], "pointer": [100, 20], "hotbar": 3,
            "move": {"forward": 1}, "release_all": true,
        }),
    )
    .unwrap();
    assert_eq!(
        agent.sent(),
        [
            &Request::Reset,
            &Request::Pointer { x: 150, y: 30 },
            &key(true, 0x57, 0x11),
            &key(true, 0x33, 0x04),
            &Request::Buttons { mask: mouse::LEFT },
            &key(false, 0x33, 0x04),
            &Request::Buttons { mask: 0 },
            &Request::Char { code: 'h'.into() },
            &Request::Char { code: 'i'.into() },
        ]
    );
    assert_eq!(controller.held(), ["KeyW"]);
}

#[test]
fn movement_axes_release_the_opposite_key_and_zero_releases_both() {
    let (mut controller, mut agent) = (Controller::default(), FakeAgent::new());
    apply(
        &mut controller,
        &mut agent,
        json!({"move": {"forward": -1}, "jump": true}),
    )
    .unwrap();
    assert_eq!(controller.held(), ["KeyS", "Space"]);
    agent.requests.clear();
    apply(
        &mut controller,
        &mut agent,
        json!({"move": {"forward": 0}, "jump": false}),
    )
    .unwrap();
    assert_eq!(
        agent.sent(),
        [&key(false, 0x53, 0x1F), &key(false, 0x20, 0x39)]
    );
    assert!(controller.held().is_empty());
}

#[test]
fn presses_wait_frames_and_keep_controls_that_are_also_held() {
    let (mut controller, mut agent) = (Controller::default(), FakeAgent::new());
    apply(
        &mut controller,
        &mut agent,
        json!({"hold": ["KeyE"], "press": ["KeyE", "KeyQ"], "press_frames": 3}),
    )
    .unwrap();
    assert_eq!(controller.held(), ["KeyE"]);
    // One status to read the start, then one per frame the fake renders.
    assert_eq!(
        agent
            .requests
            .iter()
            .filter(|r| **r == Request::Status)
            .count(),
        4
    );
}

#[test]
fn hold_ms_releases_this_requests_hold() {
    let (mut controller, mut agent) = (Controller::default(), FakeAgent::new());
    apply(
        &mut controller,
        &mut agent,
        json!({"hold": ["key.attack", "KeyA"], "hold_ms": 250}),
    )
    .unwrap();
    assert!(controller.held().is_empty());
    assert_eq!(agent.sent().last(), Some(&&Request::Buttons { mask: 0 }));
}

#[test]
fn wheel_lines_are_one_notch_per_frame() {
    let (mut controller, mut agent) = (Controller::default(), FakeAgent::new());
    apply(
        &mut controller,
        &mut agent,
        json!({"wheel": {"y": -2}, "scroll": {"y": 30, "pixels": true}}),
    )
    .unwrap();
    assert_eq!(
        agent.sent(),
        [
            &Request::Wheel { dx: 0, dy: -120 },
            &Request::Wheel { dx: 0, dy: -120 },
            &Request::Wheel { dx: 0, dy: 30 },
        ]
    );
}

#[test]
fn null_pointer_releases_and_bad_controls_send_nothing() {
    let (mut controller, mut agent) = (Controller::default(), FakeAgent::new());
    apply(&mut controller, &mut agent, json!({"cursor": null})).unwrap();
    assert_eq!(agent.sent(), [&Request::PointerRelease]);
    agent.requests.clear();
    assert!(
        apply(
            &mut controller,
            &mut agent,
            json!({"hold": ["KeyW", "Nope"]})
        )
        .is_err()
    );
    assert!(
        apply(
            &mut controller,
            &mut agent,
            json!({"look": {"yaw": 1, "pitch": 2}})
        )
        .is_err()
    );
    assert!(agent.requests.is_empty());
}

#[test]
fn stalled_frames_time_out_on_the_injected_clock() {
    let mut agent = FakeAgent::new();
    // Every status answers the same reading count.
    for _ in 0..10_000 {
        agent
            .replies
            .push_back(protocol::Reply::Status(protocol::Status {
                protocol: protocol::PROTOCOL_VERSION,
                pid: 1,
                width: 1,
                height: 1,
                foreground: false,
                game_active: true,
                mouse_readings: 5,
                gameinput_versions: vec![],
                pointer: [0, 0],
                pointer_override: false,
                input_locked: false,
                takeovers: 0,
            }));
    }
    let clock = FakeClock::new();
    assert!(wait_frames(&mut agent, &clock, 1).is_err());
    assert!(clock.elapsed() >= Duration::from_millis(2100));
}

#[test]
fn chat_opens_clears_types_and_sends() {
    let (mut controller, mut agent) = (Controller::default(), FakeAgent::new());
    controller
        .send_chat(&mut agent, &FakeClock::new(), &test_bindings(), "/a")
        .unwrap();
    assert_eq!(
        agent.sent(),
        [
            &key(true, 0x54, 0x14),
            &key(false, 0x54, 0x14),
            &key(true, 0x08, 0x0E),
            &key(false, 0x08, 0x0E),
            &Request::Char { code: '/'.into() },
            &Request::Char { code: 'a'.into() },
            &key(true, 0x0D, 0x1C),
            &key(false, 0x0D, 0x1C),
        ]
    );
    let position = |wanted: &Request| agent.requests.iter().position(|r| r == wanted).unwrap();
    let typed = position(&Request::Char { code: 'a'.into() });
    let enter_down = position(&key(true, 0x0D, 0x1C));
    let enter_up = position(&key(false, 0x0D, 0x1C));
    let polls = |range: std::ops::Range<usize>| {
        agent.requests[range]
            .iter()
            .filter(|r| **r == Request::Status)
            .count()
    };
    // Each frame wait reads a start status plus one per frame on the fake agent.
    assert!(polls(typed..enter_down) > CHAT_SUBMIT_DELAY_FRAMES as usize);
    assert!(polls(enter_down..enter_up) > CHAT_ENTER_FRAMES as usize);
}
