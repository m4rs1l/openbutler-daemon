//! Activity-ledger feed pins: `ledger.events` / `ledger.actions` read
//! state.db without ever creating it, clamp like the source, and
//! `events.retry` moves only failed/interrupted events back to pending.
//!
//! Source: `daemon-host.ts` ledger arms + `activity-ledger.ts`
//! `listEvents` / `listActions` / `retryEvent` / `setState`
//! (read 2026-11-16). The scheduler pump behind a queued retry is the
//! runtime slice and is not part of this build.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

use openbutler_daemon::dispatch::{DispatchState, Host, handle};
use openbutler_protocol::DaemonRequest;
use serde_json::{Value, json};

fn temp_data_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ob-ledger-{}-{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn request(action: &str, limit: Option<Value>) -> DaemonRequest {
    let mut request = DaemonRequest::new("t-ledger", action);
    if let Some(limit) = limit {
        request = request.with("limit", limit);
    }
    request
}

fn dispatch(
    host: &Host,
    dir: &Path,
    request: DaemonRequest,
) -> openbutler_protocol::DaemonResponse {
    let mut state = DispatchState::new();
    handle(host, dir, &mut state, &request)
}

fn success_json(response: openbutler_protocol::DaemonResponse) -> Value {
    let openbutler_protocol::DaemonResponse::Success { result, .. } = response else {
        panic!("expected success, got {response:?}");
    };
    result
}

fn failure_text(response: openbutler_protocol::DaemonResponse) -> String {
    let openbutler_protocol::DaemonResponse::Failure { error, .. } = response else {
        panic!("expected failure, got {response:?}");
    };
    error
}

/// Creates the two feed tables plus the state table the retry marker
/// uses — the columns the source schema defines for the read surface.
fn create_feed_tables(dir: &Path) {
    let connection = rusqlite::Connection::open(dir.join("state.db")).expect("open fixture db");
    connection
        .execute_batch(
            "CREATE TABLE openbutler_events (
                event_id TEXT PRIMARY KEY,
                source TEXT NOT NULL,
                kind TEXT NOT NULL,
                dedupe_key TEXT NOT NULL UNIQUE,
                workspace_root TEXT,
                occurred_at TEXT NOT NULL,
                received_at TEXT NOT NULL,
                status TEXT NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0,
                payload_json TEXT NOT NULL,
                error TEXT,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE openbutler_actions (
                request_id TEXT PRIMARY KEY,
                workspace_root TEXT NOT NULL,
                session_id TEXT NOT NULL,
                event_id TEXT,
                provider_id TEXT NOT NULL,
                tool_name TEXT NOT NULL,
                state TEXT NOT NULL,
                effect_json TEXT NOT NULL,
                input_json TEXT NOT NULL,
                result_json TEXT,
                error TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE openbutler_state (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .expect("create fixture tables");
}

fn insert_event(dir: &Path, id: &str, source: &str, status: &str, received_at: &str) {
    let connection = rusqlite::Connection::open(dir.join("state.db")).expect("open fixture db");
    connection
        .execute(
            "INSERT INTO openbutler_events
             (event_id, source, kind, dedupe_key, workspace_root, occurred_at, received_at,
              status, attempts, payload_json, error, updated_at)
             VALUES (?1, ?2, 'schedule.requested', ?1, NULL, ?3, ?3, ?4, 1, '{}', NULL, ?3)",
            rusqlite::params![id, source, received_at, status],
        )
        .expect("insert fixture event");
}

#[test]
fn a_missing_database_is_an_empty_feed_and_creates_nothing() {
    let dir = temp_data_dir("absent");
    let host = Host::new();

    let events = success_json(dispatch(&host, &dir, request("ledger.events", None)));
    let actions = success_json(dispatch(&host, &dir, request("ledger.actions", None)));

    assert_eq!(events, json!([]));
    assert_eq!(actions, json!([]));
    assert!(
        !dir.join("state.db").exists(),
        "reading the feed never creates the database"
    );
}

#[test]
fn an_empty_database_is_an_empty_feed() {
    let dir = temp_data_dir("empty");
    let host = Host::new();
    rusqlite::Connection::open(dir.join("state.db")).expect("create empty db");

    let events = success_json(dispatch(&host, &dir, request("ledger.events", None)));

    assert_eq!(
        events,
        json!([]),
        "a database without the table reads empty"
    );
}

#[test]
fn events_arrive_newest_first_and_the_limit_clamps() {
    let dir = temp_data_dir("order");
    let host = Host::new();
    create_feed_tables(&dir);
    insert_event(
        &dir,
        "e1",
        "schedule",
        "delivered",
        "2026-11-16T02:00:00.000Z",
    );
    insert_event(
        &dir,
        "e2",
        "schedule",
        "pending",
        "2026-11-16T03:00:00.000Z",
    );
    insert_event(
        &dir,
        "e3",
        "notification",
        "failed",
        "2026-11-16T04:00:00.000Z",
    );

    let default: Value = success_json(dispatch(&host, &dir, request("ledger.events", None)));
    let ids: Vec<&str> = default
        .as_array()
        .expect("array")
        .iter()
        .map(|row| row["id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids, vec!["e3", "e2", "e1"], "received_at DESC");

    let clamped: Value = success_json(dispatch(
        &host,
        &dir,
        request("ledger.events", Some(json!(500))),
    ));
    assert_eq!(
        clamped.as_array().expect("array").len(),
        3,
        "500 clamps to 200 but the fixture only has three rows"
    );

    let zero: Value = success_json(dispatch(
        &host,
        &dir,
        request("ledger.events", Some(json!(0))),
    ));
    assert_eq!(
        zero.as_array().expect("array").len(),
        1,
        "a zero limit clamps to one row"
    );

    // A non-number limit falls back to the default, like the source's
    // `typeof request.limit === 'number'` check.
    let ignored: Value = success_json(dispatch(
        &host,
        &dir,
        request("ledger.events", Some(json!("lots"))),
    ));
    assert_eq!(ignored.as_array().expect("array").len(), 3);
}

#[test]
fn actions_map_their_columns_onto_the_wire_shape() {
    let dir = temp_data_dir("actions");
    let host = Host::new();
    create_feed_tables(&dir);
    {
        let connection = rusqlite::Connection::open(dir.join("state.db")).expect("open");
        connection
            .execute(
                "INSERT INTO openbutler_actions
                 (request_id, workspace_root, session_id, event_id, provider_id, tool_name,
                  state, effect_json, input_json, result_json, error, created_at, updated_at)
                 VALUES ('req-1', '/srv', 'sess', NULL, 'vercel', 'shell', 'completed',
                         '{\"change\":\"execute\"}', '{\"command\":\"ls\"}', NULL, NULL,
                         '2026-11-16T04:00:00.000Z', '2026-11-16T04:00:02.000Z')",
                [],
            )
            .expect("insert fixture action");
    }

    let actions = success_json(dispatch(&host, &dir, request("ledger.actions", None)));
    let row = &actions.as_array().expect("array")[0];

    assert_eq!(row["requestId"], "req-1");
    assert_eq!(row["name"], "shell", "tool_name maps to name");
    assert_eq!(row["eventId"], Value::Null, "absent column is null");
    assert_eq!(row["result"], Value::Null);
    assert_eq!(row["effect"]["change"], "execute");
    assert_eq!(row["input"]["command"], "ls");
    assert_eq!(row["updatedAt"], "2026-11-16T04:00:02.000Z");
}

#[test]
fn retry_moves_a_failed_event_back_to_pending_and_records_the_marker() {
    let dir = temp_data_dir("retry");
    let host = Host::new();
    create_feed_tables(&dir);
    insert_event(&dir, "e1", "schedule", "failed", "2026-11-16T02:00:00.000Z");

    let response = dispatch(
        &host,
        &dir,
        DaemonRequest::new("t-retry", "events.retry").with("eventId", "e1"),
    );
    let result = success_json(response);
    assert_eq!(result["queued"], true);

    let connection = rusqlite::Connection::open(dir.join("state.db")).expect("open");
    let (status, error): (String, Option<String>) = connection
        .query_row(
            "SELECT status, error FROM openbutler_events WHERE event_id = 'e1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("row survives");
    assert_eq!(status, "pending");
    assert_eq!(error, None, "the refusal text is cleared");

    let marker: String = connection
        .query_row(
            "SELECT value FROM openbutler_state WHERE key = 'notification:retry:e1'",
            [],
            |row| row.get(0),
        )
        .expect("retry marker is recorded");
    assert_eq!(marker, "0");
}

#[test]
fn a_pending_notification_shortcuts_without_touching_attempts() {
    let dir = temp_data_dir("shortcut");
    let host = Host::new();
    create_feed_tables(&dir);
    insert_event(
        &dir,
        "e1",
        "notification",
        "pending",
        "2026-11-16T02:00:00.000Z",
    );

    let response = dispatch(
        &host,
        &dir,
        DaemonRequest::new("t-retry", "events.retry").with("eventId", "e1"),
    );
    let result = success_json(response);
    assert_eq!(result["queued"], true);

    let connection = rusqlite::Connection::open(dir.join("state.db")).expect("open");
    let attempts: i64 = connection
        .query_row(
            "SELECT attempts FROM openbutler_events WHERE event_id = 'e1'",
            [],
            |row| row.get(0),
        )
        .expect("row survives");
    assert_eq!(attempts, 1, "the shortcut runs no UPDATE at all");
    let marker: i64 = connection
        .query_row(
            "SELECT count(*) FROM openbutler_state WHERE key = 'notification:retry:e1'",
            [],
            |row| row.get(0),
        )
        .expect("marker row");
    assert_eq!(marker, 1, "the marker is still written");
}

#[test]
fn a_delivered_event_and_a_missing_event_queue_nothing() {
    let dir = temp_data_dir("noqueue");
    let host = Host::new();
    create_feed_tables(&dir);
    insert_event(
        &dir,
        "e1",
        "schedule",
        "delivered",
        "2026-11-16T02:00:00.000Z",
    );

    let delivered = success_json(dispatch(
        &host,
        &dir,
        DaemonRequest::new("t-retry", "events.retry").with("eventId", "e1"),
    ));
    assert_eq!(delivered["queued"], false);

    let missing = success_json(dispatch(
        &host,
        &dir,
        DaemonRequest::new("t-retry", "events.retry").with("eventId", "ghost"),
    ));
    assert_eq!(missing["queued"], false);

    let connection = rusqlite::Connection::open(dir.join("state.db")).expect("open");
    let markers: i64 = connection
        .query_row("SELECT count(*) FROM openbutler_state", [], |row| {
            row.get(0)
        })
        .expect("marker count");
    assert_eq!(markers, 0, "no marker for a refused retry");
}

#[test]
fn retry_without_an_event_id_uses_the_payload_message() {
    let dir = temp_data_dir("noeventid");
    let host = Host::new();
    create_feed_tables(&dir);

    let error = failure_text(dispatch(
        &host,
        &dir,
        DaemonRequest::new("t-retry", "events.retry"),
    ));
    assert_eq!(error, "eventId must be a non-empty string under 64 bytes.");
}
