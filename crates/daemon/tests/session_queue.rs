//! `session.queue` (steer/remove) and the send queue behind it at the
//! dispatch surface: the source's validation sentences, the seam
//! forwarding, and the accept-immediately answer.
//!
//! Source: legacy `daemon-host.ts` case `session.queue` (L3544-3552),
//! read 2026-11-18. The queue's own behavior (enqueue while busy,
//! drain, steering admission) lives in `tests/agent_loop.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};

use common::{bound_state, temp_data_dir, uuid, with_binding, write_session};
use openbutler_daemon::agent::{QueueAction, TurnExecutor, TurnRequest};
use openbutler_daemon::dispatch::{self, DispatchState, Host};
use openbutler_protocol::{DaemonRequest, DaemonResponse};
use serde_json::{Value, json};

/// The seam's test double: records queue management calls.
#[derive(Clone, Default)]
struct RecordingExecutor {
    sent: Arc<Mutex<Vec<(String, TurnRequest)>>>,
    managed: Arc<Mutex<Vec<(String, String, QueueAction)>>>,
    /// What `manage_queue` answers (the runner's refusals); `None`
    /// means an accepted manage.
    answer: Arc<Mutex<Option<Result<(), String>>>>,
    stopped: Arc<Mutex<Vec<(String, bool)>>>,
}

impl TurnExecutor for RecordingExecutor {
    fn spawn_turn(self: Arc<Self>, session_id: String, request: TurnRequest) {
        self.sent.lock().unwrap().push((session_id, request));
    }

    fn approve(&self, _session_id: &str, _approval_id: &str, _approved: bool) -> bool {
        false
    }

    fn stop(&self, session_id: &str, keep_queue: bool) -> bool {
        self.stopped
            .lock()
            .unwrap()
            .push((session_id.to_owned(), keep_queue));
        true
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

    fn answer(&self, _session_id: &str, _question_id: &str, _answer: Option<String>) -> bool {
        false
    }

    fn manage_queue(
        &self,
        session_id: &str,
        message_id: &str,
        action: QueueAction,
    ) -> Result<(), String> {
        self.managed
            .lock()
            .unwrap()
            .push((session_id.to_owned(), message_id.to_owned(), action));
        self.answer.lock().unwrap().clone().unwrap_or(Ok(()))
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

/// Attaches a chat so phase-B actions have a current session.
fn seed_and_attach(dir: &Path, host: &Host, state: &mut DispatchState) -> String {
    let id = uuid(7);
    let body = format!(
        "{{\n  \"version\": 1,\n  \"id\": \"{id}\",\n  \"title\": \"queue chat\",\n  \"createdAt\": \"2026-11-18T00:00:00.000Z\",\n  \"updatedAt\": \"2026-11-18T00:00:00.000Z\",\n  \"providerId\": \"mock\",\n  \"modelId\": \"test-model\",\n  \"entries\": [],\n  \"conversation\": []\n}}\n"
    );
    write_session(dir, &id, &body);
    let response = call(
        host,
        dir,
        state,
        request(
            "a0",
            "attach",
            &[
                ("clientKind", json!("terminal")),
                ("chatSessionId", json!(id)),
            ],
        ),
    );
    let _ = success_result(response);
    id
}

fn queue_request(pairs: &[(&str, Value)]) -> DaemonRequest {
    request("q1", "session.queue", pairs)
}

#[test]
fn session_queue_validates_its_three_fields() {
    let dir = temp_data_dir("queue-validate");
    let host = Host::new();
    let mut state = bound_state();

    let missing_session = failure_text(call(
        &host,
        &dir,
        &mut state,
        queue_request(&[("messageId", json!("m1")), ("queueAction", json!("steer"))]),
    ));
    assert_eq!(
        missing_session, "sessionId must be a non-empty string under 64 bytes.",
        "requireString(sessionId, 64)"
    );

    let missing_message = failure_text(call(
        &host,
        &dir,
        &mut state,
        queue_request(&[("sessionId", json!("s1")), ("queueAction", json!("steer"))]),
    ));
    assert_eq!(
        missing_message, "messageId must be a non-empty string under 64 bytes.",
        "requireString(messageId, 64)"
    );

    let bad_action = failure_text(call(
        &host,
        &dir,
        &mut state,
        queue_request(&[
            ("sessionId", json!("s1")),
            ("messageId", json!("m1")),
            ("queueAction", json!("nudge")),
        ]),
    ));
    assert_eq!(bad_action, "Invalid queue action.", "L3547 throw");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_pick_of_another_session_reports_the_active_session_changed() {
    let dir = temp_data_dir("queue-session");
    let host = Host::new();
    let recorder = RecordingExecutor::default();
    host.set_executor(Arc::new(recorder.clone()));
    let mut state = bound_state();
    seed_and_attach(&dir, &host, &mut state);

    let response = call(
        &host,
        &dir,
        &mut state,
        queue_request(&[
            ("sessionId", json!("some-other-session")),
            ("messageId", json!("m1")),
            ("queueAction", json!("steer")),
        ]),
    );
    assert_eq!(
        failure_text(response),
        "The active session has changed.",
        "manageQueuedMessage L2041"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_valid_manage_forwards_to_the_seam_and_accepts() {
    let dir = temp_data_dir("queue-forward");
    let host = Host::new();
    let recorder = RecordingExecutor::default();
    host.set_executor(Arc::new(recorder.clone()));
    let mut state = bound_state();
    let session = seed_and_attach(&dir, &host, &mut state);

    let result = success_result(call(
        &host,
        &dir,
        &mut state,
        queue_request(&[
            ("sessionId", json!(session)),
            ("messageId", json!("m1")),
            ("queueAction", json!("remove")),
        ]),
    ));
    assert_eq!(result, json!({ "accepted": true }));

    let managed = recorder.managed.lock().unwrap().clone();
    assert_eq!(
        managed,
        [(session.clone(), "m1".to_owned(), QueueAction::Remove)],
        "the seam receives session, message and action"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_runner_refusal_reaches_the_client_as_a_failure() {
    let dir = temp_data_dir("queue-refusal");
    let host = Host::new();
    let recorder = RecordingExecutor::default();
    *recorder.answer.lock().unwrap() = Some(Err("This message is no longer queued.".to_owned()));
    host.set_executor(Arc::new(recorder.clone()));
    let mut state = bound_state();
    let session = seed_and_attach(&dir, &host, &mut state);

    let response = call(
        &host,
        &dir,
        &mut state,
        queue_request(&[
            ("sessionId", json!(session)),
            ("messageId", json!("gone")),
            ("queueAction", json!("steer")),
        ]),
    );
    assert_eq!(
        failure_text(response),
        "This message is no longer queued.",
        "the runner's refusal is the client's error"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn session_stop_passes_keep_queue_through() {
    let dir = temp_data_dir("queue-stop");
    let host = Host::new();
    let recorder = RecordingExecutor::default();
    host.set_executor(Arc::new(recorder.clone()));
    let mut state = bound_state();
    let session = seed_and_attach(&dir, &host, &mut state);

    success_result(call(
        &host,
        &dir,
        &mut state,
        request("s1", "session.stop", &[("keepQueue", json!(true))]),
    ));
    success_result(call(
        &host,
        &dir,
        &mut state,
        request("s2", "session.stop", &[]),
    ));

    assert_eq!(
        *recorder.stopped.lock().unwrap(),
        [(session.clone(), true), (session, false)],
        "keepQueue === true reaches the runner"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
