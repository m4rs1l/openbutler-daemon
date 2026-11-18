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
