//! `session.select-project` at the dispatch surface: project binding
//! for a not-yet-started chat.
//!
//! Source: legacy `daemon-host.ts` L3636, `agent-session.ts`
//! `selectProject` (L927), `projects/store.ts` `sessionProject` /
//! `resolveProjectSelection` / `readProjects` (read 2026-11-18). This
//! build writes the binding to the draft immediately (the source keeps
//! it in memory until the first message); a draft is list-invisible, so
//! the reader sees the same chat either way. The queued-turn check has
//! no equivalent without a runtime and arrives with it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};

use common::{pretty_session, temp_data_dir, uuid, write_session};
use openbutler_daemon::agent::{TurnExecutor, TurnRequest};
use openbutler_daemon::dispatch::{self, DispatchState, Host};
use openbutler_protocol::{DaemonRequest, DaemonResponse};
use serde_json::{Value, json};

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

    fn compact(self: Arc<Self>, _session_id: String, _recovery: bool) {}
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

/// Opens a draft through `session.new` so the connection has a chat.
fn open_draft(host: &Host, dir: &Path, state: &mut DispatchState) -> String {
    success_result(call(host, dir, state, request("0", "session.new", &[])));
    state.current_session().unwrap().to_owned()
}

fn read_record(dir: &Path, id: &str) -> Value {
    let path = dir.join("sessions").join(id).join(format!("{id}.json"));
    let text = std::fs::read_to_string(&path).expect("record readable");
    serde_json::from_str(&text).expect("record is valid JSON")
}

/// The source's `realpathSync` never carries Windows' extended prefix,
/// and neither do the stored paths (`state::strip_windows_prefix`), so
/// expectations spell the same form `std::canonicalize` would not.
fn realpath(path: &Path) -> String {
    let text = path.canonicalize().unwrap().display().to_string();
    text.strip_prefix(r"\\?\")
        .map(str::to_owned)
        .unwrap_or(text)
}

#[test]
fn select_project_requires_a_literal_null_or_string_project_id() {
    let dir = temp_data_dir("proj-absent");
    let host = Host::new();
    let mut state = DispatchState::new();
    let id = open_draft(&host, &dir, &mut state);

    // Absent throws (the source only lets literal null or a string
    // through: `undefined !== null && typeof !== 'string'`).
    let response = call(
        &host,
        &dir,
        &mut state,
        request("1", "session.select-project", &[("sessionId", json!(id))]),
    );
    assert_eq!(failure_text(response), "Invalid project ID.");

    let response = call(
        &host,
        &dir,
        &mut state,
        request(
            "2",
            "session.select-project",
            &[("sessionId", json!(id)), ("projectId", json!(7))],
        ),
    );
    assert_eq!(failure_text(response), "Invalid project ID.");
}

#[test]
fn select_project_guards_the_active_chat_and_a_started_chat() {
    let dir = temp_data_dir("proj-guards");
    let host = Host::new();
    let mut state = DispatchState::new();
    let id = open_draft(&host, &dir, &mut state);

    let response = call(
        &host,
        &dir,
        &mut state,
        request(
            "1",
            "session.select-project",
            &[
                ("sessionId", json!("some-other-chat")),
                ("projectId", Value::Null),
            ],
        ),
    );
    assert_eq!(
        failure_text(response),
        "The active chat changed. Try again."
    );

    // A chat that already carries a user message refuses the binding.
    let started = uuid(9);
    write_session(
        &dir,
        &started,
        &pretty_session(&started, None, "Started", true),
    );
    let response = call(
        &host,
        &dir,
        &mut state,
        request("2", "session.load", &[("sessionId", json!(started))]),
    );
    success_result(response);
    let response = call(
        &host,
        &dir,
        &mut state,
        request(
            "3",
            "session.select-project",
            &[("sessionId", json!(started)), ("projectId", Value::Null)],
        ),
    );
    assert_eq!(
        failure_text(response),
        "This chat's project was fixed by its first message. Start a new chat."
    );
    let _ = id;
}

#[test]
fn select_project_refuses_while_a_run_is_live() {
    let dir = temp_data_dir("proj-busy");
    let host = Host::new();
    let executor = RecordingExecutor::default();
    *executor.busy.lock().unwrap() = true;
    host.set_executor(Arc::new(executor));
    let mut state = DispatchState::new();
    let id = open_draft(&host, &dir, &mut state);

    let response = call(
        &host,
        &dir,
        &mut state,
        request(
            "1",
            "session.select-project",
            &[("sessionId", json!(id)), ("projectId", Value::Null)],
        ),
    );
    assert_eq!(
        failure_text(response),
        "Wait for the running task to finish before choosing a project."
    );
}

#[test]
fn select_project_binds_null_to_the_data_workspace() {
    let dir = temp_data_dir("proj-null");
    let host = Host::new();
    let mut state = DispatchState::new();
    let id = open_draft(&host, &dir, &mut state);

    let response = call(
        &host,
        &dir,
        &mut state,
        request(
            "1",
            "session.select-project",
            &[("sessionId", json!(id)), ("projectId", Value::Null)],
        ),
    );
    assert_eq!(success_result(response), json!({ "accepted": true }));

    // The data-dir workspace folder is created and the binding lands on
    // the draft record.
    let workspace = dir.join("workspace");
    assert!(workspace.is_dir(), "data/workspace is created");
    let record = read_record(&dir, &id);
    let project = &record["project"];
    assert_eq!(project["id"], Value::Null);
    assert_eq!(project["name"], Value::Null);
    assert_eq!(project["folders"], json!([]));
    assert_eq!(project["locked"], json!(false));
    assert_eq!(project["workspaceRoot"], json!(realpath(&workspace)));
}

#[test]
fn select_project_resolves_a_stored_project_or_refuses() {
    let dir = temp_data_dir("proj-stored");
    let host = Host::new();
    let mut state = DispatchState::new();
    let id = open_draft(&host, &dir, &mut state);

    // Unknown id first: the store answers from projects.json.
    let response = call(
        &host,
        &dir,
        &mut state,
        request(
            "1",
            "session.select-project",
            &[
                ("sessionId", json!(id)),
                ("projectId", json!("11111111-2222-3333-4444-555555555555")),
            ],
        ),
    );
    assert_eq!(failure_text(response), "Project not found.");

    let folder = dir.join("proj-folder");
    std::fs::create_dir_all(&folder).unwrap();
    let canonical = realpath(&folder);
    let projects = json!({
        "version": 1,
        "projects": [{
            "id": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            "name": "Rust backend",
            "createdAt": "2026-11-18T00:00:00.000Z",
            "folders": [canonical],
        }],
    });
    std::fs::write(
        dir.join("projects.json"),
        serde_json::to_vec_pretty(&projects).unwrap(),
    )
    .unwrap();

    let response = call(
        &host,
        &dir,
        &mut state,
        request(
            "2",
            "session.select-project",
            &[
                ("sessionId", json!(id)),
                ("projectId", json!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")),
            ],
        ),
    );
    assert_eq!(success_result(response), json!({ "accepted": true }));
    let record = read_record(&dir, &id);
    let project = &record["project"];
    assert_eq!(project["id"], json!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"));
    assert_eq!(project["name"], json!("Rust backend"));
    assert_eq!(project["folders"], json!([canonical]));
}
