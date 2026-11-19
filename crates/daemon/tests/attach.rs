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

#[test]
fn attach_boolean_fields_keep_their_own_wording() {
    let dir = temp_data_dir("attach-booleans");
    let session_id = seed_chat(&dir);
    let host = Arc::new(Host::new());
    host.reload_settings(&dir).expect("settings load");
    let mut state = DispatchState::new();

    // isolatedSession may be any boolean (L3327): `false` selects
    // nothing, so the request falls to the nothing-selected sentence —
    // not to the boolean refusal.
    assert_eq!(
        failure_text(call(
            &host,
            &dir,
            &mut state,
            request(
                "b1",
                &[
                    ("clientKind", json!("terminal")),
                    ("isolatedSession", json!(false)),
                ],
            ),
        )),
        "Choose a chat or a session to attach."
    );

    // newChatSession is exactly `true` or absent (L3346).
    assert_eq!(
        failure_text(call(
            &host,
            &dir,
            &mut state,
            request(
                "b2",
                &[
                    ("clientKind", json!("terminal")),
                    ("newChatSession", json!(false)),
                ],
            ),
        )),
        "newChatSession must be true."
    );

    // The new-chat branch carries the chat path's exclusions (L3333):
    // a new chat cannot double as a custodian, worker, or background
    // session either.
    assert_eq!(
        failure_text(call(
            &host,
            &dir,
            &mut state,
            request(
                "b3",
                &[
                    ("clientKind", json!("terminal")),
                    ("newChatSession", json!(true)),
                    ("isolatedSession", json!(true)),
                ],
            ),
        )),
        "A chat attachment cannot also select a custodian, worker, or background session."
    );

    // The workspaceRoot string gate runs before the path shape (L4202):
    // non-string and empty are the requireString sentence.
    assert_eq!(
        failure_text(call(
            &host,
            &dir,
            &mut state,
            request(
                "b4",
                &[
                    ("clientKind", json!("terminal")),
                    ("chatSessionId", json!(session_id)),
                    ("workspaceRoot", json!(42)),
                ],
            ),
        )),
        "workspaceRoot must be a non-empty string under 32768 bytes."
    );
    assert_eq!(
        failure_text(call(
            &host,
            &dir,
            &mut state,
            request(
                "b5",
                &[
                    ("clientKind", json!("terminal")),
                    ("chatSessionId", json!(session_id)),
                    ("workspaceRoot", json!("")),
                ],
            ),
        )),
        "workspaceRoot must be a non-empty string under 32768 bytes."
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------
// The binding gate. Source: daemon-host.ts — the pre-attach partition
// (L3179-3311), the attach handler (L3312), the workspaceKey gate and
// attachment match (L3478-3486), the session.command resolution that
// runs only after the gate (L3492), the view-only gate (L3497), and
// session.load re-attaching through attachChat (L3621). Read
// 2026-11-19. Deliberately not here: the isolated/schedule/worker
// variant success paths — they need the M2 session runtime.

fn action_request(id: &str, action: &str, pairs: &[(&str, Value)]) -> DaemonRequest {
    let mut request = DaemonRequest::new(id, action);
    for (key, value) in pairs {
        request = request.with(*key, value.clone());
    }
    request
}

/// Attaches the seeded chat and returns the workspaceKey the attach
/// answered with — what a real client stores and echoes back.
fn attach_and_key(
    host: &Host,
    data_dir: &Path,
    state: &mut DispatchState,
    session_id: &str,
) -> String {
    let response = call(
        host,
        data_dir,
        state,
        request(
            "attach-boot",
            &[
                ("clientKind", json!("terminal")),
                ("chatSessionId", json!(session_id)),
            ],
        ),
    );
    success_result(response)["workspaceKey"]
        .as_str()
        .expect("the attach response carries a workspaceKey")
        .to_owned()
}

#[test]
fn the_binding_gate_runs_before_any_handler_reads_the_request() {
    let dir = temp_data_dir("gate-order");
    let host = Arc::new(Host::new());
    host.reload_settings(&dir).expect("settings load");
    let mut state = DispatchState::new();

    // No connection: even with a well-formed key the match refuses
    // before the handler would read anything — an empty command name
    // would say something else if resolution ran first.
    assert_eq!(
        failure_text(call(
            &host,
            &dir,
            &mut state,
            action_request(
                "g1",
                "session.command",
                &[("name", json!("")), ("workspaceKey", json!("/tmp"))],
            ),
        )),
        "This IPC client is not attached to that workspace."
    );
    // A valid name does not matter either: an unattached connection is
    // refused before the registry is consulted.
    assert_eq!(
        failure_text(call(
            &host,
            &dir,
            &mut state,
            action_request("g2", "session.command", &[("name", json!("/compact"))]),
        )),
        "workspaceKey must be a non-empty string under 32768 bytes."
    );
    // A key on an unattached connection answers the match refusal.
    assert_eq!(
        failure_text(call(
            &host,
            &dir,
            &mut state,
            action_request("g3", "session.stop", &[("workspaceKey", json!("/tmp"))]),
        )),
        "This IPC client is not attached to that workspace."
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_bound_connection_answers_only_with_its_own_workspace_key() {
    let dir = temp_data_dir("gate-match");
    let session_id = seed_chat(&dir);
    let host = Arc::new(Host::new());
    host.reload_settings(&dir).expect("settings load");
    let mut state = DispatchState::new();
    let key = attach_and_key(&host, &dir, &mut state, &session_id);

    // A different key is another workspace: refused without reading.
    assert_eq!(
        failure_text(call(
            &host,
            &dir,
            &mut state,
            action_request(
                "m1",
                "session.list",
                &[("workspaceKey", json!("/somewhere/else"))]
            ),
        )),
        "This IPC client is not attached to that workspace."
    );
    // The key the attach answered with is accepted — and the string gate
    // still runs first, so an oversized key is a string error even here.
    let attached = call(
        &host,
        &dir,
        &mut state,
        action_request(
            "m2",
            "session.list",
            &[("workspaceKey", json!(key.clone()))],
        ),
    );
    assert!(matches!(attached, DaemonResponse::Success { .. }));
    let oversized = "x".repeat(32769);
    assert_eq!(
        failure_text(call(
            &host,
            &dir,
            &mut state,
            action_request("m3", "session.list", &[("workspaceKey", json!(oversized))]),
        )),
        "workspaceKey must be a non-empty string under 32768 bytes."
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The identity key case-folds on Windows only (scopeMapKey,
/// workspace-origin.ts L35): POSIX keeps the exact string.
#[cfg(windows)]
#[test]
fn the_workspace_key_matches_case_insensitively_on_windows() {
    let dir = temp_data_dir("gate-case");
    let session_id = seed_chat(&dir);
    let host = Arc::new(Host::new());
    host.reload_settings(&dir).expect("settings load");
    let mut state = DispatchState::new();
    let key = attach_and_key(&host, &dir, &mut state, &session_id);

    let response = call(
        &host,
        &dir,
        &mut state,
        action_request(
            "c1",
            "session.list",
            &[("workspaceKey", json!(key.to_uppercase()))],
        ),
    );
    assert!(
        matches!(response, DaemonResponse::Success { .. }),
        "a case-folded key must match on Windows, got {response:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn host_wide_actions_never_need_a_binding() {
    let dir = temp_data_dir("gate-pre-attach");
    let host = Arc::new(Host::new());
    host.reload_settings(&dir).expect("settings load");
    let mut state = DispatchState::new();

    // The pre-attach partition answers with no workspaceKey at all.
    let status = call(&host, &dir, &mut state, action_request("p1", "status", &[]));
    assert!(matches!(status, DaemonResponse::Success { .. }));
    // session.list is host-wide only while the connection is unattached
    // (L3303): the whole picker, no binding.
    let list = call(
        &host,
        &dir,
        &mut state,
        action_request("p2", "session.list", &[]),
    );
    assert!(matches!(list, DaemonResponse::Success { .. }));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_read_only_connection_may_read_but_not_send() {
    let dir = temp_data_dir("gate-readonly");
    let session_id = seed_chat(&dir);
    let host = Arc::new(Host::new());
    host.reload_settings(&dir).expect("settings load");
    let mut state = DispatchState::new();
    let key = attach_and_key(&host, &dir, &mut state, &session_id);
    state.read_only = true;

    // Not a view-only action: refused by the gate, before the handler
    // would even parse the message.
    assert_eq!(
        failure_text(call(
            &host,
            &dir,
            &mut state,
            action_request(
                "ro1",
                "session.send",
                &[("workspaceKey", json!(key.clone()))]
            ),
        )),
        "This subagent session is view-only."
    );
    // The view-only allow-list keeps reading and closing (L139).
    let listed = call(
        &host,
        &dir,
        &mut state,
        action_request("ro2", "session.list", &[("workspaceKey", json!(key))]),
    );
    assert!(matches!(listed, DaemonResponse::Success { .. }));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_attached_session_list_scopes_to_the_bound_workspace() {
    let dir = temp_data_dir("gate-list-scope");
    let chat_a = common::uuid(1);
    let chat_b = common::uuid(2);
    common::write_session(
        &dir,
        &chat_a,
        &common::pretty_session(&chat_a, Some(r"C:\ws\alpha"), "Alpha", true),
    );
    common::write_session(
        &dir,
        &chat_b,
        &common::pretty_session(&chat_b, Some(r"C:\ws\beta"), "Beta", true),
    );
    let host = Arc::new(Host::new());
    host.reload_settings(&dir).expect("settings load");

    // Unattached: the whole picker.
    let mut browsing = DispatchState::new();
    let all = success_result(call(
        &host,
        &dir,
        &mut browsing,
        action_request("s0", "session.list", &[]),
    ));
    let ids: Vec<&str> = all
        .as_array()
        .expect("session.list answers an array")
        .iter()
        .filter_map(|entry| entry["id"].as_str())
        .collect();
    assert!(ids.contains(&chat_a.as_str()) && ids.contains(&chat_b.as_str()));

    // Attached: only the bound workspace's sessions (L3584 lists the
    // attachment's controller, one workspace).
    let mut attached = DispatchState::new();
    let key = attach_and_key(&host, &dir, &mut attached, &chat_a);
    let scoped = success_result(call(
        &host,
        &dir,
        &mut attached,
        action_request("s1", "session.list", &[("workspaceKey", json!(key))]),
    ));
    let ids: Vec<&str> = scoped
        .as_array()
        .expect("session.list answers an array")
        .iter()
        .filter_map(|entry| entry["id"].as_str())
        .collect();
    assert!(
        ids.contains(&chat_a.as_str()),
        "the bound workspace's own chat must be listed, got {ids:?}"
    );
    assert!(
        !ids.contains(&chat_b.as_str()),
        "another workspace's chat must not leak into the scoped list, got {ids:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn session_load_rebinds_the_connection_to_the_loaded_chat() {
    let dir = temp_data_dir("gate-rebind");
    let chat_a = common::uuid(3);
    let chat_b = common::uuid(4);
    common::write_session(
        &dir,
        &chat_a,
        &common::pretty_session(&chat_a, Some(r"C:\ws\first"), "First", true),
    );
    common::write_session(
        &dir,
        &chat_b,
        &common::pretty_session(&chat_b, Some(r"C:\ws\second"), "Second", true),
    );
    let host = Arc::new(Host::new());
    host.reload_settings(&dir).expect("settings load");
    let mut state = DispatchState::new();
    let key_a = attach_and_key(&host, &dir, &mut state, &chat_a);

    // The load itself rides the current binding (L3621 is post-attach).
    let loaded = call(
        &host,
        &dir,
        &mut state,
        action_request(
            "l1",
            "session.load",
            &[("workspaceKey", json!(key_a)), ("sessionId", json!(chat_b))],
        ),
    );
    assert!(matches!(loaded, DaemonResponse::Success { .. }));

    // The connection now belongs to the loaded chat's workspace: the old
    // key is another workspace's key, the loaded chat's root answers.
    assert_eq!(
        state.binding_key.as_deref(),
        Some(r"C:\ws\second"),
        "session.load rebinds through attachChat (L3633: bindingKey = chat.workspace.root)"
    );
    let stale = call(
        &host,
        &dir,
        &mut state,
        action_request(
            "l2",
            "session.list",
            &[("workspaceKey", json!(r"C:\ws\first"))],
        ),
    );
    assert_eq!(
        failure_text(stale),
        "This IPC client is not attached to that workspace."
    );
    let fresh = call(
        &host,
        &dir,
        &mut state,
        action_request(
            "l3",
            "session.list",
            &[("workspaceKey", json!(r"C:\ws\second"))],
        ),
    );
    assert!(matches!(fresh, DaemonResponse::Success { .. }));

    let _ = std::fs::remove_dir_all(&dir);
}
