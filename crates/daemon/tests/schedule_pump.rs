//! Sched-C: the event pump — `pumpEvents` + `processScheduleEvent`.
//!
//! Fail-before: these pins are written against `schedule::pump::pump_once`
//! and the two executor seams the pump needs (`has_live_session`,
//! `spawn_turn_for_event`). None of them exist yet.
//!
//! The pump tests the pump: turn execution itself is agent_loop's job, so
//! the executor here is a recording double that admits the wake text and
//! settles immediately.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use openbutler_daemon::agent::executor::TurnExecutor;
use openbutler_daemon::agent::{QueueAction, TurnRequest};
use openbutler_daemon::dispatch::Host;
use openbutler_daemon::ledger::{EventInput, LedgerWriter};
use openbutler_daemon::schedule::{self, ScheduleRuntimeStatus, read_schedule_runtime};
use openbutler_daemon::state;
use openbutler_daemon::state::record_path;
use openbutler_daemon::state::schedules::{ScheduleState, ScheduleTrigger};
use serde_json::json;

/// The origin session, background session id, and every admitted text —
/// recorded for the assertions.
#[derive(Default)]
struct PumpExecutor {
    spawned: Mutex<Vec<(String, String)>>,
    fail_next: Mutex<bool>,
    origins_alive: Mutex<Vec<String>>,
}

impl TurnExecutor for PumpExecutor {
    fn spawn_turn(self: Arc<Self>, _session_id: String, _request: TurnRequest) {}

    fn spawn_turn_for_event(
        self: Arc<Self>,
        session_id: String,
        request: TurnRequest,
    ) -> tokio::sync::oneshot::Receiver<Result<(), String>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let failing = std::mem::take(&mut *self.fail_next.lock().unwrap());
        if failing {
            let _ = tx.send(Err("The provider rejected the request.".to_owned()));
            return rx;
        }
        self.spawned
            .lock()
            .unwrap()
            .push((session_id, request.text.clone()));
        let _ = tx.send(Ok(()));
        rx
    }

    fn approve(&self, _session_id: &str, _approval_id: &str, _approved: bool) -> bool {
        false
    }
    fn stop(&self, _session_id: &str, _keep_queue: bool) -> bool {
        false
    }
    fn answer(&self, _session_id: &str, _question_id: &str, _answer: Option<String>) -> bool {
        false
    }
    fn is_busy(&self, _session_id: &str) -> bool {
        false
    }
    fn manage_queue(
        &self,
        _session_id: &str,
        _message_id: &str,
        _action: QueueAction,
    ) -> Result<(), String> {
        Err("No queued message with that id.".to_owned())
    }
    fn compact(self: Arc<Self>, _session_id: String, _recovery: bool) {}

    fn has_live_session(&self, session_id: &str) -> bool {
        self.origins_alive
            .lock()
            .unwrap()
            .iter()
            .any(|id| id == session_id)
    }
}

fn temp_data_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ob-pump-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// A daily schedule whose workspace lives under `workspace`.
fn daily_schedule(id: &str, workspace: &Path, origin: &str) -> ScheduleState {
    ScheduleState {
        id: id.to_owned(),
        name: format!("Sched {id}"),
        instruction: "Report the build status.".to_owned(),
        workspace_root: workspace.display().to_string(),
        origin_session_id: origin.to_owned(),
        enabled: true,
        trigger: ScheduleTrigger {
            adapter: "daily".to_owned(),
            options: json!({"at": "09:00"}),
        },
    }
}

fn queue_event(writer: &LedgerWriter, schedule_id: &str, facts: serde_json::Value) -> String {
    let (id, _) = writer
        .insert_event(&EventInput {
            source: "schedule".to_owned(),
            kind: "fired".to_owned(),
            dedupe_key: format!("schedule:{schedule_id}:manual:test-1"),
            workspace_root: None,
            occurred_at: None,
            payload: json!({"scheduleId": schedule_id, "facts": facts}),
        })
        .expect("insert event");
    id
}

fn runtime_status(writer: &LedgerWriter, schedule_id: &str) -> ScheduleRuntimeStatus {
    read_schedule_runtime(schedule_id, |key| writer.state_get(key).ok().flatten()).status
}

#[tokio::test]
async fn missing_workspace_fails_the_event_and_the_schedule() {
    let data_dir = temp_data_dir("missing");
    let schedule = daily_schedule(
        "abc123",
        Path::new("C:/definitely-gone-workspace"),
        "origin-gone",
    );
    state::schedules::save(&data_dir, &[schedule]).expect("save schedule");
    let writer = LedgerWriter::open(&data_dir).expect("ledger");
    let event_id = queue_event(
        &writer,
        "abc123",
        json!({"scheduledAt": "2026-11-18T09:00:00Z"}),
    );

    let executor = Arc::new(PumpExecutor::default());
    let host = Host::new();
    host.set_executor(executor.clone());

    schedule::pump::pump_once(&host, &data_dir)
        .await
        .expect("pump runs");

    // No turn ever spawned; the event failed with the source's message.
    assert!(executor.spawned.lock().unwrap().is_empty());
    let event = writer
        .get_event(&event_id)
        .expect("event read")
        .expect("event exists");
    assert_eq!(event.status, "failed");
    let error = event.error.unwrap_or_default();
    assert!(
        error.starts_with("Workspace is missing: "),
        "error = {error:?}"
    );
    // The schedule's durable runtime records the same failure.
    assert_eq!(
        runtime_status(&writer, "abc123"),
        ScheduleRuntimeStatus::Error
    );
    let runtime = read_schedule_runtime("abc123", |key| writer.state_get(key).ok().flatten());
    assert!(
        runtime
            .status_error
            .as_deref()
            .unwrap_or_default()
            .starts_with("Workspace is missing: ")
    );
}

#[tokio::test]
async fn a_queued_schedule_event_runs_in_the_background_session_and_delivers() {
    let data_dir = temp_data_dir("happy");
    let workspace = temp_data_dir("happy-ws");
    let schedule = daily_schedule("abc123", &workspace, "origin-gone");
    state::schedules::save(&data_dir, &[schedule]).expect("save schedule");
    let writer = LedgerWriter::open(&data_dir).expect("ledger");
    let event_id = queue_event(
        &writer,
        "abc123",
        json!({"scheduledAt": "2026-11-18T09:00:00Z"}),
    );

    let executor = Arc::new(PumpExecutor::default());
    let host = Host::new();
    host.reload_settings(&data_dir).expect("settings load");
    host.set_executor(executor.clone());

    schedule::pump::pump_once(&host, &data_dir)
        .await
        .expect("pump runs");

    let spawned = executor.spawned.lock().unwrap();
    assert_eq!(spawned.len(), 1, "one turn");
    let (target, text) = &spawned[0];
    // The origin session is dead; the run continues in the schedule's
    // persistent background session — minted by the pump under
    // `session:schedule:{id}` (the source's state-map key) and bound
    // to the schedule's workspace.
    let mapped = writer
        .state_get("session:schedule:abc123")
        .expect("state read")
        .expect("mapped session id");
    assert_eq!(target, &mapped);
    assert_eq!(mapped.len(), 36, "a UUID session id: {mapped}");
    let path = record_path(&data_dir, &mapped).expect("record path");
    let record = state::read_session_record(&path, &mapped).expect("background record");
    assert_eq!(
        record.meta.workspace_root.as_deref(),
        Some(workspace.to_string_lossy().as_ref()),
        "the background session runs in the schedule's workspace"
    );
    assert!(
        text.starts_with("Created in session origin-gone, which no longer exists."),
        "text = {text:?}"
    );
    assert!(text.contains("Report the build status."), "wake text");
    drop(spawned);

    let event = writer.get_event(&event_id).expect("read").expect("exists");
    assert_eq!(event.status, "delivered");
    assert_eq!(
        runtime_status(&writer, "abc123"),
        ScheduleRuntimeStatus::Idle
    );
    let runtime = read_schedule_runtime("abc123", |key| writer.state_get(key).ok().flatten());
    assert_eq!(
        runtime.last_result,
        Some(openbutler_daemon::schedule::ScheduleRunResult::Completed)
    );
    let runtime = read_schedule_runtime("abc123", |key| writer.state_get(key).ok().flatten());
    assert_eq!(runtime.run_count, 1);
}

#[tokio::test]
async fn a_failed_turn_marks_the_event_failed_with_the_turn_error() {
    let data_dir = temp_data_dir("turnfail");
    let workspace = temp_data_dir("turnfail-ws");
    let schedule = daily_schedule("abc123", &workspace, "origin-gone");
    state::schedules::save(&data_dir, &[schedule]).expect("save");
    let writer = LedgerWriter::open(&data_dir).expect("ledger");
    let event_id = queue_event(&writer, "abc123", json!({}));

    let executor = Arc::new(PumpExecutor::default());
    *executor.fail_next.lock().unwrap() = true;
    let host = Host::new();
    host.reload_settings(&data_dir).expect("settings load");
    host.set_executor(executor.clone());

    schedule::pump::pump_once(&host, &data_dir)
        .await
        .expect("pump runs");

    let event = writer.get_event(&event_id).expect("read").expect("exists");
    assert_eq!(event.status, "failed");
    assert_eq!(
        event.error.as_deref(),
        Some("The provider rejected the request.")
    );
    assert_eq!(
        runtime_status(&writer, "abc123"),
        ScheduleRuntimeStatus::Idle
    );
    let runtime = read_schedule_runtime("abc123", |key| writer.state_get(key).ok().flatten());
    assert_eq!(
        runtime.last_result,
        Some(openbutler_daemon::schedule::ScheduleRunResult::Failed)
    );
    assert_eq!(
        runtime.last_error.as_deref(),
        Some("The provider rejected the request.")
    );
}

#[tokio::test]
async fn the_origin_session_wins_when_it_is_alive() {
    let data_dir = temp_data_dir("origin");
    let workspace = temp_data_dir("origin-ws");
    let schedule = daily_schedule("abc123", &workspace, "00000000-0000-4000-8000-000000000001");
    state::schedules::save(&data_dir, &[schedule]).expect("save");
    let writer = LedgerWriter::open(&data_dir).expect("ledger");
    let event_id = queue_event(&writer, "abc123", json!({}));

    let executor = Arc::new(PumpExecutor::default());
    executor
        .origins_alive
        .lock()
        .unwrap()
        .push("00000000-0000-4000-8000-000000000001".to_owned());
    let host = Host::new();
    host.reload_settings(&data_dir).expect("settings load");
    host.set_executor(executor.clone());

    schedule::pump::pump_once(&host, &data_dir)
        .await
        .expect("pump runs");

    let spawned = executor.spawned.lock().unwrap();
    assert_eq!(spawned.len(), 1);
    // The report appears where the schedule was asked for — no
    // provenance note, because the origin still exists.
    assert_eq!(spawned[0].0, "00000000-0000-4000-8000-000000000001");
    assert!(
        !spawned[0].1.starts_with("Created in session"),
        "text = {:?}",
        spawned[0].1
    );
    drop(spawned);
    let event = writer.get_event(&event_id).expect("read").expect("exists");
    assert_eq!(event.status, "delivered");
}
