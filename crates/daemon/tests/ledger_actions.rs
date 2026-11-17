//! c2a — the ledger's tool-action write path: the write-ahead record that
//! must exist before execution, the transition chain, terminal stamping,
//! and boot recovery for interrupted work.
//!
//! Sources: legacy `src/activity-ledger.ts` — `recordToolAction` (L335),
//! `recoverInterruptedActions` (L486), `safeJson` (L903), `LEDGER_SCHEMA`
//! (L173), and `src/skill-path.ts` `skillNameFromPath`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use openbutler_daemon::ledger::{LedgerWriter, ToolActionState, ToolActionUpdate};
use serde_json::{Value, json};

fn temp_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "ob-ledger-actions-{tag}-{}-{nanos:x}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn action(request_id: &str, state: ToolActionState) -> ToolActionUpdate {
    ToolActionUpdate {
        request_id: request_id.to_owned(),
        workspace_root: "C:\\work\\proj".to_owned(),
        session_id: "22222222-2222-4222-8222-222222222222".to_owned(),
        event_id: None,
        provider_id: "host".to_owned(),
        name: "execute_command".to_owned(),
        input: json!({ "command": "echo hi", "cwd": null }),
        effect: json!({ "change": "unknown", "data": "unknown" }),
        state,
        result: None,
        error: None,
    }
}

fn query(data_dir: &Path, sql: &str) -> Vec<Vec<String>> {
    let connection = rusqlite::Connection::open(data_dir.join("state.db")).unwrap();
    let mut statement = connection.prepare(sql).unwrap();
    statement
        .query_map([], |row| {
            let mut values = Vec::new();
            let mut index = 0;
            loop {
                match row.get::<_, Option<String>>(index) {
                    Ok(Some(value)) => values.push(value),
                    Ok(None) => values.push(String::new()),
                    Err(_) => break,
                }
                index += 1;
            }
            Ok(values)
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

#[test]
fn a_proposal_records_once_and_appends_one_transition() {
    let root = temp_root("proposed");
    let mut ledger = LedgerWriter::open(&root).expect("writer opens");

    ledger
        .record_tool_action(&action("req-1", ToolActionState::Proposed))
        .expect("first proposal");
    ledger
        .record_tool_action(&action("req-1", ToolActionState::Proposed))
        .expect("a duplicate proposal is idempotent");

    let rows = query(
        &root,
        "SELECT state, error, completed_at FROM openbutler_actions WHERE request_id = 'req-1'",
    );
    assert_eq!(rows.len(), 1, "one row for one request id");
    assert_eq!(rows[0][0], "proposed");
    assert!(rows[0][2].is_empty(), "a proposal has no completed_at");

    let transitions = query(
        &root,
        "SELECT state FROM openbutler_action_transitions WHERE request_id = 'req-1' ORDER BY id",
    );
    assert_eq!(
        transitions,
        vec![vec!["proposed".to_owned()]],
        "the duplicate appends no transition"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn execution_without_the_write_ahead_record_is_refused() {
    let root = temp_root("write-ahead");
    let mut ledger = LedgerWriter::open(&root).expect("writer opens");

    let error = ledger
        .record_tool_action(&action("req-2", ToolActionState::Started))
        .expect_err("no row exists yet");
    assert_eq!(
        error.to_string(),
        "An action must be recorded before execution."
    );

    // With the proposal in place the same transition succeeds — the order
    // proposed → started is the write-ahead sequence the executor follows.
    ledger
        .record_tool_action(&action("req-2", ToolActionState::Proposed))
        .unwrap();
    ledger
        .record_tool_action(&action("req-2", ToolActionState::Started))
        .unwrap();

    let transitions = query(
        &root,
        "SELECT state FROM openbutler_action_transitions WHERE request_id = 'req-2' ORDER BY id",
    );
    assert_eq!(
        transitions,
        vec![vec!["proposed".to_owned()], vec!["started".to_owned()]]
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn terminal_states_stamp_completed_at_and_errors_land_in_the_row() {
    let root = temp_root("terminal");
    let mut ledger = LedgerWriter::open(&root).expect("writer opens");

    let mut failing = action("req-3", ToolActionState::Proposed);
    ledger.record_tool_action(&failing).unwrap();
    failing.state = ToolActionState::Failed;
    failing.error = Some("the command exited 1".to_owned());
    ledger.record_tool_action(&failing).unwrap();

    let rows = query(
        &root,
        "SELECT state, error, completed_at FROM openbutler_actions WHERE request_id = 'req-3'",
    );
    assert_eq!(rows[0][0], "failed");
    assert_eq!(rows[0][1], "the command exited 1");
    assert!(
        rows[0][2].ends_with('Z') && rows[0][2].contains('T'),
        "completed_at is an ISO timestamp: {}",
        rows[0][2]
    );

    // The transition detail carries the error envelope.
    let details = query(
        &root,
        "SELECT detail_json FROM openbutler_action_transitions WHERE request_id = 'req-3' AND state = 'failed'",
    );
    assert!(
        details[0][0].contains("exited 1"),
        "detail: {}",
        details[0][0]
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn recovery_fails_everything_unresolved_and_spares_terminal_rows() {
    let root = temp_root("recovery");
    {
        let mut ledger = LedgerWriter::open(&root).expect("writer opens");
        for (id, state) in [
            ("open-proposal", ToolActionState::Proposed),
            ("mid-run", ToolActionState::Started),
            ("done", ToolActionState::Completed),
        ] {
            ledger
                .record_tool_action(&action(id, ToolActionState::Proposed))
                .unwrap();
            if state != ToolActionState::Proposed {
                ledger.record_tool_action(&action(id, state)).unwrap();
            }
        }
    }

    // A fresh writer (a restarted host) runs recovery at boot.
    let mut ledger = LedgerWriter::open(&root).expect("writer reopens");
    let recovered = ledger.recover_interrupted_actions().expect("recovery runs");
    assert_eq!(recovered, 2, "proposal and mid-run are unresolved");

    let rows = query(
        &root,
        "SELECT request_id, state, error, completed_at FROM openbutler_actions ORDER BY request_id",
    );
    assert_eq!(rows.len(), 3);
    let message = "Host stopped before recording the action outcome. Inspect the target; do not replay this action automatically.";
    for row in &rows {
        match row[0].as_str() {
            "done" => {
                assert_eq!(row[1], "completed", "terminal rows are untouched");
                assert!(row[2].is_empty());
            }
            _ => {
                assert_eq!(row[1], "failed", "{} becomes failed", row[0]);
                assert_eq!(row[2], message, "the exact recovery wording");
                assert!(!row[3].is_empty(), "recovery stamps completion");
            }
        }
    }

    // Recovery appended its own transitions for the two rows it moved.
    let recovery_transitions = query(
        &root,
        "SELECT CAST(COUNT(*) AS TEXT) FROM openbutler_action_transitions WHERE state = 'failed'",
    );
    assert_eq!(recovery_transitions[0][0], "2");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_schema_check_refuses_states_outside_the_array() {
    let root = temp_root("check");
    LedgerWriter::open(&root).expect("writer creates the schema");

    let connection = rusqlite::Connection::open(root.join("state.db")).unwrap();
    let refused = connection.execute(
        "INSERT INTO openbutler_actions (
            request_id, workspace_root, session_id, provider_id, tool_name, state,
            effect_json, input_json, input_sha256, created_at, updated_at
        ) VALUES ('bad', 'w', 's', 'host', 't', 'bogus', '{}', '{}', 'x', 'now', 'now')",
        [],
    );
    assert!(
        refused.is_err(),
        "CHECK (state IN (…)) rejects a state the array does not know"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn opening_twice_is_idempotent_and_derives_the_skill_name() {
    let root = temp_root("reopen");
    LedgerWriter::open(&root).expect("first open creates");
    LedgerWriter::open(&root).expect("second open is a no-op");

    let mut ledger = LedgerWriter::open(&root).unwrap();
    let mut update = action("req-4", ToolActionState::Proposed);
    update.input =
        json!({ "path": "C:\\Users\\x\\AppData\\Local\\openbutler\\skills\\bug-reaper\\SKILL.md" });
    ledger.record_tool_action(&update).unwrap();

    let rows = query(
        &root,
        "SELECT skill FROM openbutler_actions WHERE request_id = 'req-4'",
    );
    assert_eq!(
        rows[0][0], "bug-reaper",
        "the skill segment beside /skills/ is recorded at write time"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn oversized_json_lands_in_the_truncation_envelope() {
    let root = temp_root("envelope");
    let mut ledger = LedgerWriter::open(&root).unwrap();

    let huge = "x".repeat(40_000);
    let mut update = action("req-5", ToolActionState::Proposed);
    update.input = Value::String(huge.clone());
    ledger.record_tool_action(&update).unwrap();

    let rows = query(
        &root,
        "SELECT input_json FROM openbutler_actions WHERE request_id = 'req-5'",
    );
    let stored: Value = serde_json::from_str(&rows[0][0]).unwrap();
    assert_eq!(stored["status"], "truncated");
    // JS `serialized.length` counts the surrounding quotes too — 40 002
    // UTF-16 units for a 40 000-char payload.
    assert_eq!(stored["originalChars"], 40_002);
    assert_eq!(
        stored["preview"].as_str().unwrap().len(),
        32_000,
        "the preview keeps MAX_STORED_JSON_CHARS"
    );

    let _ = std::fs::remove_dir_all(&root);
}
