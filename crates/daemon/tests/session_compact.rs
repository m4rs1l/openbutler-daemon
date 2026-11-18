//! `session.compact` at the dispatch surface: the validation, the
//! fire-and-forget acceptance, and the standalone compaction through
//! the production executor (fake sidecar).
//!
//! Source: legacy `daemon-host.ts` case `session.compact` (L3514) and
//! `agent-session.ts` `compact()` (L2737) / `applyCompaction` (L2849),
//! read 2026-11-18. Deferred work recorded in `docs/context-compaction.md`:
//! mid-run queue serving at the step boundary and the streamed
//! checkpoint deltas arrive with the auto-trigger slice.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{temp_data_dir, uuid, write_session};
use openbutler_daemon::agent::executor::DoorExecutor;
use openbutler_daemon::agent::{TurnExecutor, TurnRequest};
use openbutler_daemon::dispatch::{self, DispatchState, Host};
use openbutler_daemon::sidecar::DoorConfig;
use openbutler_protocol::{DaemonRequest, DaemonResponse};
use serde_json::{Value, json};

/// The seam's test double: records what dispatch handed over.
#[derive(Clone, Default)]
struct RecordingExecutor {
    compacts: Arc<Mutex<Vec<(String, bool)>>>,
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
        false
    }

    fn compact(self: Arc<Self>, session_id: String, recovery: bool) {
        self.compacts.lock().unwrap().push((session_id, recovery));
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

/// A chat with a resolvable provider and a non-empty conversation.
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
        "{{\n  \"version\": 1,\n  \"id\": \"{id}\",\n  \"title\": \"compact chat\",\n  \"createdAt\": \"2026-11-18T00:00:00.000Z\",\n  \"updatedAt\": \"2026-11-18T00:00:00.000Z\",\n  \"providerId\": \"mock\",\n  \"modelId\": \"test-model\",\n  \"contextTokens\": 12345,\n  \"workspaceRoot\": \"work/chat\",\n  \"entries\": [\n    {{\n      \"kind\": \"user\",\n      \"text\": \"start the work\",\n      \"source\": \"terminal\",\n      \"turnId\": \"turn-1\"\n    }},\n    {{\n      \"kind\": \"assistant\",\n      \"text\": \"working on it\"\n    }}\n  ],\n  \"conversation\": [\n    {{ \"role\": \"user\", \"content\": \"start the work\" }},\n    {{ \"role\": \"assistant\", \"content\": \"{{{{\\\"type\\\":\\\"text\\\",\\\"text\\\":\\\"working on it\\\"}}}}\" }}\n  ]\n}}\n"
    );
    write_session(dir, &id, &body);
    id
}

fn read_record(dir: &Path, id: &str) -> Value {
    let path = dir.join("sessions").join(id).join(format!("{id}.json"));
    let text = std::fs::read_to_string(&path).expect("record readable");
    serde_json::from_str(&text).expect("record is valid JSON")
}

fn compact_request(recovery: Option<Value>) -> DaemonRequest {
    match recovery {
        Some(value) => request("c1", "session.compact", &[("recovery", value)]),
        None => request("c1", "session.compact", &[]),
    }
}

/// One `openbutler_usage` row as the compaction test reads it: session,
/// kind, attempt, status, the three token counts, usage source, provider,
/// model.
type CompactionUsageRow = (
    String,
    String,
    i64,
    String,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    String,
    String,
    String,
);

#[test]
fn recovery_must_be_a_boolean_when_present() {
    let dir = temp_data_dir("compact-validation");
    let host = Arc::new(Host::new());
    let recorder = RecordingExecutor::default();
    host.set_executor(Arc::new(recorder.clone()));
    let mut state = DispatchState::new();
    seed_chat(&dir);

    let response = call(&host, &dir, &mut state, compact_request(Some(json!("yes"))));
    assert_eq!(failure_text(response), "recovery must be a boolean.");
    assert!(
        recorder.compacts.lock().unwrap().is_empty(),
        "a refused request never reaches the seam"
    );
}

#[test]
fn accepted_compaction_reaches_the_seam_with_its_flag() {
    let dir = temp_data_dir("compact-accepted");
    let host = Arc::new(Host::new());
    let recorder = RecordingExecutor::default();
    host.set_executor(Arc::new(recorder.clone()));
    let mut state = DispatchState::new();
    let id = seed_chat(&dir);
    call(
        &host,
        &dir,
        &mut state,
        request("0", "session.load", &[("sessionId", json!(id))]),
    );

    let response = call(&host, &dir, &mut state, compact_request(None));
    assert_eq!(success_result(response), json!({ "accepted": true }));
    let response = call(&host, &dir, &mut state, compact_request(Some(json!(true))));
    assert_eq!(success_result(response), json!({ "accepted": true }));

    let recorded = recorder.compacts.lock().unwrap().clone();
    assert_eq!(
        recorded,
        vec![(id.clone(), false), (id, true)],
        "fire-and-forget with the source's recovery flag"
    );
}

fn fake_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-sidecar.mjs")
}

fn door_config(log_file: PathBuf) -> DoorConfig {
    DoorConfig {
        node_program: PathBuf::from("node"),
        node_args: Vec::new(),
        script: fake_script(),
        mode: String::new(),
        providers: Some(vec!["mock".to_owned()]),
        log_file: Some(log_file),
        env: Vec::new(),
        spawn_timeout: Duration::from_millis(4000),
        idle_kill: Duration::from_secs(3600),
        drain_max: Duration::from_millis(900),
        queue_max_wait: Duration::from_millis(2500),
        first_event_timeout: Duration::from_millis(700),
        inter_event_timeout: Duration::from_millis(700),
        concurrency: 4,
        line_cap: 64 * 1024,
    }
}

/// Polls the record until the predicate holds or the deadline passes.
/// Async sleep: a `std::thread::sleep` here would block the
/// current-thread runtime and starve the spawned compaction task.
async fn wait_for(dir: &Path, id: &str, what: &str, predicate: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..160 {
        let record = read_record(dir, id);
        if predicate(&record) {
            return record;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn a_standalone_compaction_swaps_the_conversation_and_lands_the_row() {
    let dir = temp_data_dir("compact-e2e");
    let id = seed_chat(&dir);
    let original_conversation = read_record(&dir, &id)["conversation"].clone();
    let log = dir.join("calls.log");
    let host = Arc::new(Host::new());
    let executor = DoorExecutor::new(dir.clone(), door_config(log.clone()), Arc::downgrade(&host));
    host.set_executor(Arc::new(executor));
    // Boot mirror: the daemon loads settings before serving, and the
    // compact path resolves the provider and window from them.
    host.reload_settings(&dir).expect("settings load");
    let mut state = DispatchState::new();
    call(
        &host,
        &dir,
        &mut state,
        request("0", "session.load", &[("sessionId", json!(id))]),
    );

    let response = call(&host, &dir, &mut state, compact_request(None));
    assert_eq!(success_result(response), json!({ "accepted": true }));

    let record = wait_for(&dir, &id, "the compaction row to settle", |record| {
        record["entries"].as_array().is_some_and(|entries| {
            entries
                .iter()
                .any(|entry| entry["kind"] == "compaction" && entry["status"] == "done")
        })
    })
    .await;

    let entries = record["entries"].as_array().unwrap();
    let row = entries
        .iter()
        .find(|entry| entry["kind"] == "compaction")
        .expect("row");
    assert_eq!(
        row["turnId"], "turn-1",
        "the row names the turn the transcript ends in"
    );
    assert!(
        row["beforeTokens"]
            .as_i64()
            .is_some_and(|tokens| tokens > 0),
        "before-tokens recorded: {row}"
    );
    assert!(
        row["afterTokens"].as_i64().is_some_and(|tokens| tokens > 0),
        "after-tokens recorded: {row}"
    );

    let conversation = record["conversation"].as_array().unwrap();
    assert!(
        conversation[0]["content"]
            .as_str()
            .is_some_and(|text| text.starts_with(
                "OpenButler conversation summary (context only):"
            )),
        "checkpoint first: {:?}",
        conversation[0]
    );
    assert_eq!(
        conversation.last().unwrap()["content"],
        json!(
            "Continue the unfinished user request from the compacted history. Use the summary to recover the current goal, constraints, completed work, and next action."
        ),
        "the continue message closes it"
    );
    assert_eq!(
        record["conversationArchive"], original_conversation,
        "the archive keeps the original transcript exactly once"
    );
    assert!(
        record["contextTokens"].is_null(),
        "the measured context dies with the rewrite"
    );

    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(
        calls.lines().filter(|line| !line.is_empty()).count(),
        1,
        "one summarisation call: {calls}"
    );
    assert!(
        calls.starts_with("compact:"),
        "call ids carry the compact tag: {calls}"
    );
    // The checkpoint text streamed while it was produced, tagged with
    // the compaction provenance so it can never read as an answer.
    let streamed = record["entries"].as_array().and_then(|entries| {
        entries
            .iter()
            .find(|entry| entry["kind"] == "assistant" && entry["internal"] == "compaction")
            .and_then(|entry| entry["text"].as_str())
    });
    assert_eq!(
        streamed,
        Some("Hello world"),
        "the streamed checkpoint entry carries the compaction provenance"
    );

    // The summarisation request is accounted for: one row, compaction
    // kind, the source's key and the tokens the fake stated
    // (legacy `onStepUsage(report, 'compaction')`).
    let connection = rusqlite::Connection::open(dir.join("state.db")).unwrap();
    let mut statement = connection
        .prepare(
            "SELECT session_id, run_kind, attempt, status, input_tokens, output_tokens,
                    total_tokens, usage_source, provider_id, model_id
             FROM openbutler_usage",
        )
        .unwrap();
    let rows: Vec<CompactionUsageRow> = statement
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "one summarisation request, one row: {rows:?}"
    );
    let row = &rows[0];
    assert_eq!(row.0, id, "the row names its session");
    assert_eq!(row.1, "compaction");
    assert_eq!(row.2, 1, "each summarisation request is its own run");
    assert_eq!(row.3, "ok");
    assert_eq!((row.4, row.5, row.6), (Some(10), Some(5), Some(15)));
    assert_eq!(row.7, "provider");
    assert_eq!(row.8, "mock", "the session's provider, as in the source");
    assert_eq!(row.9, "test-model");
}

#[tokio::test]
async fn an_empty_conversation_notices_and_calls_nothing() {
    let dir = temp_data_dir("compact-empty");
    let id = seed_chat(&dir);
    // Empty the conversation, keep the entries.
    let mut record = read_record(&dir, &id);
    record["conversation"] = json!([]);
    let path = dir.join("sessions").join(&id).join(format!("{id}.json"));
    std::fs::write(&path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();

    let log = dir.join("calls.log");
    let host = Arc::new(Host::new());
    let executor = DoorExecutor::new(dir.clone(), door_config(log.clone()), Arc::downgrade(&host));
    host.set_executor(Arc::new(executor));
    // Boot mirror: the daemon loads settings before serving, and the
    // compact path resolves the provider and window from them.
    host.reload_settings(&dir).expect("settings load");
    let mut state = DispatchState::new();
    call(
        &host,
        &dir,
        &mut state,
        request("0", "session.load", &[("sessionId", json!(id))]),
    );

    let response = call(&host, &dir, &mut state, compact_request(None));
    assert_eq!(success_result(response), json!({ "accepted": true }));

    let record = wait_for(&dir, &id, "the empty-conversation notice", |record| {
        record["entries"].as_array().is_some_and(|entries| {
            entries.iter().any(|entry| {
                entry["kind"] == "notice" && entry["text"] == "There is no conversation to compact."
            })
        })
    })
    .await;
    assert!(
        !record["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["kind"] == "compaction"),
        "no row when there was nothing to compact"
    );
    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        calls.trim().is_empty(),
        "the door was never called: {calls}"
    );
}
