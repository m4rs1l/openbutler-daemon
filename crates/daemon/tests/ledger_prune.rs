//! `ledger.prune` — the source's settled-history selection, with a preview
//! that runs the same SQL.
//!
//! Source: `activity-ledger.ts` PRUNABLE_* and `pruneBefore` /
//! `previewPruneBefore`, plus the CLI's `runLedgerPrune` (read 2026-11-26).
//! Age alone never settles work: worker identities and one-shot schedule
//! identities stay forever, an inbox event waits for its read marker, and an
//! event an action or usage row still references is not history yet.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;

use common::temp_data_dir;
use openbutler_daemon::dispatch::{self, DispatchState, Host};
use openbutler_daemon::ledger::{EventInput, LedgerWriter, ToolActionState, ToolActionUpdate};
use openbutler_protocol::{DaemonRequest, DaemonResponse};
use serde_json::{Value, json};

fn event(source: &str, kind: &str, key: &str) -> EventInput {
    EventInput {
        source: source.to_owned(),
        kind: kind.to_owned(),
        dedupe_key: key.to_owned(),
        workspace_root: None,
        occurred_at: Some("2026-11-01T00:00:00.000Z".to_owned()),
        payload: json!({}),
    }
}

fn action(request_id: &str, state: ToolActionState) -> ToolActionUpdate {
    ToolActionUpdate {
        request_id: request_id.to_owned(),
        workspace_root: "C:\\work".to_owned(),
        session_id: "session-1".to_owned(),
        event_id: None,
        provider_id: "mock".to_owned(),
        name: "execute_command".to_owned(),
        input: json!({"command": "echo"}),
        effect: json!({}),
        state,
        result: None,
        error: None,
    }
}

fn prune(dir: &Path, payload: Value) -> Result<Value, String> {
    let host = Host::new();
    let mut state = DispatchState::new();
    let mut request = DaemonRequest::new("prune-1", "ledger.prune");
    if let Value::Object(fields) = payload {
        for (key, value) in fields {
            request.payload.insert(key, value);
        }
    }
    match dispatch::handle(&host, dir, &mut state, &request) {
        DaemonResponse::Success { result, .. } => Ok(result),
        DaemonResponse::Failure { error, .. } => Err(error),
    }
}

/// Moves every ledger row's timestamps into the past, leaving the
/// statuses and references exactly as the tests set them.
fn age_every_row(dir: &Path) {
    let connection = rusqlite::Connection::open(dir.join("state.db")).unwrap();
    connection
        .execute(
            "UPDATE openbutler_events SET received_at = '2020-01-01T00:00:00.000Z', updated_at = '2020-01-01T00:00:00.000Z'",
            [],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE openbutler_actions SET created_at = '2020-01-01T00:00:00.000Z', updated_at = '2020-01-01T00:00:00.000Z'",
            [],
        )
        .unwrap();
}

fn count_events(dir: &Path) -> i64 {
    let connection = rusqlite::Connection::open(dir.join("state.db")).unwrap();
    connection
        .query_row("SELECT COUNT(*) FROM openbutler_events", [], |row| {
            row.get(0)
        })
        .unwrap()
}

#[test]
fn a_preview_counts_the_same_selection_apply_deletes() {
    let dir = temp_data_dir("prune-events");
    let writer = LedgerWriter::open(&dir).unwrap();
    let (settled, _) = writer
        .insert_event(&event("timer", "schedule.interval", "t:s1:1"))
        .unwrap();
    writer.set_event_state(&settled, "delivered", None).unwrap();
    let (worker, _) = writer
        .insert_event(&event("worker", "worker.done", "w:s1:1"))
        .unwrap();
    writer.set_event_state(&worker, "delivered", None).unwrap();
    let (one_shot, _) = writer
        .insert_event(&event("timer", "schedule.at", "t:s1:2"))
        .unwrap();
    writer
        .set_event_state(&one_shot, "delivered", None)
        .unwrap();
    let (unread_inbox, _) = writer
        .insert_event(&event("owner-inbox", "owner.message", "o:s1:1"))
        .unwrap();
    writer
        .set_event_state(&unread_inbox, "delivered", None)
        .unwrap();
    let (pending, _) = writer
        .insert_event(&event("timer", "schedule.interval", "t:s1:3"))
        .unwrap();
    writer.set_event_state(&pending, "pending", None).unwrap();
    age_every_row(&dir);

    // Preview: exactly one row is history; nothing is deleted yet.
    let preview = prune(&dir, json!({ "kind": "events", "olderThan": "30d" })).unwrap();
    assert_eq!(
        preview,
        json!({ "events": 1, "actions": 0, "transitions": 0 })
    );
    assert_eq!(count_events(&dir), 5, "a preview never deletes");

    // Apply: the same selection goes away; every guard survives.
    let applied = prune(
        &dir,
        json!({ "kind": "events", "olderThan": "30d", "apply": true }),
    )
    .unwrap();
    assert_eq!(
        applied,
        json!({ "events": 1, "actions": 0, "transitions": 0 })
    );
    assert_eq!(count_events(&dir), 4);
    let connection = rusqlite::Connection::open(dir.join("state.db")).unwrap();
    let survivor: String = connection
        .query_row(
            "SELECT event_id FROM openbutler_events WHERE dedupe_key = 't:s1:1'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap_err()
        .to_string();
    assert!(survivor.contains("Query returned no rows"), "{survivor}");
}

#[test]
fn actions_prune_only_terminal_rows() {
    let dir = temp_data_dir("prune-actions");
    // The write-ahead rule: a row is recorded before it moves.
    let mut writer = LedgerWriter::open(&dir).unwrap();
    writer
        .record_tool_action(&action("req-done", ToolActionState::Proposed))
        .unwrap();
    writer
        .record_tool_action(&action("req-done", ToolActionState::Completed))
        .unwrap();
    writer
        .record_tool_action(&action("req-running", ToolActionState::Proposed))
        .unwrap();
    writer
        .record_tool_action(&action("req-running", ToolActionState::Started))
        .unwrap();
    age_every_row(&dir);

    let preview = prune(&dir, json!({ "kind": "actions", "olderThan": "12h" })).unwrap();
    // Two transitions belong to the completed action (proposed, completed).
    assert_eq!(
        preview,
        json!({ "events": 0, "actions": 1, "transitions": 2 })
    );
    let applied = prune(
        &dir,
        json!({ "kind": "actions", "olderThan": "12h", "apply": true }),
    )
    .unwrap();
    assert_eq!(
        applied,
        json!({ "events": 0, "actions": 1, "transitions": 2 })
    );
    let connection = rusqlite::Connection::open(dir.join("state.db")).unwrap();
    let ids: Vec<String> = {
        let mut statement = connection
            .prepare("SELECT request_id FROM openbutler_actions ORDER BY request_id")
            .unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<String>, _>>()
            .unwrap()
    };
    assert_eq!(ids, vec!["req-running".to_owned()]);
}

#[test]
fn the_refusals_are_the_source_sentences() {
    let dir = temp_data_dir("prune-refusals");
    assert_eq!(
        prune(&dir, json!({ "kind": "usage", "olderThan": "30d" })).unwrap_err(),
        "kind must be events or actions."
    );
    assert_eq!(
        prune(&dir, json!({ "kind": "events", "olderThan": "30" })).unwrap_err(),
        "Invalid duration \"30\". Use a value like 30d, 12h, 90m, or 2w."
    );
    assert_eq!(
        prune(&dir, json!({ "kind": "events", "olderThan": "0d" })).unwrap_err(),
        "Invalid duration \"0d\". Use a value like 30d, 12h, 90m, or 2w."
    );
    assert_eq!(
        prune(
            &dir,
            json!({ "kind": "events", "olderThan": "30d", "apply": "yes" })
        )
        .unwrap_err(),
        "apply must be a boolean."
    );
    // A missing database is an empty selection, and nothing is created.
    let counts = prune(&dir, json!({ "kind": "events", "olderThan": "30d" })).unwrap();
    assert_eq!(
        counts,
        json!({ "events": 0, "actions": 0, "transitions": 0 })
    );
    assert!(!dir.join("state.db").exists());
}
