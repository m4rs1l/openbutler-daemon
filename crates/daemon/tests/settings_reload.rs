//! `settings.reload` pins: the host's live settings copy refreshes from
//! disk under the same lock every reader uses, a broken store fails the
//! reload without touching the previous copy, and the answer is the
//! status view the source handler returns.
//!
//! Source: `daemon-host.ts` `request.action === 'settings.reload'`
//! (read 2026-11-16): `await reloadServices()` then `status()`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

use openbutler_daemon::dispatch::{DispatchState, Host, handle};
use openbutler_protocol::DaemonRequest;

fn temp_data_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ob-reload-{}-{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn request(action: &str) -> DaemonRequest {
    serde_json::from_str(&format!(
        r#"{{"version":2,"id":"t-reload","action":"{action}"}}"#
    ))
    .expect("well-formed request frame")
}

fn put(root: &Path, name: &str, body: &str) {
    fs::write(root.join(name), body).expect("fixture write");
}

#[test]
fn reload_answers_with_the_status_view() {
    let dir = temp_data_dir("answer");
    let host = Host::new();
    let mut state = DispatchState::new();

    let response = handle(&host, &dir, &mut state, &request("settings.reload"));

    let openbutler_protocol::DaemonResponse::Success { result, .. } = response else {
        panic!("reload must succeed on a healthy store, got {response:?}");
    };
    let status: openbutler_protocol::DaemonStatus =
        serde_json::from_value(result).expect("the answer is a status view");
    assert_eq!(status.pid, std::process::id());
    assert!(host.settings().is_some(), "the reload left a live copy");
}

#[test]
fn reload_picks_up_an_outside_change() {
    let dir = temp_data_dir("outside");
    let host = Host::new();
    let mut state = DispatchState::new();
    handle(&host, &dir, &mut state, &request("settings.reload"));
    assert_eq!(
        host.settings().as_ref().unwrap().default_provider,
        "",
        "the first load saw an empty machine"
    );

    put(&dir, "settings.yaml", "defaultProvider: outside\n");
    let response = handle(&host, &dir, &mut state, &request("settings.reload"));

    assert!(
        matches!(
            response,
            openbutler_protocol::DaemonResponse::Success { .. }
        ),
        "reload succeeds, got {response:?}"
    );
    assert_eq!(
        host.settings().as_ref().unwrap().default_provider,
        "outside"
    );
}

#[test]
fn a_broken_store_fails_the_reload_and_keeps_the_previous_copy() {
    let dir = temp_data_dir("broken");
    let host = Host::new();
    let mut state = DispatchState::new();
    put(&dir, "settings.yaml", "defaultProvider: healthy\n");
    handle(&host, &dir, &mut state, &request("settings.reload"));
    assert_eq!(
        host.settings().as_ref().unwrap().default_provider,
        "healthy"
    );

    put(&dir, "config.yaml", "providers: [not a mapping]\n");
    let response = handle(&host, &dir, &mut state, &request("settings.reload"));

    let openbutler_protocol::DaemonResponse::Failure { error, .. } = response else {
        panic!("a broken store must fail the reload, got {response:?}");
    };
    assert!(
        error.contains("config providers must be a mapping"),
        "the refusal names the file's problem: {error}"
    );
    assert_eq!(
        host.settings().as_ref().unwrap().default_provider,
        "healthy",
        "the live copy survives a failed reload untouched"
    );
}

#[test]
fn reload_is_repeatable_on_a_healthy_store() {
    let dir = temp_data_dir("repeat");
    let host = Host::new();
    let mut state = DispatchState::new();

    for attempt in 0..2 {
        let response = handle(&host, &dir, &mut state, &request("settings.reload"));
        assert!(
            matches!(
                response,
                openbutler_protocol::DaemonResponse::Success { .. }
            ),
            "attempt {attempt} must succeed, got {response:?}"
        );
    }
}
