//! Behavioral pins for `ScheduleRunner`: statuses, the timer adapters'
//! emit shape, one-shot prior handling, the filesystem watcher, and the
//! shutdown contract.
//!
//! Source: `tools/schedule.ts` L69-415 (read 2026-11-18). Timers use the
//! past-due seeding trick where possible (validation floors `everySeconds`
//! at 10, so a stored due in the past makes an interval fire immediately).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use chrono::TimeZone;
use openbutler_daemon::ledger::{EventInput, LedgerWriter};
use openbutler_daemon::schedule::{
    ScheduleRunner, ScheduleStatus, TriggerState, trigger_state_key,
};
use openbutler_daemon::state::schedules::{ScheduleState, ScheduleTrigger};
use serde_json::{Value, json};

fn temp_root(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("ob-schedule-runner-{tag}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn schedule(id: &str, adapter: &str, options: Value) -> ScheduleState {
    ScheduleState {
        id: id.to_owned(),
        name: format!("name-{id}"),
        enabled: true,
        workspace_root: "/work".to_owned(),
        origin_session_id: String::new(),
        instruction: "Do it.".to_owned(),
        trigger: ScheduleTrigger {
            adapter: adapter.to_owned(),
            options,
        },
    }
}

struct Harness {
    dir: PathBuf,
    writer: Arc<Mutex<LedgerWriter>>,
    events: Arc<Mutex<Vec<EventInput>>>,
    logs: Arc<Mutex<Vec<String>>>,
    runner: ScheduleRunner,
}

fn harness(tag: &str) -> Harness {
    let dir = temp_root(tag);
    let writer = Arc::new(Mutex::new(LedgerWriter::open(&dir).expect("writer opens")));
    let events = Arc::new(Mutex::new(Vec::new()));
    let logs = Arc::new(Mutex::new(Vec::new()));
    let publish = {
        let writer = Arc::clone(&writer);
        let events = Arc::clone(&events);
        Arc::new(move |event: EventInput| {
            writer
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert_event(&event)
                .map_err(|error| error.to_string())?;
            events
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(event);
            Ok(())
        }) as openbutler_daemon::schedule::Publish
    };
    let log = {
        let logs = Arc::clone(&logs);
        Arc::new(move |message: String| {
            logs.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(message);
        }) as openbutler_daemon::schedule::Log
    };
    let runner = ScheduleRunner::new(Arc::clone(&writer), publish, log);
    Harness {
        dir,
        writer,
        events,
        logs,
        runner,
    }
}

impl Harness {
    fn event_count(&self) -> usize {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    async fn wait_for_event(&self, timeout: Duration, kind: &str) -> Option<EventInput> {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let list = self.events.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(found) = list.iter().find(|event| event.kind == kind) {
                    return Some(found.clone());
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn logs(&self) -> Vec<String> {
        self.logs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn status_of(&self, id: &str) -> Option<ScheduleStatus> {
        self.runner
            .list_statuses()
            .into_iter()
            .find(|entry| entry.id == id)
    }
}

#[tokio::test]
async fn disabled_and_invalid_triggers_read_their_statuses() {
    let harness = harness("statuses");
    let mut disabled = schedule("d1", "manual", json!({}));
    disabled.enabled = false;
    let invalid = schedule("d2", "daily", json!({ "at": "9:00" }));

    harness.runner.reload(vec![disabled, invalid]).await;

    assert_eq!(
        harness.status_of("d1"),
        Some(ScheduleStatus {
            id: "d1".to_owned(),
            state: TriggerState::Disabled,
            error: None,
        })
    );
    let error = harness.status_of("d2").expect("invalid status");
    assert_eq!(error.state, TriggerState::Error);
    assert_eq!(
        error.error.as_deref(),
        Some("daily.at must be \"HH:MM\" in server-local wall time.")
    );
    assert_eq!(
        harness.logs(),
        vec!["Schedule d2: daily.at must be \"HH:MM\" in server-local wall time.".to_owned()]
    );
}

#[tokio::test]
async fn manual_is_active_and_never_emits() {
    let harness = harness("manual");
    harness
        .runner
        .reload(vec![schedule("m1", "manual", json!({}))])
        .await;
    assert_eq!(
        harness.status_of("m1").map(|entry| entry.state),
        Some(TriggerState::Active)
    );
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(harness.event_count(), 0, "manual must never fire");
}

#[tokio::test]
async fn interval_fires_immediately_when_the_stored_due_is_in_the_past() {
    let harness = harness("interval");
    let schedule_ref = schedule("i1", "interval", json!({ "everySeconds": 10 }));
    let state_key = trigger_state_key(&schedule_ref, "due");
    harness
        .writer
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .set_state(&state_key, "1")
        .expect("seed due");
    let before = chrono::Utc::now().timestamp_millis();

    harness.runner.reload(vec![schedule_ref.clone()]).await;

    let event = harness
        .wait_for_event(Duration::from_secs(3), "schedule.interval")
        .await
        .expect("the past due fires immediately");
    assert_eq!(event.source, "schedule:interval");
    assert_eq!(event.dedupe_key, "schedule:i1:1");
    assert_eq!(event.workspace_root.as_deref(), Some("/work"));
    assert_eq!(
        event.payload,
        json!({
            "scheduleId": "i1",
            "facts": { "scheduledAt": "1970-01-01T00:00:00.001Z" }
        })
    );

    // The due advanced to now + everySeconds (missed intervals coalesce).
    let stored = harness
        .writer
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .state_get(&state_key)
        .expect("read due")
        .expect("due written");
    let due: i64 = stored.parse().expect("numeric due");
    assert!(
        due >= before + 9_000,
        "due must advance to ~now+10s, got {due} vs {before}"
    );
}

#[tokio::test]
async fn at_fires_once_then_stays_scheduled_until_delivered() {
    let harness = harness("at");
    let at = "2020-01-01T10:00";
    let due = chrono::Local
        .with_ymd_and_hms(2020, 1, 1, 10, 0, 0)
        .single()
        .expect("local instant")
        .timestamp_millis();
    let schedule_ref = schedule("a1", "at", json!({ "at": at }));

    harness.runner.reload(vec![schedule_ref.clone()]).await;
    let event = harness
        .wait_for_event(Duration::from_secs(3), "schedule.at")
        .await
        .expect("a past at fires immediately");
    assert_eq!(event.dedupe_key, format!("schedule:a1:at:{due}"));
    assert_eq!(event.payload["facts"]["scheduledAt"], to_iso(due));

    // Second reload: the pending event is found — active, no re-fire.
    harness.runner.reload(vec![schedule_ref.clone()]).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(harness.event_count(), 1, "one-shot must not fire twice");
    assert_eq!(
        harness.status_of("a1").map(|entry| entry.state),
        Some(TriggerState::Active)
    );

    // Delivered: the one-shot is completed.
    {
        let connection = rusqlite::Connection::open(harness.dir.join("state.db")).expect("open");
        connection
            .execute(
                "UPDATE openbutler_events SET status = 'delivered' WHERE dedupe_key = ?1",
                [format!("schedule:a1:at:{due}")],
            )
            .expect("deliver");
    }
    harness.runner.reload(vec![schedule_ref.clone()]).await;
    assert_eq!(
        harness.status_of("a1").map(|entry| entry.state),
        Some(TriggerState::Completed)
    );
}

#[tokio::test]
async fn interrupted_at_prior_retries_the_event_without_rearming() {
    let harness = harness("at-interrupted");
    let at = "2020-01-01T10:00";
    let due = chrono::Local
        .with_ymd_and_hms(2020, 1, 1, 10, 0, 0)
        .single()
        .expect("local instant")
        .timestamp_millis();
    let schedule_ref = schedule("a2", "at", json!({ "at": at }));
    {
        let writer = harness
            .writer
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (id, _) = writer
            .insert_event(&EventInput {
                source: "schedule:at".to_owned(),
                kind: "schedule.at".to_owned(),
                dedupe_key: format!("schedule:a2:at:{due}"),
                workspace_root: Some("/work".to_owned()),
                occurred_at: None,
                payload: json!({ "scheduleId": "a2", "facts": {} }),
            })
            .expect("insert prior");
        drop(writer);
        let connection = rusqlite::Connection::open(harness.dir.join("state.db")).expect("open");
        connection
            .execute(
                "UPDATE openbutler_events SET status = 'interrupted' WHERE event_id = ?1",
                [id],
            )
            .expect("interrupt");
    }

    harness.runner.reload(vec![schedule_ref]).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        harness.event_count(),
        0,
        "no re-arm on an interrupted prior"
    );
    assert_eq!(
        harness.status_of("a2").map(|entry| entry.state),
        Some(TriggerState::Active)
    );
    let connection = rusqlite::Connection::open(harness.dir.join("state.db")).expect("open");
    let status: String = connection
        .query_row(
            "SELECT status FROM openbutler_events WHERE dedupe_key = ?1",
            [format!("schedule:a2:at:{due}")],
            |row| row.get(0),
        )
        .expect("row");
    assert_eq!(status, "pending", "retryEvent moved it back to pending");
}

#[tokio::test]
async fn window_in_the_past_fires_late_immediately() {
    let harness = harness("window");
    let from = "2020-01-01T09:00";
    let due = chrono::Local
        .with_ymd_and_hms(2020, 1, 1, 9, 0, 0)
        .single()
        .expect("local instant")
        .timestamp_millis();
    harness
        .runner
        .reload(vec![schedule(
            "w1",
            "window",
            json!({ "from": from, "to": "2020-01-01T11:00" }),
        )])
        .await;

    let event = harness
        .wait_for_event(Duration::from_secs(3), "schedule.window")
        .await
        .expect("a past window fires as late");
    assert_eq!(event.dedupe_key, format!("schedule:w1:window:{due}"));
    assert_eq!(event.payload["facts"]["scheduledAt"], to_iso(due));
}

#[tokio::test]
async fn reload_replaces_the_previous_statuses_and_stops() {
    let harness = harness("replace");
    harness
        .runner
        .reload(vec![schedule("old", "manual", json!({}))])
        .await;
    assert_eq!(harness.runner.list_statuses().len(), 1);

    harness
        .runner
        .reload(vec![schedule("new", "manual", json!({}))])
        .await;
    let statuses = harness.runner.list_statuses();
    assert_eq!(statuses.len(), 1, "close() cleared the old entries");
    assert_eq!(statuses[0].id, "new");
}

#[tokio::test]
async fn filesystem_change_emits_and_close_stops_the_watcher() {
    let dir = temp_root("fs-watch");
    let root = std::fs::canonicalize(&dir)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let harness = harness("fs");
    harness
        .runner
        .reload(vec![schedule(
            "f1",
            "filesystem",
            json!({
                "path": dir.to_string_lossy(),
                "recursive": false,
                "debounceSeconds": 1,
            }),
        )])
        .await;
    assert_eq!(
        harness.status_of("f1").map(|entry| entry.state),
        Some(TriggerState::Active)
    );

    std::fs::write(dir.join("first.txt"), "x").expect("create");
    let event = harness
        .wait_for_event(Duration::from_secs(5), "filesystem.changed")
        .await
        .expect("the watcher reports the change");
    let facts = &event.payload["facts"];
    assert_eq!(facts["root"], root);
    assert_eq!(facts["overflow"], false);
    let changes = facts["changes"].as_array().expect("changes list");
    assert!(!changes.is_empty(), "the create must be recorded");
    let name = changes[0]["event"].as_str().expect("event name");
    // A create+write yields several OS events and the last name per path
    // wins (the source has the same race with node's fs.watch); the pin
    // is the vocabulary: only `rename` and `change` ever reach the facts.
    assert!(
        name == "rename" || name == "change",
        "unexpected event name: {name}"
    );

    harness.runner.close();
    assert!(harness.runner.list_statuses().is_empty());
    std::fs::write(dir.join("second.txt"), "y").expect("create after close");
    tokio::time::sleep(Duration::from_millis(1400)).await;
    assert_eq!(
        harness.event_count(),
        1,
        "a closed watcher must not emit again"
    );
}

#[tokio::test]
async fn memory_pressure_starts_and_close_clears_it() {
    let harness = harness("memory");
    harness
        .runner
        .reload(vec![schedule(
            "m1",
            "memory_pressure",
            json!({ "usedPercent": 99, "sampleSeconds": 10, "durationSeconds": 60 }),
        )])
        .await;
    assert_eq!(
        harness.status_of("m1").map(|entry| entry.state),
        Some(TriggerState::Active)
    );
    harness.runner.close();
    assert!(harness.runner.list_statuses().is_empty());
}

#[tokio::test]
async fn shutdown_clears_and_refuses_further_reloads() {
    let harness = harness("shutdown");
    harness
        .runner
        .reload(vec![schedule("s1", "manual", json!({}))])
        .await;

    harness.runner.shutdown().await.expect("clean shutdown");
    assert!(harness.runner.list_statuses().is_empty());

    // The shutting-down guard: a later reload starts nothing.
    harness
        .runner
        .reload(vec![schedule("s2", "manual", json!({}))])
        .await;
    assert!(harness.runner.list_statuses().is_empty());
}

/// `new Date(ms).toISOString()` for the assertions.
fn to_iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|instant| instant.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .expect("instant in range")
}
