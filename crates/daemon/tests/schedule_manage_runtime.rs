//! The runtime half of `schedule.manage`: `run` queues one manual
//! firing into the feed, `list` reports adapter/describe/runtime state,
//! `log` maps the run history — and the CRUD reloads the installed
//! runner so its response carries the real `runtime`.
//!
//! Source: `schedule-service.ts` `executeBackground` (the runtime half
//! re-read 2026-11-18).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use openbutler_daemon::dispatch::{DispatchState, Host, handle};
use openbutler_daemon::ledger::{EventInput, LedgerWriter};
use openbutler_daemon::schedule::ScheduleRunner;
use openbutler_protocol::{DaemonRequest, DaemonResponse};
use serde_json::{Value, json};

fn temp_data_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ob-sched-rt-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

use std::path::PathBuf;

fn manage(input: Value) -> DaemonRequest {
    DaemonRequest::new("t-rt", "schedule.manage").with("input", input)
}

fn apply(host: &Host, dir: &Path, input: Value) -> DaemonResponse {
    let mut state = DispatchState::new();
    handle(host, dir, &mut state, &manage(input))
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

fn daily_schedule(id: &str, root: &Path, enabled: bool) -> Value {
    json!({
        "id": id,
        "name": id,
        "enabled": enabled,
        "workspaceRoot": root.display().to_string(),
        "instruction": "i",
        "trigger": { "adapter": "daily", "options": { "at": "02:00" } }
    })
}

#[test]
fn run_queues_a_manual_event_for_an_enabled_schedule() {
    let host = Host::new();
    let dir = temp_data_dir("run-ok");
    let root = dir.join("work");
    std::fs::create_dir_all(&root).expect("workspace");
    let added = success_json(apply(
        &host,
        &dir,
        json!({
            "action": "add", "schedule": daily_schedule("s1", &root, true)
        }),
    ));

    let queued = success_json(apply(
        &host,
        &dir,
        json!({
            "action": "run", "scheduleId": "s1"
        }),
    ));
    assert_eq!(queued["status"], "queued");
    assert_eq!(queued["scheduleId"], "s1");

    // The event landed in the feed with the source's envelope.
    let writer = LedgerWriter::open(&dir).expect("writer");
    let rows = writer.schedule_events("s1", 50).expect("rows");
    assert_eq!(rows.len(), 1, "the manual firing is queued once");
    assert_eq!(rows[0].source, "schedule:manual");
    assert_eq!(rows[0].kind, "schedule.requested");
    assert!(rows[0].dedupe_key.starts_with("schedule:s1:manual:"));
    assert_eq!(rows[0].status, "pending");
    assert_eq!(rows[0].payload["scheduleId"], "s1");
    assert!(rows[0].payload["facts"]["requestedAt"].is_string());
    assert_eq!(
        rows[0].workspace_root.as_deref(),
        Some(root.to_string_lossy().as_ref())
    );
    assert_eq!(added["saved"], true);
}

#[test]
fn run_refuses_missing_and_disabled_schedules() {
    let host = Host::new();
    let dir = temp_data_dir("run-refuse");
    let root = dir.join("work");
    std::fs::create_dir_all(&root).expect("workspace");
    success_json(apply(
        &host,
        &dir,
        json!({
            "action": "add", "schedule": daily_schedule("off", &root, false)
        }),
    ));

    let error = failure_text(apply(
        &host,
        &dir,
        json!({ "action": "run", "scheduleId": "nope" }),
    ));
    assert_eq!(error, "Schedule does not exist.");
    let error = failure_text(apply(
        &host,
        &dir,
        json!({ "action": "run", "scheduleId": "off" }),
    ));
    assert_eq!(error, "Enable the schedule before running it.");
    let error = failure_text(apply(&host, &dir, json!({ "action": "run" })));
    assert_eq!(
        error,
        "scheduleId must be a non-empty string under 64 bytes."
    );
}

#[test]
fn list_reports_idle_adapter_states_and_the_describe_line() {
    let host = Host::new();
    let dir = temp_data_dir("list");
    let root = dir.join("work");
    std::fs::create_dir_all(&root).expect("workspace");
    success_json(apply(
        &host,
        &dir,
        json!({
            "action": "add", "schedule": daily_schedule("s1", &root, true)
        }),
    ));

    let listed = success_json(apply(&host, &dir, json!({ "action": "list" })));
    let entries = listed["schedules"].as_array().expect("list");
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    // The spread: every stored field rides along.
    assert_eq!(entry["id"], "s1");
    assert_eq!(
        entry["workspaceRoot"].as_str(),
        Some(root.to_string_lossy().as_ref())
    );
    // No runner installed: the source's `?? 'idle'` for an unknown id.
    assert_eq!(entry["adapterState"], "idle");
    assert!(entry.get("adapterError").is_none(), "idle carries no error");
    assert_eq!(entry["runtime"]["status"], "idle");
    assert_eq!(entry["runtime"]["runCount"], 0);
    assert!(
        entry["runtime"].get("lastResult").is_none(),
        "absent options are absent, not null"
    );
    assert_eq!(entry["schedule"]["human"], "daily 02:00");
    assert!(entry["schedule"]["nextDueAt"].is_number());
}

#[test]
fn log_maps_the_run_history_with_scheduled_at_and_truthy_errors() {
    let host = Host::new();
    let dir = temp_data_dir("log");
    let root = dir.join("work");
    std::fs::create_dir_all(&root).expect("workspace");
    success_json(apply(
        &host,
        &dir,
        json!({
            "action": "add", "schedule": daily_schedule("s1", &root, true)
        }),
    ));
    let ids;
    {
        let writer = LedgerWriter::open(&dir).expect("writer");
        let (first_id, _) = writer
            .insert_event(&EventInput {
                source: "schedule:daily".to_owned(),
                kind: "schedule.daily".to_owned(),
                dedupe_key: "schedule:s1:daily:1".to_owned(),
                workspace_root: Some(root.to_string_lossy().into_owned()),
                occurred_at: None,
                payload: json!({
                    "scheduleId": "s1",
                    "facts": { "scheduledAt": "2026-01-01T02:00:00.000Z" }
                }),
            })
            .expect("insert with error");
        // A run whose error is an empty string: the source's truthiness
        // omits it from the mapping.
        let (second_id, _) = writer
            .insert_event(&EventInput {
                source: "schedule:daily".to_owned(),
                kind: "schedule.daily".to_owned(),
                dedupe_key: "schedule:s1:daily:2".to_owned(),
                workspace_root: None,
                occurred_at: None,
                payload: json!({ "scheduleId": "s1", "facts": {} }),
            })
            .expect("insert clean");
        writer
            .set_event_state(&second_id, "failed", Some(""))
            .expect("empty error");
        writer
            .set_event_state(&first_id, "delivered", Some("boom"))
            .expect("error");
        ids = (first_id, second_id);
    }

    let logged = success_json(apply(
        &host,
        &dir,
        json!({
            "action": "log", "scheduleId": "s1"
        }),
    ));
    assert_eq!(logged["scheduleId"], "s1");
    let runs = logged["runs"].as_array().expect("runs");
    assert_eq!(runs.len(), 2, "newest first");
    // Newest first: the second insert leads, and `eventId` is the row's
    // id (the source maps `event.id`, not the dedupe key).
    assert_eq!(runs[0]["eventId"], ids.1);
    assert_eq!(runs[0]["status"], "failed");
    assert_eq!(runs[1]["eventId"], ids.0);
    assert_eq!(runs[1]["status"], "delivered");
    assert_eq!(runs[1]["error"], "boom");
    assert_eq!(runs[1]["scheduledAt"], "2026-01-01T02:00:00.000Z");
    assert!(
        runs[0].get("error").is_none(),
        "an empty error string is falsy and omitted"
    );
    assert!(
        runs[0].get("scheduledAt").is_none(),
        "facts without scheduledAt stay absent"
    );

    let error = failure_text(apply(
        &host,
        &dir,
        json!({
            "action": "log", "scheduleId": "nope"
        }),
    ));
    assert_eq!(error, "Schedule does not exist.");
}

#[tokio::test]
async fn crud_reloads_the_installed_runner_and_reports_runtime() {
    let host = Host::new();
    let dir = temp_data_dir("crud-runtime");
    let root = dir.join("work");
    std::fs::create_dir_all(&root).expect("workspace");

    let writer = Arc::new(Mutex::new(LedgerWriter::open(&dir).expect("writer")));
    let events: Arc<Mutex<Vec<EventInput>>> = Arc::new(Mutex::new(Vec::new()));
    let publish = {
        let writer = Arc::clone(&writer);
        let events = Arc::clone(&events);
        Arc::new(move |event: EventInput| {
            writer
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert_event(&event)
                .map(|_| ())
                .map_err(|error| error.to_string())?;
            events
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(event);
            Ok(())
        }) as openbutler_daemon::schedule::Publish
    };
    let log = Arc::new(|_message: String| {}) as openbutler_daemon::schedule::Log;
    host.set_scheduler(ScheduleRunner::new(writer, publish, log));

    let added = success_json(apply(
        &host,
        &dir,
        json!({
            "action": "add", "schedule": daily_schedule("s1", &root, true)
        }),
    ));
    assert_eq!(added["status"], "completed");
    assert_eq!(added["runtime"]["state"], "active", "reload reported");
    assert!(added["runtime"]["id"] == "s1");

    let disabled = success_json(apply(
        &host,
        &dir,
        json!({
            "action": "disable", "scheduleId": "s1"
        }),
    ));
    assert_eq!(disabled["status"], "completed");
    assert_eq!(disabled["runtime"]["state"], "disabled");

    let listed = success_json(apply(&host, &dir, json!({ "action": "list" })));
    assert_eq!(listed["schedules"][0]["adapterState"], "disabled");

    let removed = success_json(apply(
        &host,
        &dir,
        json!({
            "action": "remove", "scheduleId": "s1"
        }),
    ));
    // The runner reloads with the schedule gone: no status, no runtime.
    assert_eq!(removed["status"], "completed");
    assert!(removed.get("runtime").is_none());
}
