//! `permissions.set-mode` pins: the mode is validated with the source's
//! one message, `ask` canonicalises to `workspace`, the change persists
//! through the load→save path, and the host's live copy follows.
//!
//! Source: `daemon-host.ts` `permissions.set-mode` + `applyApprovalMode`
//! (read 2026-11-16): validate, canonicalise, `loadSettings()` mutate
//! `saveSettings()`, then push to the live runtimes (M2 attaches that half).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

use openbutler_daemon::dispatch::{DispatchState, Host, handle};
use openbutler_protocol::DaemonRequest;

fn temp_data_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ob-setmode-{}-{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn set_mode(mode: &str) -> DaemonRequest {
    DaemonRequest::new("t-mode", "permissions.set-mode").with("mode", mode)
}

fn apply(host: &Host, dir: &Path, mode: &str) -> openbutler_protocol::DaemonResponse {
    let mut state = DispatchState::new();
    handle(host, dir, &mut state, &set_mode(mode))
}

#[test]
fn a_valid_mode_persists_and_reaches_the_live_copy() {
    let dir = temp_data_dir("persist");
    let host = Host::new();
    host.reload_settings(&dir).expect("boot load");

    let response = apply(&host, &dir, "full-access");

    let openbutler_protocol::DaemonResponse::Success { result, .. } = response else {
        panic!("a valid mode must succeed, got {response:?}");
    };
    assert_eq!(
        result.get("mode").and_then(|v| v.as_str()),
        Some("full-access")
    );
    let saved = fs::read_to_string(dir.join("settings.yaml")).expect("settings written");
    assert!(
        saved.contains("approvalMode: full-access"),
        "the change persists to disk: {saved}"
    );
    let live = host.settings().as_ref().expect("live copy").store.clone();
    assert_eq!(
        live.approval_mode.as_str(),
        "full-access",
        "the host's live copy carries the new mode"
    );
}

#[test]
fn ask_canonicalises_to_workspace() {
    let dir = temp_data_dir("ask");
    let host = Host::new();
    host.reload_settings(&dir).expect("boot load");

    let response = apply(&host, &dir, "ask");

    let openbutler_protocol::DaemonResponse::Success { result, .. } = response else {
        panic!("ask must succeed, got {response:?}");
    };
    assert_eq!(
        result.get("mode").and_then(|v| v.as_str()),
        Some("workspace")
    );
    let saved = fs::read_to_string(dir.join("settings.yaml")).expect("settings written");
    assert!(
        saved.contains("approvalMode: workspace"),
        "ask is stored as workspace: {saved}"
    );
}

#[test]
fn an_unknown_mode_is_refused_with_the_source_message_and_changes_nothing() {
    let dir = temp_data_dir("refuse");
    let host = Host::new();
    host.reload_settings(&dir).expect("boot load");

    let response = apply(&host, &dir, "yolo");

    let openbutler_protocol::DaemonResponse::Failure { error, .. } = response else {
        panic!("an unknown mode must fail, got {response:?}");
    };
    assert_eq!(
        error,
        "mode must be workspace, auto-review, or full-access."
    );
    assert!(
        !dir.join("settings.yaml").exists(),
        "a refused mode never writes"
    );
    assert_eq!(
        host.settings()
            .as_ref()
            .expect("live copy")
            .store
            .approval_mode
            .as_str(),
        "workspace",
        "the live copy keeps the boot value"
    );
}
