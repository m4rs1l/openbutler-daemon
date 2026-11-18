//! The activity ledger read side: `openbutler_events` /
//! `openbutler_actions` from `state.db`, and moving a retryable event
//! back to pending.
//!
//! Source: `activity-ledger.ts` `listEvents` / `listActions` /
//! `retryEvent` (read 2026-11-16 and 2026-11-17). Reads still never
//! create `state.db`; the writer half owns the one writable open.

use std::path::Path;

use openbutler_protocol::ledger::{LedgerAction, LedgerEvent};
use rusqlite::{Connection, OptionalExtension, Row};
use serde_json::{Value, json};

use super::{LedgerError, now_iso};

/// `ledger.events`: newest first, limit clamped to 1..=200, default 30.
/// A missing database or a missing table is an empty feed — and reading
/// never creates the file.
pub fn list_events(
    data_dir: &Path,
    limit: Option<&Value>,
) -> Result<Vec<LedgerEvent>, LedgerError> {
    let Some(connection) = open_readonly(data_dir)? else {
        return Ok(Vec::new());
    };
    if !table_exists(&connection, "openbutler_events")? {
        return Ok(Vec::new());
    }
    let bounded = bounded_limit(limit);
    let mut statement =
        connection.prepare("SELECT * FROM openbutler_events ORDER BY received_at DESC LIMIT ?1")?;
    let rows = statement
        .query_map([bounded], event_from_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// `ledger.actions`: newest first with the same clamp.
pub fn list_actions(
    data_dir: &Path,
    limit: Option<&Value>,
) -> Result<Vec<LedgerAction>, LedgerError> {
    let Some(connection) = open_readonly(data_dir)? else {
        return Ok(Vec::new());
    };
    if !table_exists(&connection, "openbutler_actions")? {
        return Ok(Vec::new());
    }
    let bounded = bounded_limit(limit);
    let mut statement =
        connection.prepare("SELECT * FROM openbutler_actions ORDER BY created_at DESC LIMIT ?1")?;
    let rows = statement
        .query_map([bounded], action_from_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// `events.retry`: failed or interrupted events return to `pending` and
/// record the retry marker; a pending notification shortcut queues
/// without touching the row at all. `true` means queued — the scheduler
/// pump that consumes the queue is the runtime slice.
pub fn retry_event(data_dir: &Path, event_id: &str) -> Result<bool, LedgerError> {
    let path = data_dir.join("state.db");
    if !path.exists() {
        return Ok(false);
    }
    let connection = Connection::open(path)?;
    if !table_exists(&connection, "openbutler_events")? {
        return Ok(false);
    }
    let current: Option<(String, String)> = connection
        .query_row(
            "SELECT source, status FROM openbutler_events WHERE event_id = ?1",
            [event_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((source, status)) = current else {
        return Ok(false);
    };
    let queued = if source == "notification" && status == "pending" {
        true
    } else {
        let changed = connection.execute(
            "UPDATE openbutler_events SET status = 'pending', error = NULL, updated_at = ?1
             WHERE event_id = ?2 AND status IN ('failed','interrupted')",
            rusqlite::params![now_iso(), event_id],
        )?;
        changed > 0
    };
    if queued {
        connection.execute(
            "INSERT INTO openbutler_state (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![format!("notification:retry:{event_id}"), "0"],
        )?;
    }
    Ok(queued)
}

/// The source's `typeof limit === 'number' ? limit : 30` followed by
/// `Math.max(1, Math.min(200, Math.trunc(limit)))`.
fn bounded_limit(limit: Option<&Value>) -> i64 {
    let fallback = 30;
    let Some(Value::Number(number)) = limit else {
        return fallback;
    };
    let truncated = number
        .as_f64()
        .map_or(fallback, |value| value.trunc() as i64);
    truncated.clamp(1, 200)
}

/// A read-only connection, or `None` when the database does not exist:
/// reading must never bring a database into being.
pub(super) fn open_readonly(data_dir: &Path) -> Result<Option<Connection>, LedgerError> {
    let path = data_dir.join("state.db");
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?))
}

fn table_exists(connection: &Connection, name: &str) -> Result<bool, LedgerError> {
    let found: Option<i64> = connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [name],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

/// `eventFromRow`: `payload` parses to `{}` for anything that is not an
/// object — including invalid JSON, like the source's `parseRecord`.
fn event_from_row(row: &Row<'_>) -> rusqlite::Result<LedgerEvent> {
    Ok(LedgerEvent {
        id: row.get("event_id")?,
        source: row.get("source")?,
        kind: row.get("kind")?,
        dedupe_key: row.get("dedupe_key")?,
        workspace_root: row.get("workspace_root")?,
        occurred_at: row.get("occurred_at")?,
        received_at: row.get("received_at")?,
        status: row.get("status")?,
        attempts: row.get("attempts")?,
        payload: parse_record(&row.get::<_, String>("payload_json")?),
        error: row.get("error")?,
    })
}

/// `actionFromRow`: `effect` and `input` follow the same `{}` fallback;
/// `result` is null for a null column or any parse failure (`parseJson`).
fn action_from_row(row: &Row<'_>) -> rusqlite::Result<LedgerAction> {
    let result_json: Option<String> = row.get("result_json")?;
    Ok(LedgerAction {
        request_id: row.get("request_id")?,
        workspace_root: row.get("workspace_root")?,
        session_id: row.get("session_id")?,
        event_id: row.get("event_id")?,
        provider_id: row.get("provider_id")?,
        name: row.get("tool_name")?,
        state: row.get("state")?,
        effect: parse_record(&row.get::<_, String>("effect_json")?),
        input: parse_record(&row.get::<_, String>("input_json")?),
        result: result_json.and_then(|text| serde_json::from_str::<Value>(&text).ok()),
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        error: row.get("error")?,
    })
}

fn parse_record(text: &str) -> Value {
    match serde_json::from_str::<Value>(text) {
        Ok(value) if value.is_object() => value,
        _ => json!({}),
    }
}
