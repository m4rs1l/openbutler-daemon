//! `settings.set-listen` pins: `openbutler daemon start --listen` saves the
//! remote listener through the daemon, validated by the very rule the host
//! binds by, and `--no-listen` clears it.
//!
//! Source: `cli-main.ts` L1783-1798 (`listenPreference`) and
//! `listen-target.ts` (the refusal wordings), read 2026-11-24.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;

use common::temp_data_dir;
use openbutler_daemon::dispatch::{DispatchState, Host, handle};
use openbutler_protocol::{DaemonRequest, DaemonResponse};
use serde_json::{Value, json};

fn request(listen: Value) -> DaemonRequest {
    DaemonRequest::new("r1", "settings.set-listen").with("listen", listen)
}

fn call(host: &Host, dir: &Path, listen: Value) -> DaemonResponse {
    handle(host, dir, &mut DispatchState::new(), &request(listen))
}

fn success_json(response: DaemonResponse) -> Value {
    let DaemonResponse::Success { result, .. } = response else {
        panic!("expected success, got {response:?}");
    };
    result
}

fn failure_text(response: DaemonResponse) -> String {
    let DaemonResponse::Failure { error, .. } = response else {
        panic!("expected failure, got {response:?}");
    };
    error
}

fn settings_text(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("settings.yaml")).unwrap_or_default()
}

#[test]
fn a_tailnet_target_is_saved_and_the_live_copy_moves() {
    let dir = temp_data_dir("listen-save");
    let host = Host::new();
    host.reload_settings(&dir).unwrap();

    let answer = success_json(call(&host, &dir, json!("100.64.0.5:45280")));
    assert_eq!(answer["listen"], "100.64.0.5:45280");
    assert!(
        settings_text(&dir).contains("listen: 100.64.0.5:45280"),
        "the listener lands in settings.yaml: {}",
        settings_text(&dir)
    );
    let live = host.settings();
    assert_eq!(
        live.as_ref().unwrap().store.daemon.as_ref().unwrap().listen,
        "100.64.0.5:45280",
        "the live copy is reloaded, not left stale"
    );
}

#[test]
fn a_public_address_is_refused_with_the_binding_rules_own_sentence() {
    let dir = temp_data_dir("listen-public");
    let host = Host::new();
    host.reload_settings(&dir).unwrap();

    let error = failure_text(call(&host, &dir, json!("203.0.113.7:45280")));
    assert_eq!(
        error,
        "203.0.113.7 is not a tailnet address. The daemon binds the tailnet or loopback only; 0.0.0.0 and public addresses are refused."
    );
    assert!(
        !settings_text(&dir).contains("listen"),
        "a refused target is never written"
    );
}

#[test]
fn a_malformed_target_is_refused_before_anything_is_written() {
    let dir = temp_data_dir("listen-shape");
    let host = Host::new();
    host.reload_settings(&dir).unwrap();

    let error = failure_text(call(&host, &dir, json!("not-an-address")));
    assert_eq!(
        error,
        "--listen needs an IPv4 address and port, e.g. 100.64.0.5:45280 (got \"not-an-address\")."
    );
    assert!(!settings_text(&dir).contains("listen"));
}

#[test]
fn null_clears_the_saved_target() {
    let dir = temp_data_dir("listen-clear");
    let host = Host::new();
    host.reload_settings(&dir).unwrap();
    success_json(call(&host, &dir, json!("127.0.0.1:45281")));
    assert!(settings_text(&dir).contains("listen: 127.0.0.1:45281"));

    let answer = success_json(call(&host, &dir, Value::Null));
    assert!(answer["listen"].is_null());
    assert!(
        !settings_text(&dir).contains("listen"),
        "a cleared listener leaves no daemon section: {}",
        settings_text(&dir)
    );
    assert!(host.settings().as_ref().unwrap().store.daemon.is_none());
}

#[test]
fn a_missing_or_non_string_value_is_one_refusal() {
    let dir = temp_data_dir("listen-shape-gate");
    let host = Host::new();
    host.reload_settings(&dir).unwrap();

    let missing = handle(
        &host,
        &dir,
        &mut DispatchState::new(),
        &DaemonRequest::new("r2", "settings.set-listen"),
    );
    assert_eq!(
        failure_text(missing),
        "listen must be a host:port address or null."
    );
    assert_eq!(
        failure_text(call(&host, &dir, json!(42))),
        "listen must be a host:port address or null."
    );
}

/// The boot-time rule (`daemonRunOptions`): a typed flag wins, the
/// suppress flag refuses the saved preference, otherwise the saved value
/// applies — so `daemon start --listen` keeps binding on every later boot.
#[test]
fn the_boot_listener_prefers_the_flag_then_the_saved_value() {
    use openbutler_daemon::listen_target::effective_listen;
    assert_eq!(
        effective_listen(Some("100.64.0.9:1"), false, Some("127.0.0.1:2")).as_deref(),
        Some("100.64.0.9:1")
    );
    assert_eq!(effective_listen(None, true, Some("127.0.0.1:2")), None);
    assert_eq!(
        effective_listen(None, false, Some("127.0.0.1:2")).as_deref(),
        Some("127.0.0.1:2")
    );
    assert_eq!(effective_listen(None, false, None), None);
}
