//! `session.command` (the registry normalization) and the telegram trio
//! at the dispatch surface.
//!
//! Source: legacy `daemon-host.ts` L3492 (`session.command` re-dispatch
//! before the switch), L3261-3271 (telegram), and
//! `commands/registry.ts` `daemonCommandAction` (L94) — read
//! 2026-11-18. The registry holds three daemon commands; every other
//! name is surface-only or unknown and stops with `Unknown command.`.
//!
//! `telegram.status` answers from the fixed disabled shape: this build
//! has no telegram service, which is exactly the source's `?? fallback`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};

use common::{bound_state, pretty_session, temp_data_dir, uuid, with_binding, write_session};
use openbutler_daemon::agent::{TurnExecutor, TurnRequest};
use openbutler_daemon::dispatch::{self, DispatchState, Host};
use openbutler_protocol::{DaemonRequest, DaemonResponse};
use serde_json::{Value, json};

/// The seam's test double: records stops.
#[derive(Clone, Default)]
struct RecordingExecutor {
    stops: Arc<Mutex<Vec<(String, bool)>>>,
}

impl TurnExecutor for RecordingExecutor {
    fn spawn_turn(self: Arc<Self>, _session_id: String, _request: TurnRequest) {}

    fn approve(&self, _session_id: &str, _approval_id: &str, _approved: bool) -> bool {
        false
    }

    fn stop(&self, session_id: &str, keep_queue: bool) -> bool {
        self.stops
            .lock()
            .unwrap()
            .push((session_id.to_owned(), keep_queue));
        true
    }

    fn manage_queue(
        &self,
        _session_id: &str,
        _message_id: &str,
        _action: openbutler_daemon::agent::QueueAction,
    ) -> Result<(), String> {
        Ok(())
    }

    fn answer(&self, _session_id: &str, _question_id: &str, _answer: Option<String>) -> bool {
        false
    }

    fn is_busy(&self, _session_id: &str) -> bool {
        false
    }

    fn compact(self: Arc<Self>, _session_id: String, _recovery: bool) {}

    fn has_live_session(&self, _session_id: &str) -> bool {
        false
    }

    fn spawn_turn_for_event(
        self: Arc<Self>,
        _session_id: String,
        _request: TurnRequest,
    ) -> tokio::sync::oneshot::Receiver<Result<(), String>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = tx.send(Ok(()));
        rx
    }
}

fn request(id: &str, action: &str, pairs: &[(&str, Value)]) -> DaemonRequest {
    let mut request = DaemonRequest::new(id, action);
    for (key, value) in pairs {
        request = request.with(*key, value.clone());
    }
    request
}

fn call(
    host: &Host,
    data_dir: &Path,
    state: &mut DispatchState,
    request: DaemonRequest,
) -> DaemonResponse {
    let request = with_binding(state, request);
    dispatch::handle(host, data_dir, state, &request)
}

fn success_result(response: DaemonResponse) -> Value {
    let DaemonResponse::Success { result, .. } = response else {
        panic!("expected a success envelope, got {response:?}");
    };
    result
}

fn failure_text(response: DaemonResponse) -> String {
    let DaemonResponse::Failure { error, .. } = response else {
        panic!("expected a failure envelope, got {response:?}");
    };
    error
}

fn load_chat(host: &Host, dir: &Path, state: &mut DispatchState, id: &str) {
    let response = call(
        host,
        dir,
        state,
        request("1", "session.load", &[("sessionId", json!(id))]),
    );
    success_result(response);
}

#[test]
fn command_requires_a_name_within_the_limit() {
    let dir = temp_data_dir("cmd-name");
    let host = Host::new();
    let mut state = bound_state();

    let response = call(
        &host,
        &dir,
        &mut state,
        request("1", "session.command", &[]),
    );
    assert_eq!(
        failure_text(response),
        "name must be a non-empty string under 64 bytes."
    );

    let response = call(
        &host,
        &dir,
        &mut state,
        request("2", "session.command", &[("name", json!(""))]),
    );
    assert_eq!(
        failure_text(response),
        "name must be a non-empty string under 64 bytes."
    );
}

#[test]
fn command_refuses_surface_and_unknown_names() {
    let dir = temp_data_dir("cmd-unknown");
    let host = Host::new();
    let mut state = bound_state();

    // `/help` exists in the vocabulary but its effect belongs to a
    // surface — the daemon has no part in it.
    for name in ["/help", "/skills", "/nope"] {
        let response = call(
            &host,
            &dir,
            &mut state,
            request("1", "session.command", &[("name", json!(name))]),
        );
        assert_eq!(failure_text(response), "Unknown command.", "name {name}");
    }
}

#[test]
fn command_routes_stop_with_its_payload_intact() {
    let dir = temp_data_dir("cmd-stop");
    let host = Host::new();
    let executor = RecordingExecutor::default();
    host.set_executor(Arc::new(executor.clone()));
    let mut state = bound_state();
    let id = uuid(1);
    write_session(&dir, &id, &pretty_session(&id, None, "Chat", true));
    load_chat(&host, &dir, &mut state, &id);

    // `/stop` re-dispatches to `session.stop` — the target arm reads the
    // same payload (`keepQueue` rides along untouched).
    let response = call(
        &host,
        &dir,
        &mut state,
        request(
            "1",
            "session.command",
            &[("name", json!("/stop")), ("keepQueue", json!(true))],
        ),
    );
    assert_eq!(success_result(response), json!({ "accepted": true }));
    let stops = executor.stops.lock().unwrap();
    assert_eq!(stops.as_slice(), &[(id.clone(), true)]);
}

#[test]
fn command_routes_new_to_the_draft_arm() {
    let dir = temp_data_dir("cmd-new");
    let host = Host::new();
    let mut state = bound_state();

    let response = call(
        &host,
        &dir,
        &mut state,
        request("1", "session.command", &[("name", json!("/new"))]),
    );
    assert_eq!(success_result(response), json!({ "accepted": true }));
    assert!(
        state.current_session().is_some(),
        "the draft arm ran behind the name"
    );
}

#[test]
fn command_resolves_compact_to_the_routed_action() {
    let dir = temp_data_dir("cmd-compact");
    let host = Host::new();
    let mut state = bound_state();
    let id = uuid(2);
    write_session(&dir, &id, &pretty_session(&id, None, "Chat", true));
    load_chat(&host, &dir, &mut state, &id);

    // Resolution and routing: `session.compact` is live, so the
    // command reaches the arm's own executor gate (the seam itself is
    // pinned in session_compact.rs).
    let response = call(
        &host,
        &dir,
        &mut state,
        request("1", "session.command", &[("name", json!("/compact"))]),
    );
    assert_eq!(failure_text(response), "The host has no turn executor.");
}

#[test]
fn telegram_status_reports_the_disabled_service() {
    let dir = temp_data_dir("tg-status");
    let host = Host::new();
    let mut state = bound_state();

    let response = call(
        &host,
        &dir,
        &mut state,
        request("1", "telegram.status", &[]),
    );
    assert_eq!(
        success_result(response),
        json!({ "state": "disabled", "paired": false, "username": null, "error": null })
    );
}

#[test]
fn telegram_pair_and_test_refuse_without_configuration() {
    let dir = temp_data_dir("tg-pair");
    let host = Host::new();
    let mut state = bound_state();
    let expected = "Telegram is not configured. Run `openbutler telegram setup` first.";

    let response = call(&host, &dir, &mut state, request("1", "telegram.pair", &[]));
    assert_eq!(failure_text(response), expected);

    let response = call(&host, &dir, &mut state, request("2", "telegram.test", &[]));
    assert_eq!(failure_text(response), expected);
}
