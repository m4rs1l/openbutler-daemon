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

use super::{LedgerError, LedgerWriter, now_iso};

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
    retry_event_on(&connection, event_id)
}

/// The retry body, shared by the data-dir entry point and the runner's
/// writer connection (one implementation, two doors).
fn retry_event_on(connection: &Connection, event_id: &str) -> Result<bool, LedgerError> {
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

/// `scheduleEvents`: newest-first events of one schedule for run
/// history — the payload's `scheduleId` (or the pre-rename `taskId`)
/// selects the rows, newest `received_at` first with `rowid` as the
/// tie-break, limit clamped to 1..=200. Shared by the read door and the
/// runner's writer connection (one query, two doors).
fn schedule_events_on(
    connection: &Connection,
    schedule_id: &str,
    limit: i64,
) -> Result<Vec<LedgerEvent>, LedgerError> {
    let bounded = limit.clamp(1, 200);
    let mut statement = connection.prepare(
        "SELECT * FROM openbutler_events
         WHERE COALESCE(json_extract(payload_json, '$.scheduleId'), json_extract(payload_json, '$.taskId')) = ?1
         ORDER BY received_at DESC, rowid DESC LIMIT ?2",
    )?;
    let rows = statement
        .query_map(rusqlite::params![schedule_id, bounded], event_from_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The read door for callers that hold only a data directory (the
/// `log` action). A missing database or table is an empty history.
pub fn schedule_events(
    data_dir: &Path,
    schedule_id: &str,
    limit: i64,
) -> Result<Vec<LedgerEvent>, LedgerError> {
    let Some(connection) = open_readonly(data_dir)? else {
        return Ok(Vec::new());
    };
    if !table_exists(&connection, "openbutler_events")? {
        return Ok(Vec::new());
    }
    schedule_events_on(&connection, schedule_id, limit)
}

impl LedgerWriter {
    /// The data-dir read door for a schedule's run history.
    pub fn schedule_events(
        &self,
        schedule_id: &str,
        limit: i64,
    ) -> Result<Vec<LedgerEvent>, LedgerError> {
        schedule_events_on(&self.connection, schedule_id, limit)
    }

    /// The pump's opening read — the source's `pendingEvents` narrowed
    /// to the schedule lane this build produces: every pending
    /// schedule-sourced event (registered or not, so an unknown one can
    /// be ignored), oldest first like the source's `received_at`
    /// ascending walk.
    pub fn pending_schedule_events(&self, limit: i64) -> Result<Vec<LedgerEvent>, LedgerError> {
        let bounded = limit.clamp(1, 1000);
        let mut statement = self.connection.prepare(
            "SELECT * FROM openbutler_events
             WHERE status = 'pending' AND source LIKE 'schedule%'
             ORDER BY received_at ASC, rowid ASC LIMIT ?1",
        )?;
        let rows = statement
            .query_map(rusqlite::params![bounded], event_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// `getEvent`: one row by id.
    pub fn get_event(&self, event_id: &str) -> Result<Option<LedgerEvent>, LedgerError> {
        let row = self
            .connection
            .query_row(
                "SELECT * FROM openbutler_events WHERE event_id = ?1",
                [event_id],
                event_from_row,
            )
            .optional()?;
        Ok(row)
    }

    /// `claimEvent`: pending → processing with `attempts + 1` and a
    /// cleared error; `None` when the row was not claimable (another
    /// claim, or not pending). The pump's single-claim gate.
    pub fn claim_event(&self, event_id: &str) -> Result<Option<LedgerEvent>, LedgerError> {
        let changed = self.connection.execute(
            "UPDATE openbutler_events
             SET status = 'processing', attempts = attempts + 1, updated_at = ?1, error = NULL
             WHERE event_id = ?2 AND status = 'pending'",
            rusqlite::params![now_iso(), event_id],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.get_event(event_id)
    }

    /// `setEventState`: the plain status/error write (no state guard —
    /// the source has none). The source passes the error through its
    /// redaction engine; that gate is deferred with the tool-output one
    /// (`docs/agent-loop.md`), a cutover prerequisite.
    pub fn set_event_state(
        &self,
        event_id: &str,
        state: &str,
        error: Option<&str>,
    ) -> Result<(), LedgerError> {
        self.connection.execute(
            "UPDATE openbutler_events SET status = ?1, error = ?2, updated_at = ?3
             WHERE event_id = ?4",
            rusqlite::params![state, error, now_iso(), event_id],
        )?;
        Ok(())
    }

    /// `recoverInterruptedEvents`: a restart leaves `processing` rows
    /// split by source — retryable sources go back to `pending`, the
    /// rest become `interrupted`, each with the source's message.
    pub fn recover_interrupted_events(&self) -> Result<u64, LedgerError> {
        let changed = self.connection.execute(
            "UPDATE openbutler_events SET
             status = CASE WHEN source IN ('notification','owner-inbox','worker') OR (source = 'telegram' AND kind LIKE 'inbox.%') THEN 'pending' ELSE 'interrupted' END,
             error = CASE WHEN source = 'owner-inbox' THEN 'Owner message is available for retry after host restart.' ELSE 'Host stopped while this event was being processed.' END,
             updated_at = ?1
             WHERE status = 'processing'",
            [now_iso()],
        )?;
        Ok(changed as u64)
    }

    /// `retryEvent` through the writer's own connection — the runner's
    /// one-shot adapters retry an interrupted fire without a second open.
    pub fn retry_event(&self, event_id: &str) -> Result<bool, LedgerError> {
        retry_event_on(&self.connection, event_id)
    }
}
