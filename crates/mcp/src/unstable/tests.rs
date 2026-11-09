//! Loading, the off switch and failure memory of the unstable layer against a scripted host.

use std::sync::{Arc, Mutex};

use protocol::unstable::{CapabilityState, Request};

use super::{
    Unstable,
    fake::{FakeHost, Script, Shared, entry, stack, ui_screen},
};

fn unstable(script: Script) -> (Unstable, Shared) {
    let shared = Arc::new(Mutex::new(script));
    (
        Unstable::new(Box::new(FakeHost(Arc::clone(&shared)))),
        shared,
    )
}

#[test]
fn loads_on_any_build_once_the_agent_is_attached() {
    let (mut layer, shared) = unstable(Script::working());
    assert!(!layer.ensure(false));
    assert!(layer.note().unwrap().contains("stable agent"));
    assert_eq!(shared.lock().unwrap().attaches, 0);
    assert!(layer.ensure(true));
    assert!(layer.ensure(true));
    assert_eq!(shared.lock().unwrap().attaches, 1);
    assert!(layer.active("screens") && layer.active("content_log"));
    let report = layer.report();
    assert_eq!(report["enabled"], true);
    assert_eq!(report["load"]["state"], "loaded");
    assert_eq!(report["status"]["capabilities"][0]["name"], "content_log");
    assert_eq!(layer.capabilities().unwrap()["screens"], "active");
}

#[test]
fn disabled_never_injects() {
    let mut script = Script::working();
    script.setting = Some("0".into());
    let (mut layer, shared) = unstable(script);
    assert!(!layer.ensure(true));
    assert!(layer.note().unwrap().contains("BEDROCK_MCP_UNSTABLE=0"));
    let report = layer.report();
    assert_eq!(report["enabled"], false);
    assert_eq!(report["load"]["state"], "not_loaded");
    assert_eq!(shared.lock().unwrap().attaches, 0);
}

#[test]
fn failures_are_remembered_until_the_game_restarts() {
    let mut script = Script::working();
    script.attach = Err("bedrock_unstable.dll is missing".into());
    let (mut layer, shared) = unstable(script);
    assert!(!layer.ensure(true));
    assert!(!layer.ensure(true));
    assert_eq!(shared.lock().unwrap().attaches, 1);
    let report = layer.report();
    assert_eq!(report["load"]["state"], "failed");
    assert!(
        report["load"]["detail"]
            .as_str()
            .unwrap()
            .contains("missing")
    );
    {
        let mut script = shared.lock().unwrap();
        script.attach = Ok(());
        script.pid = Some(5000);
    }
    assert!(layer.ensure(true), "a new game process is tried again");
}

#[test]
fn a_broken_connection_marks_the_layer_failed() {
    let (mut layer, shared) = unstable(Script::working());
    assert!(layer.ensure(true));
    shared.lock().unwrap().disconnect_after = Some(1);
    assert!(layer.screens().is_err());
    assert!(!layer.ensure(true));
    assert!(!layer.active("screens"));
    assert_eq!(shared.lock().unwrap().attaches, 1);
}

#[test]
fn a_capability_that_breaks_reports_why_and_leaves_the_others_usable() {
    let mut script = Script::working();
    script.capabilities = vec![
        ("screens", CapabilityState::Pending, ""),
        (
            "content_log",
            CapabilityState::Failed,
            "vtable has 4 slots, expected 3",
        ),
    ];
    let (mut layer, shared) = unstable(script);
    assert!(layer.ensure(true));
    assert!(!layer.active("content_log"));
    assert_eq!(
        layer.note(),
        Some("unstable capability content_log is failed: vtable has 4 slots, expected 3")
    );
    assert!(!layer.active("screens"));
    assert_eq!(layer.note(), Some("unstable capability screens is pending"));
    assert!(!layer.active("missing"));
    assert!(layer.note().unwrap().contains("not reported"));
    shared.lock().unwrap().capabilities[0].1 = CapabilityState::Active;
    assert!(layer.active("screens"), "a pending capability is refreshed");
    assert!(
        layer.ensure(true),
        "a failed capability keeps the layer loaded"
    );
}

#[test]
fn requests_return_the_dlls_data() {
    let mut script = Script::working();
    script.log = vec![entry(1, 2, "a"), entry(2, 3, "b")];
    script.stacks = vec![stack(&["hud_screen"])];
    let (mut layer, _) = unstable(script);
    assert!(layer.ensure(true));
    let batch = layer.content_log(1, 10).unwrap();
    assert_eq!(batch.entries.len(), 1);
    assert_eq!(batch.next, 2);
    assert_eq!(layer.screens().unwrap()[0].screens, ["hud_screen"]);
}

#[test]
fn ui_tree_requests_pass_their_options_and_filter_by_stack() {
    let mut script = Script::working();
    script.ui_screens = vec![ui_screen(1, "hud_screen"), ui_screen(2, "pause_screen")];
    let (mut layer, shared) = unstable(script);
    assert!(layer.ensure(true));
    assert_eq!(layer.ui_tree(None, None, true).unwrap().len(), 2);
    let only = layer.ui_tree(Some(2), Some(5), false).unwrap();
    assert_eq!(only.len(), 1);
    assert_eq!(only[0].scene, "pause_screen");
    assert_eq!(
        shared.lock().unwrap().requests.last(),
        Some(&Request::UiTree {
            stack: Some(2),
            max_depth: Some(5),
            visible_only: false,
        })
    );
}
