//! `session.new` / `session.flush` / `session.add-entry` and the three
//! `runtime.save-*` preference arms at the dispatch surface — the
//! disk-side chat mutations.
//!
//! Source: legacy `daemon-host.ts` L3643-3702 (all six arms) and
//! `parseSessionEntry` L4127 (read 2026-11-18), `agent-session.ts`
//! `addEntry` (L741), `flushSave` (L734), `setReasoningPreference`
//! (L1178); `runtime.save-approval-mode` persists machine-wide through
//! the settings store (`applyApprovalMode(mode, true)`).
//!
//! One documented adaptation: this build has no background save queue —
//! each mutation writes synchronously, so failures surface at the arm
//! instead of being deferred to `sessionSaveError`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};

use common::{pretty_session, temp_data_dir, uuid, write_session};
use openbutler_daemon::agent::{TurnExecutor, TurnRequest};
use openbutler_daemon::dispatch::{self, DispatchState, Host};
use openbutler_protocol::{DaemonRequest, DaemonResponse};
use serde_json::{Value, json};

/// The seam's test double: busy on demand.
#[derive(Clone, Default)]
struct RecordingExecutor {
    busy: Arc<Mutex<bool>>,
}

impl TurnExecutor for RecordingExecutor {
    fn spawn_turn(self: Arc<Self>, _session_id: String, _request: TurnRequest) {}

    fn approve(&self, _session_id: &str, _approval_id: &str, _approved: bool) -> bool {
        false
    }

    fn stop(&self, _session_id: &str, _keep_queue: bool) -> bool {
        false
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
        *self.busy.lock().unwrap()
    }
}

/// A sink that accepts every frame: the snapshot-event gate requires
/// one, exactly like a live connection has.
struct AcceptSink;

impl openbutler_daemon::dispatch::fanout::EventSink for AcceptSink {
    fn try_send(&self, _frame: Vec<u8>) -> bool {
        true
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

/// The saved record as raw JSON (the record on disk is the whole truth).
fn read_record(dir: &Path, id: &str) -> Value {
    let path = dir.join("sessions").join(id).join(format!("{id}.json"));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("record {id} unreadable at {}: {error}", path.display()));
    serde_json::from_str(&text).expect("record is valid JSON")
}

/// Loads an existing chat so the connection has a current session.
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
fn new_refuses_while_the_current_chat_runs() {
    let dir = temp_data_dir("sw-new-busy");
    let host = Host::new();
    let executor = RecordingExecutor::default();
    *executor.busy.lock().unwrap() = true;
    host.set_executor(Arc::new(executor));
    let mut state = DispatchState::new();
    let id = uuid(1);
    write_session(&dir, &id, &pretty_session(&id, None, "Busy chat", true));
    load_chat(&host, &dir, &mut state, &id);

    let response = call(&host, &dir, &mut state, request("1", "session.new", &[]));
    assert_eq!(
        failure_text(response),
        "Wait for the current run before creating a new chat."
    );
}

#[test]
fn new_creates_an_invisible_draft_and_points_the_connection_at_it() {
    let dir = temp_data_dir("sw-new-ok");
    let host = Host::new();
    let mut state = DispatchState::new();
    state.set_sink(Arc::new(AcceptSink));

    let response = call(&host, &dir, &mut state, request("1", "session.new", &[]));
    assert_eq!(success_result(response), json!({ "accepted": true }));

    let id = state
        .current_session()
        .expect("the connection now has a chat")
        .to_owned();
    let record = read_record(&dir, &id);
    assert_eq!(record["version"], 1);
    assert_eq!(record["title"], "New session");
    assert_eq!(record["entries"], json!([]));
    assert!(record["keyPositions"].is_object());

    // A draft never appears in the list (list.rs drops records without a
    // user message) — the same user-visible rule as an unsaved controller.
    let listed = success_result(call(
        &host,
        &dir,
        &mut state,
        request("2", "session.list", &[]),
    ));
    let ids: Vec<String> = listed["chats"]
        .as_array()
        .map(|chats| {
            chats
                .iter()
                .map(|chat| chat["id"].as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        !ids.contains(&id),
        "draft must stay out of the list: {ids:?}"
    );

    // The attach snapshot event rides behind the answer (L1459).
    assert_eq!(state.take_pending_events().len(), 1);
}

#[test]
fn new_rejects_a_non_string_project_id() {
    let dir = temp_data_dir("sw-new-proj");
    let host = Host::new();
    let mut state = DispatchState::new();

    let response = call(
        &host,
        &dir,
        &mut state,
        request("1", "session.new", &[("projectId", json!(7))]),
    );
    assert_eq!(failure_text(response), "Invalid project ID.");

    // A string (or null) is accepted; projects are not implemented in this
    // build, so a valid id is carried and ignored.
    let response = call(
        &host,
        &dir,
        &mut state,
        request("2", "session.new", &[("projectId", json!("proj-1"))]),
    );
    assert_eq!(success_result(response), json!({ "accepted": true }));
}

#[test]
fn add_entry_needs_an_open_chat() {
    let dir = temp_data_dir("sw-entry-open");
    let host = Host::new();
    let mut state = DispatchState::new();

    let response = call(
        &host,
        &dir,
        &mut state,
        request(
            "1",
            "session.add-entry",
            &[("entry", json!({ "kind": "notice", "text": "hi" }))],
        ),
    );
    assert_eq!(failure_text(response), "Chat not found.");
}

#[test]
fn add_entry_rejects_malformed_entries_with_the_source_messages() {
    let dir = temp_data_dir("sw-entry-bad");
    let host = Host::new();
    let mut state = DispatchState::new();
    let id = uuid(2);
    write_session(&dir, &id, &pretty_session(&id, None, "Chat", true));
    load_chat(&host, &dir, &mut state, &id);

    let cases: [(&str, Value, &str); 4] = [
        ("not-a-record", json!(42), "Session entry is invalid."),
        (
            "tool-without-result",
            json!({ "kind": "tool" }),
            "Tool session entry is invalid.",
        ),
        (
            "user-without-text",
            json!({ "kind": "user" }),
            "User session entry is invalid.",
        ),
        (
            "unsupported-kind",
            json!({ "kind": "divider" }),
            "Session entry kind is not supported.",
        ),
    ];
    for (name, entry, expected) in cases {
        let response = call(
            &host,
            &dir,
            &mut state,
            request("1", "session.add-entry", &[("entry", entry)]),
        );
        assert_eq!(failure_text(response), expected, "case {name}");
    }
}

#[test]
fn add_entry_appends_a_notice_immediately() {
    let dir = temp_data_dir("sw-entry-notice");
    let host = Host::new();
    let mut state = DispatchState::new();
    let id = uuid(3);
    write_session(&dir, &id, &pretty_session(&id, None, "Chat", true));
    load_chat(&host, &dir, &mut state, &id);

    let entry = json!({
        "kind": "notice",
        "text": "slash output",
        "detail": "d".repeat(3000),
        "tone": "warn",
    });
    let response = call(
        &host,
        &dir,
        &mut state,
        request("1", "session.add-entry", &[("entry", entry)]),
    );
    assert_eq!(success_result(response), json!({ "accepted": true }));

    let record = read_record(&dir, &id);
    let entries = record["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "the notice lands on disk immediately");
    let notice = entries.last().unwrap();
    assert_eq!(notice["kind"], "notice");
    assert_eq!(notice["text"], "slash output");
    assert_eq!(
        notice["detail"].as_str().unwrap().chars().count(),
        2000,
        "detail is sliced to 2000 like the source"
    );
    assert_eq!(notice["tone"], "warn");
    assert!(notice["at"].is_i64(), "the entry is stamped");
    assert!(
        notice.get("turnId").is_none(),
        "no active turn on this path"
    );
}

#[test]
fn add_entry_gates_everything_but_notices_on_a_view_only_chat() {
    let dir = temp_data_dir("sw-entry-view");
    let host = Host::new();
    let mut state = DispatchState::new();
    let id = uuid(4);
    write_session(&dir, &id, &pretty_session(&id, None, "Chat", true));
    load_chat(&host, &dir, &mut state, &id);
    state.read_only = true;

    let response = call(
        &host,
        &dir,
        &mut state,
        request(
            "1",
            "session.add-entry",
            &[("entry", json!({ "kind": "user", "text": "not allowed" }))],
        ),
    );
    assert_eq!(
        failure_text(response),
        "This subagent session is view-only."
    );

    let response = call(
        &host,
        &dir,
        &mut state,
        request(
            "2",
            "session.add-entry",
            &[(
                "entry",
                json!({ "kind": "notice", "text": "viewer warning" }),
            )],
        ),
    );
    assert_eq!(success_result(response), json!({ "accepted": true }));
}

#[test]
fn save_reasoning_persists_with_the_chat() {
    let dir = temp_data_dir("sw-reasoning");
    let host = Host::new();
    let mut state = DispatchState::new();
    let id = uuid(5);
    write_session(&dir, &id, &pretty_session(&id, None, "Chat", true));
    load_chat(&host, &dir, &mut state, &id);

    let response = call(
        &host,
        &dir,
        &mut state,
        request(
            "1",
            "runtime.save-reasoning",
            &[("variantId", json!("high"))],
        ),
    );
    assert_eq!(success_result(response), json!({ "saved": true }));
    let record = read_record(&dir, &id);
    assert_eq!(record["reasoning"], json!("high"));

    // Same value: a no-op — nothing is rewritten (updatedAt stays put).
    let before = record["updatedAt"].clone();
    let response = call(
        &host,
        &dir,
        &mut state,
        request(
            "2",
            "runtime.save-reasoning",
            &[("variantId", json!("high"))],
        ),
    );
    assert_eq!(success_result(response), json!({ "saved": true }));
    let record = read_record(&dir, &id);
    assert_eq!(record["updatedAt"], before, "same value writes nothing");
}

#[test]
fn save_reasoning_validates_the_variant_id() {
    let dir = temp_data_dir("sw-reasoning-bad");
    let host = Host::new();
    let mut state = DispatchState::new();
    let id = uuid(6);
    write_session(&dir, &id, &pretty_session(&id, None, "Chat", true));
    load_chat(&host, &dir, &mut state, &id);

    let response = call(
        &host,
        &dir,
        &mut state,
        request("1", "runtime.save-reasoning", &[("variantId", json!(42))]),
    );
    assert_eq!(
        failure_text(response),
        "variantId must be a non-empty string under 256 bytes."
    );
}

#[test]
fn save_show_reasoning_toggles_with_the_chat() {
    let dir = temp_data_dir("sw-show");
    let host = Host::new();
    let mut state = DispatchState::new();
    let id = uuid(7);
    write_session(&dir, &id, &pretty_session(&id, None, "Chat", true));
    load_chat(&host, &dir, &mut state, &id);

    let response = call(
        &host,
        &dir,
        &mut state,
        request(
            "1",
            "runtime.save-show-reasoning",
            &[("visible", json!("yes"))],
        ),
    );
    assert_eq!(failure_text(response), "visible must be a boolean.");

    let response = call(
        &host,
        &dir,
        &mut state,
        request(
            "2",
            "runtime.save-show-reasoning",
            &[("visible", json!(false))],
        ),
    );
    assert_eq!(success_result(response), json!({ "saved": true }));
    let record = read_record(&dir, &id);
    assert_eq!(record["showReasoning"], json!(false));
}

#[test]
fn save_approval_mode_lands_in_settings_and_the_live_copy() {
    let dir = temp_data_dir("sw-mode");
    std::fs::write(dir.join("settings.yaml"), "approvalMode: workspace\n").unwrap();
    let host = Host::new();
    host.reload_settings(&dir).unwrap();
    let mut state = DispatchState::new();

    let response = call(
        &host,
        &dir,
        &mut state,
        request(
            "1",
            "runtime.save-approval-mode",
            &[("mode", json!("chaos"))],
        ),
    );
    assert_eq!(
        failure_text(response),
        "mode must be workspace, auto-review, or full-access."
    );

    let response = call(
        &host,
        &dir,
        &mut state,
        request(
            "2",
            "runtime.save-approval-mode",
            &[("mode", json!("full-access"))],
        ),
    );
    assert_eq!(success_result(response), json!({ "saved": true }));

    let yaml = std::fs::read_to_string(dir.join("settings.yaml")).unwrap();
    assert!(yaml.contains("approvalMode: full-access"), "yaml: {yaml}");
    let settings = host.settings();
    let loaded = settings.as_ref().expect("settings stay loaded");
    assert_eq!(
        format!("{:?}", loaded.store.approval_mode),
        "FullAccess",
        "the live copy reloaded"
    );
}

#[test]
fn flush_passes_for_an_open_chat_and_reports_a_missing_one() {
    let dir = temp_data_dir("sw-flush");
    let host = Host::new();
    let mut state = DispatchState::new();
    let id = uuid(8);
    write_session(&dir, &id, &pretty_session(&id, None, "Chat", true));

    let response = call(&host, &dir, &mut state, request("1", "session.flush", &[]));
    assert_eq!(failure_text(response), "Chat not found.");

    load_chat(&host, &dir, &mut state, &id);
    let response = call(&host, &dir, &mut state, request("2", "session.flush", &[]));
    assert_eq!(success_result(response), json!({ "saved": true }));
}
