//! `attach` at the dispatch surface: the chat path — validation with the
//! source's wording, the windowed answer, the subscription, and the
//! snapshot event that follows the response.
//!
//! Source: legacy `daemon-host.ts` `attach` handler (L3312-3425),
//! `attachChat` (L1432), `validateAttachWorkspaceRoot` (L4200), read
//! 2026-11-17. Deliberately not here: isolated/schedule/worker/new-chat
//! attachment paths — they need the runtime (M2).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};

use common::{temp_data_dir, write_session};
use openbutler_daemon::dispatch::{DispatchState, Host, fanout::EventSink};
use openbutler_protocol::envelope::DaemonEvent;
use openbutler_protocol::{DaemonRequest, DaemonResponse};
use serde_json::{Value, json};

/// A sink that records every published frame.
#[derive(Default)]
struct RecordingSink {
    frames: Mutex<Vec<Vec<u8>>>,
}

impl EventSink for RecordingSink {
    fn try_send(&self, frame: Vec<u8>) -> bool {
        self.frames.lock().unwrap().push(frame);
        true
    }
}

fn request(id: &str, pairs: &[(&str, Value)]) -> DaemonRequest {
    let mut request = DaemonRequest::new(id, "attach");
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
    openbutler_daemon::dispatch::handle(host, data_dir, state, &request)
}

fn success_result(response: DaemonResponse) -> Value {
    match response {
        DaemonResponse::Success { result, .. } => result,
        other => panic!("expected success, got {other:?}"),
    }
}

fn failure_text(response: DaemonResponse) -> String {
    match response {
        DaemonResponse::Failure { error, .. } => error,
        other => panic!("expected failure, got {other:?}"),
    }
}

/// Seeds one readable chat record and returns its id.
fn seed_chat(data_dir: &Path) -> String {
    let id = "33333333-3333-4333-8333-333333333333".to_owned();
    let body = json!({
        "version": 1,
        "id": id,
        "title": "Attached chat",
        "createdAt": "2026-11-17T21:00:00.000Z",
        "updatedAt": "2026-11-17T21:00:00.000Z",
        "providerId": "mock",
        "modelId": "test-model",
        "entries": [
            {
                "kind": "user",
                "id": "e1",
                "at": 1763400000000i64,
                "text": "one",
            },
            {
                "kind": "user",
                "id": "e2",
                "at": 1763400001000i64,
                "text": "two",
            },
        ],
        "conversation": [],
    });
    write_session(data_dir, &id, &body.to_string());
    id
}

#[test]
fn an_attach_answers_with_the_chat_snapshot_and_subscribes_the_sink() {
    let dir = temp_data_dir("attach-chat");
    let session_id = seed_chat(&dir);
    let host = Arc::new(Host::new());
    host.reload_settings(&dir).expect("settings load");
    let mut state = DispatchState::new();
    let sink = Arc::new(RecordingSink::default());
    state.set_sink(sink.clone());

    let response = call(
        &host,
        &dir,
        &mut state,
        request(
            "a1",
            &[
                ("clientKind", json!("terminal")),
                ("chatSessionId", json!(session_id)),
            ],
        ),
    );
    // The connection's role: answer first, then flush the queued event.
    for frame in state.take_pending_events() {
        assert!(sink.try_send(frame));
    }
    let result = success_result(response);
    assert_eq!(result["sessionAttachments"], json!(true));
    assert_eq!(result["clientKind"], json!("terminal"));
    assert!(result["workspaceKey"].as_str().is_some(), "a binding key");
    assert_eq!(result["snapshot"]["session"]["id"], json!(session_id));
    assert!(
        result["snapshot"]["session"]["entries"]
            .as_array()
            .map_or(0, |e| e.len())
            == 2,
        "the full transcript by default"
    );
    assert!(
        result["savedSessions"]
            .as_array()
            .is_some_and(|list| !list.is_empty()),
        "the picker list rides along"
    );

    // attachChat pushes the same tail window as an event, after the
    // response (daemon-host.ts L1459).
    let frames = sink.frames.lock().unwrap().clone();
    assert_eq!(frames.len(), 1, "one snapshot event after the answer");
    let event: DaemonEvent =
        serde_json::from_slice(frames[0].strip_suffix(b"\n").unwrap_or(&frames[0]))
            .expect("frame decodes");
    match event {
        DaemonEvent::Snapshot { snapshot, .. } => {
            assert_eq!(snapshot.session.meta.id, session_id);
        }
        other => panic!("expected a snapshot event, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn only_a_web_attach_may_window_the_transcript() {
    let dir = temp_data_dir("attach-window");
    let session_id = seed_chat(&dir);
    let host = Arc::new(Host::new());
    host.reload_settings(&dir).expect("settings load");

    // Web with a window: one entry, with the window markers.
    let mut state = DispatchState::new();
    state.set_sink(Arc::new(RecordingSink::default()));
    let result = success_result(call(
        &host,
        &dir,
        &mut state,
        request(
            "w1",
            &[
                ("clientKind", json!("web")),
                ("chatSessionId", json!(session_id)),
                ("entriesWindow", json!(1)),
            ],
        ),
    ));
    assert_eq!(result["snapshot"]["entriesFrom"], json!(1));
    assert_eq!(result["snapshot"]["totalEntries"], json!(2));
    assert_eq!(
        result["snapshot"]["session"]["entries"]
            .as_array()
            .map_or(0, |e| e.len()),
        1,
        "the tail window only"
    );

    // Terminal ignores the field entirely (L3317): full transcript, no
    // window markers.
    let mut state = DispatchState::new();
    state.set_sink(Arc::new(RecordingSink::default()));
    let result = success_result(call(
        &host,
        &dir,
        &mut state,
        request(
            "w2",
            &[
                ("clientKind", json!("terminal")),
                ("chatSessionId", json!(session_id)),
                ("entriesWindow", json!(1)),
            ],
        ),
    ));
    assert!(
        result["snapshot"]["entriesFrom"].is_null()
            || result["snapshot"].get("entriesFrom").is_none(),
        "a terminal attach never windows"
    );
    assert_eq!(
        result["snapshot"]["session"]["entries"]
            .as_array()
            .map_or(0, |e| e.len()),
        2
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn attach_validation_keeps_the_source_wording() {
    let dir = temp_data_dir("attach-validate");
    let session_id = seed_chat(&dir);
    let host = Arc::new(Host::new());
    host.reload_settings(&dir).expect("settings load");
    let mut state = DispatchState::new();

    // clientKind values (L3313).
    assert_eq!(
        failure_text(call(
            &host,
            &dir,
            &mut state,
            request(
                "v1",
                &[
                    ("clientKind", json!("mobile")),
                    ("chatSessionId", json!(session_id)),
                ],
            ),
        )),
        "clientKind must be terminal or web."
    );

    // entriesWindow bounds, web only (L3319).
    assert_eq!(
        failure_text(call(
            &host,
            &dir,
            &mut state,
            request(
                "v2",
                &[
                    ("clientKind", json!("web")),
                    ("chatSessionId", json!(session_id)),
                    ("entriesWindow", json!(0)),
                ],
            ),
        )),
        "entriesWindow must be an integer between 1 and 2000."
    );

    // An existing chat and a new one at once (L3334).
    assert_eq!(
        failure_text(call(
            &host,
            &dir,
            &mut state,
            request(
                "v3",
                &[
                    ("clientKind", json!("terminal")),
                    ("chatSessionId", json!(session_id)),
                    ("newChatSession", json!(true)),
                ],
            ),
        )),
        "Choose an existing chat or a new chat."
    );

    // A chat attachment cannot double as another session kind (L3337).
    assert_eq!(
        failure_text(call(
            &host,
            &dir,
            &mut state,
            request(
                "v4",
                &[
                    ("clientKind", json!("terminal")),
                    ("chatSessionId", json!(session_id)),
                    ("isolatedSession", json!(true)),
                ],
            ),
        )),
        "A chat attachment cannot also select a custodian, worker, or background session."
    );

    // Every attachment names what it attaches to (L3365).
    assert_eq!(
        failure_text(call(
            &host,
            &dir,
            &mut state,
            request("v5", &[("clientKind", json!("terminal"))]),
        )),
        "Choose a chat or a session to attach."
    );

    // workspaceRoot must be absolute (L4203).
    assert_eq!(
        failure_text(call(
            &host,
            &dir,
            &mut state,
            request(
                "v6",
                &[
                    ("clientKind", json!("terminal")),
                    ("chatSessionId", json!(session_id)),
                    ("workspaceRoot", json!("relative/path")),
                ],
            ),
        )),
        "workspaceRoot must be an absolute path on this host (got \"relative/path\"). Omit workspaceRoot to use this host's default workspace."
    );

    let _ = std::fs::remove_dir_all(&dir);
}
