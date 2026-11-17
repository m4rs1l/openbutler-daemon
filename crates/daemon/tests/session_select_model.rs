//! `session.select-model` at the dispatch surface: the source's two
//! requireString checks, the accept-immediately answer, the queued pick
//! the next turn resolves, and the settings record the pick teaches.
//!
//! Source: legacy `daemon-host.ts` case `session.select-model`
//! (L3559-3581) and `rememberProviderModel` (settings.ts L561),
//! read 2026-11-18. The effort-variants half (`describeModelReasoning`)
//! needs the runtime catalog and is not part of this slice.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};

use common::{temp_data_dir, uuid, write_session};
use openbutler_daemon::agent::{TurnExecutor, TurnRequest};
use openbutler_daemon::dispatch::{self, DispatchState, Host};
use openbutler_protocol::{DaemonRequest, DaemonResponse};
use serde_json::{Value, json};

/// The seam's test double: records what dispatch handed over.
#[derive(Clone, Default)]
struct RecordingExecutor {
    sent: Arc<Mutex<Vec<(String, TurnRequest)>>>,
}

impl TurnExecutor for RecordingExecutor {
    fn spawn_turn(self: Arc<Self>, session_id: String, request: TurnRequest) {
        self.sent.lock().unwrap().push((session_id, request));
    }

    fn approve(&self, _session_id: &str, _approval_id: &str, _approved: bool) -> bool {
        false
    }

    fn stop(&self, _session_id: &str) -> bool {
        false
    }

    fn answer(&self, _session_id: &str, _question_id: &str, _answer: Option<String>) -> bool {
        false
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

/// A chat with a resolvable provider: `mock` in settings.yaml (url) and
/// config.yaml (secret key), model and workspace on the record.
fn seed_chat(dir: &Path) -> String {
    std::fs::write(
        dir.join("settings.yaml"),
        "providers:\n  mock:\n    enabled: true\n    url: http://127.0.0.1:9/v1\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("config.yaml"),
        "providers:\n  mock:\n    keys:\n      - sk-test-key\n",
    )
    .unwrap();
    let id = uuid(7);
    let body = format!(
        "{{\n  \"version\": 1,\n  \"id\": \"{id}\",\n  \"title\": \"pick chat\",\n  \"createdAt\": \"2026-11-18T00:00:00.000Z\",\n  \"updatedAt\": \"2026-11-18T00:00:00.000Z\",\n  \"providerId\": \"mock\",\n  \"modelId\": \"test-model\",\n  \"workspaceRoot\": \"work/chat\",\n  \"entries\": [],\n  \"conversation\": []\n}}\n"
    );
    write_session(dir, &id, &body);
    id
}

fn pick_request(provider_id: Value, model_id: Value) -> DaemonRequest {
    request(
        "r1",
        "session.select-model",
        &[("providerId", provider_id), ("modelId", model_id)],
    )
}

/// Attaches the chat — `session.select-model` is a phase-B action, so
/// the pick only queues while a chat is attached.
fn attach(host: &Host, data_dir: &Path, state: &mut DispatchState, session_id: &str) {
    let response = call(
        host,
        data_dir,
        state,
        request(
            "a0",
            "attach",
            &[
                ("clientKind", json!("terminal")),
                ("chatSessionId", json!(session_id)),
            ],
        ),
    );
    let _ = success_result(response);
}

#[test]
fn select_model_refuses_a_missing_or_oversized_id() {
    let dir = temp_data_dir("select-model-validate");
    let host = Host::new();
    let mut state = DispatchState::default();

    let missing = failure_text(call(
        &host,
        &dir,
        &mut state,
        pick_request(Value::Null, json!("m")),
    ));
    assert_eq!(
        missing, "providerId must be a non-empty string under 256 bytes.",
        "requireString(providerId, 256)"
    );

    let oversized = failure_text(call(
        &host,
        &dir,
        &mut state,
        pick_request(json!("p"), json!("x".repeat(513))),
    ));
    assert_eq!(
        oversized, "modelId must be a non-empty string under 512 bytes.",
        "requireString(modelId, 512)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn select_model_answers_accepted_and_queues_the_pick() {
    let dir = temp_data_dir("select-model-queue");
    let session = seed_chat(&dir);
    let host = Host::new();
    host.reload_settings(&dir).expect("settings load");
    let mut state = DispatchState::new();
    attach(&host, &dir, &mut state, &session);

    let result = success_result(call(
        &host,
        &dir,
        &mut state,
        pick_request(json!("mock"), json!("picked-model")),
    ));
    assert_eq!(result, json!({ "accepted": true }), "accepted immediately");

    assert_eq!(
        host.peek_model_pick(&session),
        Some(("mock".to_owned(), "picked-model".to_owned())),
        "the pick waits for the next turn boundary"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn select_model_records_the_pick_in_settings() {
    let dir = temp_data_dir("select-model-settings");
    let _session = seed_chat(&dir);
    let host = Host::new();
    let mut state = DispatchState::default();

    success_result(call(
        &host,
        &dir,
        &mut state,
        pick_request(json!("mock"), json!("picked-model")),
    ));

    let fresh = openbutler_daemon::settings::load_settings(&dir).expect("reload");
    assert_eq!(
        fresh.store.providers["mock"].models.as_deref(),
        Some(["picked-model".to_owned()].as_slice()),
        "rememberProviderModel lands in settings.yaml"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_next_send_runs_with_the_picked_model() {
    let dir = temp_data_dir("select-model-next-turn");
    let session = seed_chat(&dir);
    let host = Host::new();
    host.reload_settings(&dir).expect("settings load");
    let recorder = RecordingExecutor::default();
    host.set_executor(Arc::new(recorder.clone()));
    let mut state = DispatchState::new();
    attach(&host, &dir, &mut state, &session);

    success_result(call(
        &host,
        &dir,
        &mut state,
        pick_request(json!("mock"), json!("picked-model")),
    ));
    success_result(call(
        &host,
        &dir,
        &mut state,
        request(
            "r2",
            "session.send",
            &[("message", json!({ "text": "hello", "images": [] }))],
        ),
    ));

    let sent = recorder.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1, "one turn handed to the seam");
    let (sent_session, turn) = &sent[0];
    assert_eq!(sent_session, &session, "for this chat");
    assert_eq!(
        turn.provider_key, "mock",
        "the URL and key resolve for the picked provider"
    );
    assert_eq!(
        turn.model, "picked-model",
        "the pick lands on the next turn boundary"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
