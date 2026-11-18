//! The observed-event write path: validation ceilings, the dedupe
//! insert, and the events table that the writer schema creates.
//!
//! Source: `activity-ledger.ts` `insertEvent`/`validateEvent` (L409-434,
//! L934-945, read 2026-11-18).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use openbutler_daemon::ledger::{EventInput, LedgerWriter};
use serde_json::json;

fn temp_root(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("ob-ledger-events-{tag}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn event(dedupe_key: &str) -> EventInput {
    EventInput {
        source: "schedule:interval".to_owned(),
        kind: "schedule.interval".to_owned(),
        dedupe_key: dedupe_key.to_owned(),
        workspace_root: Some("/work".to_owned()),
        occurred_at: None,
        payload: json!({ "scheduleId": "s1", "facts": {} }),
    }
}

#[test]
fn fresh_database_creates_the_events_table_and_round_trips() {
    // The writer schema must create `openbutler_events` — before that
    // DDL the writer's own insert (and the schedule runner) failed on a
    // fresh database with "no such table".
    let root = temp_root("fresh");
    let writer = LedgerWriter::open(&root).expect("writer opens");

    let (id, inserted) = writer
        .insert_event(&event("schedule:s1:1"))
        .expect("insert");
    assert!(inserted);
    assert!(!id.is_empty());

    // Same dedupe key: the existing row wins, no second insert.
    let (again, inserted) = writer
        .insert_event(&event("schedule:s1:1"))
        .expect("second insert");
    assert!(!inserted);
    assert_eq!(again, id);

    let rows = writer.schedule_events("s1", 25).expect("read back");
    assert_eq!(rows.len(), 1, "the conflict must not duplicate the row");
    assert_eq!(rows[0].id, id);
    assert_eq!(rows[0].dedupe_key, "schedule:s1:1");
    assert_eq!(rows[0].status, "pending");
    assert_eq!(rows[0].workspace_root.as_deref(), Some("/work"));
    assert_eq!(rows[0].payload["scheduleId"], "s1");
}

#[test]
fn schedule_events_selects_by_payload_and_clamps_the_limit() {
    let root = temp_root("select");
    let writer = LedgerWriter::open(&root).expect("writer opens");
    for index in 0..3 {
        let mut input = event(&format!("schedule:s1:{index}"));
        input.payload = json!({ "scheduleId": "s1", "seq": index });
        writer.insert_event(&input).expect("insert");
    }
    let other = EventInput {
        payload: json!({ "scheduleId": "other" }),
        ..event("schedule:other:1")
    };
    writer.insert_event(&other).expect("insert other");

    let rows = writer.schedule_events("s1", 25).expect("select");
    assert_eq!(rows.len(), 3, "the other schedule must not leak in");
    // Newest first: the loop inserted in ascending order.
    assert_eq!(rows[0].payload["seq"], 2);
    assert_eq!(rows[2].payload["seq"], 0);

    // Limit clamps to 1..=200 (0 would read nothing without the clamp).
    let rows = writer.schedule_events("s1", 0).expect("clamped");
    assert_eq!(rows.len(), 1);
    let rows = writer.schedule_events("s1", 500).expect("clamped");
    assert_eq!(rows.len(), 3);
}

#[test]
fn validation_refusals_are_the_source_messages() {
    let root = temp_root("validation");
    let writer = LedgerWriter::open(&root).expect("writer opens");

    let mut input = event("schedule:s1:1");
    input.source = String::new();
    let error = writer.insert_event(&input).expect_err("empty source");
    assert_eq!(
        error.to_string(),
        "Observed event source must be 1-64 characters."
    );

    // JS `.length` counts UTF-16 units: 33 x e-acute is 66 UTF-8 bytes
    // but 33 units — the gate accepts it (byte counting would not), which
    // pins the unit; one unit over the ceiling is refused.
    let mut input = event("schedule:s1:1");
    input.source = "é".repeat(33);
    writer.insert_event(&input).expect("utf16 units, not bytes");
    let mut input = event("schedule:s1:1");
    input.source = "é".repeat(65);
    let error = writer.insert_event(&input).expect_err("utf16 length");
    assert_eq!(
        error.to_string(),
        "Observed event source must be 1-64 characters."
    );

    let mut input = event("schedule:s1:1");
    input.dedupe_key = "k".repeat(513);
    let error = writer.insert_event(&input).expect_err("dedupe length");
    assert_eq!(
        error.to_string(),
        "Observed event dedupeKey must be 1-512 characters."
    );

    let mut input = event("schedule:s1:1");
    input.workspace_root = Some("/".repeat(32_769));
    let error = writer.insert_event(&input).expect_err("root length");
    assert_eq!(
        error.to_string(),
        "Observed event workspace root is too long."
    );
}

#[test]
fn payload_ceiling_is_utf8_bytes_not_characters() {
    let root = temp_root("payload");
    let writer = LedgerWriter::open(&root).expect("writer opens");

    let mut input = event("schedule:s1:1");
    // Exactly at the 256 KiB ceiling: the serialized payload is a quoted
    // string, so a slightly smaller body still lands under the limit.
    input.payload = json!(format!("x{}", "a".repeat(256 * 1024 - 3)));
    writer.insert_event(&input).expect("at the ceiling");

    let mut input = event("schedule:s1:2");
    input.payload = json!(format!("x{}", "a".repeat(256 * 1024)));
    let error = writer.insert_event(&input).expect_err("over the ceiling");
    assert_eq!(
        error.to_string(),
        "Observed event payload exceeded its size limit."
    );

    // The notification source gets the 32 MiB ceiling.
    let mut input = event("schedule:s1:3");
    input.source = "notification".to_owned();
    input.payload = json!(format!("x{}", "a".repeat(256 * 1024)));
    writer.insert_event(&input).expect("notification ceiling");
}

#[test]
fn claim_moves_pending_to_processing_exactly_once() {
    let root = temp_root("claim");
    let writer = LedgerWriter::open(&root).expect("writer opens");
    let (id, _) = writer
        .insert_event(&event("schedule:s1:1"))
        .expect("insert");

    let claimed = writer.claim_event(&id).expect("claim").expect("row");
    assert_eq!(claimed.status, "processing");
    assert_eq!(claimed.attempts, 1);
    assert_eq!(claimed.error, None, "the claim clears the error");

    // A second claim finds no pending row — the pump's single-claim gate.
    assert!(writer.claim_event(&id).expect("second claim").is_none());
    let row = writer.get_event(&id).expect("read").expect("row");
    assert_eq!(row.attempts, 1, "the failed claim must not bump attempts");
}

#[test]
fn set_event_state_writes_status_and_error_plainly() {
    let root = temp_root("set-state");
    let writer = LedgerWriter::open(&root).expect("writer opens");
    let (id, _) = writer
        .insert_event(&event("schedule:s1:1"))
        .expect("insert");

    writer
        .set_event_state(&id, "failed", Some("Workspace is missing: /gone"))
        .expect("fail");
    let row = writer.get_event(&id).expect("read").expect("row");
    assert_eq!(row.status, "failed");
    assert_eq!(row.error.as_deref(), Some("Workspace is missing: /gone"));

    // No state guard (the source has none): any status writes.
    writer
        .set_event_state(&id, "delivered", None)
        .expect("deliver");
    let row = writer.get_event(&id).expect("read").expect("row");
    assert_eq!(row.status, "delivered");
    assert_eq!(row.error, None, "None clears the error column");
}

#[test]
fn recover_interrupted_splits_processing_rows_by_source() {
    let root = temp_root("recover");
    let writer = LedgerWriter::open(&root).expect("writer opens");
    let insert = |source: &str, kind: &str, key: &str| {
        let input = EventInput {
            source: source.to_owned(),
            kind: kind.to_owned(),
            dedupe_key: key.to_owned(),
            workspace_root: None,
            occurred_at: None,
            payload: json!({}),
        };
        let (id, _) = writer.insert_event(&input).expect("insert");
        writer
            .set_event_state(&id, "processing", None)
            .expect("processing");
        id
    };
    let schedule = insert("schedule:interval", "schedule.interval", "k1");
    let notification = insert("notification", "message", "k2");
    let inbox = insert("owner-inbox", "message", "k3");
    let telegram = insert("telegram", "inbox.message", "k4");
    // A delivered row must not be touched.
    let delivered = insert("schedule:at", "schedule.at", "k5");
    writer
        .set_event_state(&delivered, "delivered", None)
        .expect("deliver");

    let recovered = writer.recover_interrupted_events().expect("recover");
    assert_eq!(recovered, 4, "only the processing rows move");

    let row = |id: &str| writer.get_event(id).expect("read").expect("row");
    let schedule_row = row(&schedule);
    assert_eq!(schedule_row.status, "interrupted");
    assert_eq!(
        schedule_row.error.as_deref(),
        Some("Host stopped while this event was being processed.")
    );
    let notification_row = row(&notification);
    assert_eq!(notification_row.status, "pending");
    assert_eq!(
        notification_row.error.as_deref(),
        Some("Host stopped while this event was being processed.")
    );
    let inbox_row = row(&inbox);
    assert_eq!(inbox_row.status, "pending");
    assert_eq!(
        inbox_row.error.as_deref(),
        Some("Owner message is available for retry after host restart.")
    );
    let telegram_row = row(&telegram);
    assert_eq!(telegram_row.status, "pending", "telegram inbox.* retries");
    assert_eq!(row(&delivered).status, "delivered", "terminal rows stay");
}
