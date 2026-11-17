//! The activity ledger: read `openbutler_events` / `openbutler_actions`
//! from `state.db`, move a retryable event back to pending, and — the
//! writer half — record tool actions with their write-ahead proposal and
//! boot recovery.
//!
//! Source: `activity-ledger.ts` `listEvents` / `listActions` /
//! `retryEvent` / `setState` / `recordToolAction` /
//! `recoverInterruptedActions` (read 2026-11-16 and 2026-11-17). Reads
//! still never create `state.db`; `LedgerWriter` is the one writable
//! open, owning the schema for the tables this slice uses.

use std::path::Path;

use openbutler_protocol::ledger::{LedgerAction, LedgerEvent};
use rusqlite::{Connection, OptionalExtension, Row};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// One refusal from the ledger surface. The message *is* the contract.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("{0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The write-ahead record is the gate in front of every other state
    /// (`recordToolAction` in the source).
    #[error("An action must be recorded before execution.")]
    MissingAction,
}

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
fn open_readonly(data_dir: &Path) -> Result<Option<Connection>, LedgerError> {
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

/// The retry marker's timestamp, shaped like JS `toISOString()`.
fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

// --- the writer: tool-action write path (c2a) --------------------------------

/// The nine action states — **the single authority**: the type below and
/// the table's `CHECK` constraint are both built from this array (the
/// source keeps the same array as type and constraint).
pub const TOOL_ACTION_STATES: [&str; 9] = [
    "proposed",
    "reviewed",
    "review_unavailable",
    "approved",
    "denied",
    "started",
    "completed",
    "failed",
    "cancelled",
];

/// The source's `TERMINAL_ACTION_STATES` — everything else is what boot
/// recovery sweeps to `failed`.
const TERMINAL_ACTION_STATES: [&str; 3] = ["completed", "failed", "cancelled"];

/// A row's lifecycle position (`TOOL_ACTION_STATES` as a type).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolActionState {
    Proposed,
    Reviewed,
    ReviewUnavailable,
    Approved,
    Denied,
    Started,
    Completed,
    Failed,
    Cancelled,
}

impl ToolActionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Reviewed => "reviewed",
            Self::ReviewUnavailable => "review_unavailable",
            Self::Approved => "approved",
            Self::Denied => "denied",
            Self::Started => "started",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(self) -> bool {
        TERMINAL_ACTION_STATES.contains(&self.as_str())
    }
}

/// One write to the tool-action table. Owned: the executor builds one per
/// call and the ledger serialises it inside one transaction.
#[derive(Debug, Clone)]
pub struct ToolActionUpdate {
    pub request_id: String,
    pub workspace_root: String,
    pub session_id: String,
    pub event_id: Option<String>,
    pub provider_id: String,
    pub name: String,
    pub input: Value,
    pub effect: Value,
    pub state: ToolActionState,
    pub result: Option<Value>,
    pub error: Option<String>,
}

/// `MAX_STORED_JSON_CHARS` — above this a stored JSON value becomes the
/// source's truncation envelope instead of the payload.
const MAX_STORED_JSON_CHARS: usize = 32_000;

/// The two tool-action tables and their indexes: `LEDGER_SCHEMA`'s slice
/// for this writer. The CHECK list is generated from
/// `TOOL_ACTION_STATES`, so a state added to the array cannot drift out of
/// the schema (the source's `stateCheck` trick).
fn tool_action_schema() -> String {
    let states = TOOL_ACTION_STATES
        .iter()
        .map(|state| format!("'{state}'"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "CREATE TABLE IF NOT EXISTS openbutler_actions (
            request_id TEXT PRIMARY KEY,
            workspace_root TEXT NOT NULL,
            session_id TEXT NOT NULL,
            event_id TEXT,
            provider_id TEXT NOT NULL,
            tool_name TEXT NOT NULL,
            skill TEXT,
            state TEXT NOT NULL CHECK (state IN ({states})),
            effect_json TEXT NOT NULL,
            input_json TEXT NOT NULL,
            input_sha256 TEXT NOT NULL,
            result_json TEXT,
            error TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            completed_at TEXT
        );
        CREATE INDEX IF NOT EXISTS openbutler_actions_session_created_idx
            ON openbutler_actions(session_id, created_at);
        CREATE INDEX IF NOT EXISTS openbutler_actions_state_updated_idx
            ON openbutler_actions(state, updated_at);
        CREATE TABLE IF NOT EXISTS openbutler_action_transitions (
            id INTEGER PRIMARY KEY,
            request_id TEXT NOT NULL,
            state TEXT NOT NULL,
            detail_json TEXT,
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS openbutler_action_transitions_request_idx
            ON openbutler_action_transitions(request_id, id);"
    )
}

/// The one writable ledger open: creates the database when absent, sets
/// the source's durable pragmas (WAL + synchronous FULL), and owns the
/// tool-action schema. One host, one writer — the start lock elsewhere in
/// the daemon keeps that true.
pub struct LedgerWriter {
    connection: Connection,
}

impl LedgerWriter {
    pub fn open(data_dir: &Path) -> Result<LedgerWriter, LedgerError> {
        let connection = Connection::open(data_dir.join("state.db"))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.execute_batch(&tool_action_schema())?;
        Ok(LedgerWriter { connection })
    }

    /// `recordToolAction`: `proposed` inserts idempotently (a duplicate
    /// proposal is a no-op with no transition); every other state
    /// **requires the row to exist** — the write-ahead gate — and appends
    /// one transition row. One `BEGIN IMMEDIATE` transaction around both
    /// statements, rolled back on any failure.
    pub fn record_tool_action(&mut self, action: &ToolActionUpdate) -> Result<(), LedgerError> {
        let now = now_iso();
        let input = safe_json(&action.input);
        let input_hash = {
            let serialized =
                serde_json::to_string(&action.input).unwrap_or_else(|_| String::from("null"));
            format!("{:x}", Sha256::digest(serialized.as_bytes()))
        };
        let result = action.result.as_ref().map(safe_json);
        let completed_at = action.state.is_terminal().then_some(now.as_str());
        let skill = skill_name_from_path(action.input.get("path").and_then(Value::as_str));

        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        let outcome = (|| -> Result<(), LedgerError> {
            if action.state == ToolActionState::Proposed {
                let inserted = self.connection.execute(
                    "INSERT INTO openbutler_actions (
                        request_id, workspace_root, session_id, event_id, provider_id, tool_name,
                        skill, state, effect_json, input_json, input_sha256, result_json, error,
                        created_at, updated_at, completed_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14, ?15)
                     ON CONFLICT(request_id) DO NOTHING",
                    rusqlite::params![
                        action.request_id,
                        action.workspace_root,
                        action.session_id,
                        action.event_id,
                        action.provider_id,
                        action.name,
                        skill,
                        action.state.as_str(),
                        safe_json(&action.effect),
                        input,
                        input_hash,
                        result,
                        action.error,
                        now,
                        completed_at,
                    ],
                )?;
                if inserted == 0 {
                    return Ok(());
                }
            } else {
                let updated = self.connection.execute(
                    "UPDATE openbutler_actions
                     SET state = ?1, result_json = COALESCE(?2, result_json), error = ?3,
                         updated_at = ?4, completed_at = COALESCE(?5, completed_at)
                     WHERE request_id = ?6",
                    rusqlite::params![
                        action.state.as_str(),
                        result,
                        action.error,
                        now,
                        completed_at,
                        action.request_id,
                    ],
                )?;
                if updated == 0 {
                    return Err(LedgerError::MissingAction);
                }
            }
            let detail = result.clone().or_else(|| {
                action
                    .error
                    .as_ref()
                    .map(|error| safe_json(&json!({ "error": error })))
            });
            self.connection.execute(
                "INSERT INTO openbutler_action_transitions (request_id, state, detail_json, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![action.request_id, action.state.as_str(), detail, now],
            )?;
            Ok(())
        })();
        match outcome {
            Ok(()) => {
                self.connection.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(error) => {
                let _ = self.connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    /// `recoverInterruptedActions`: everything not terminal had an outcome
    /// the host never recorded — it becomes `failed` with the source's
    /// exact wording, never resumable. Transitions land first (inside the
    /// same transaction) so the audit trail shows the recovery itself.
    pub fn recover_interrupted_actions(&mut self) -> Result<u64, LedgerError> {
        let now = now_iso();
        let message = "Host stopped before recording the action outcome. Inspect the target; do not replay this action automatically.";
        let unresolved = TOOL_ACTION_STATES
            .iter()
            .filter(|state| !TERMINAL_ACTION_STATES.contains(state))
            .map(|state| format!("'{state}'"))
            .collect::<Vec<_>>()
            .join(", ");
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        let outcome = (|| -> Result<u64, LedgerError> {
            self.connection.execute(
                &format!(
                    "INSERT INTO openbutler_action_transitions (request_id, state, detail_json, created_at)
                     SELECT request_id, 'failed', ?1, ?2 FROM openbutler_actions WHERE state IN ({unresolved})"
                ),
                rusqlite::params![safe_json(&json!({ "error": message })), now],
            )?;
            let changed = self.connection.execute(
                &format!(
                    "UPDATE openbutler_actions SET state = 'failed', error = ?1, updated_at = ?2, completed_at = ?2
                     WHERE state IN ({unresolved})"
                ),
                rusqlite::params![message, now],
            )?;
            Ok(changed as u64)
        })();
        match outcome {
            Ok(changed) => {
                self.connection.execute_batch("COMMIT")?;
                Ok(changed)
            }
            Err(error) => {
                let _ = self.connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }
}

/// `safeJson` minus the redaction engine (which lands with the executor
/// slice — no real model input reaches this path before then): serialize,
/// and anything above the stored budget becomes the truncation envelope.
/// The preview and the byte count are UTF-16 units, matching JS
/// `String.prototype.slice`/`length`.
fn safe_json(value: &Value) -> String {
    let serialized = serde_json::to_string(value).unwrap_or_else(|_| String::from("null"));
    if serialized.len() <= MAX_STORED_JSON_CHARS {
        return serialized;
    }
    let original_chars = utf16_len(&serialized);
    let preview = crate::state::slice_utf16(&serialized, MAX_STORED_JSON_CHARS);
    json!({
        "status": "truncated",
        "preview": preview,
        "originalChars": original_chars,
    })
    .to_string()
}

fn utf16_len(value: &str) -> usize {
    value.chars().map(char::len_utf16).sum()
}

/// `skillNameFromPath`: the segment beside any `/skills/` directory, so
/// skill usage is recorded data rather than a report-time guess. The
/// marker search is case-insensitive (as in the source); the sliced
/// segment keeps its own case.
fn skill_name_from_path(path: Option<&str>) -> Option<String> {
    let path = path?;
    let normalized = path.replace('\\', "/");
    let marker = normalized.to_lowercase().find("/skills/")?;
    let segment = normalized
        .get(marker + "/skills/".len()..)?
        .split('/')
        .next()
        .unwrap_or("");
    (!segment.is_empty() && segment != "SKILL.md").then(|| segment.to_owned())
}
