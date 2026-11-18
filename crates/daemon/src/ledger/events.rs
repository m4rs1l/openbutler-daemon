//! The observed-event write path — `insertEvent` in the source
//! (`activity-ledger.ts:409-434`, read 2026-11-18): validation, the
//! payload size ceilings, and the dedupe insert. The row shape and the
//! table live with the ledger schema; this file owns what a publisher
//! may write.

use crate::shape::js_len;
use rusqlite::OptionalExtension;
use serde_json::Value;

use super::{LedgerError, LedgerWriter, now_iso};

/// `OBSERVED_EVENT_STATES` — the source's array, in order; the table's
/// CHECK clause is generated from it (the source's `stateCheck` trick).
pub const OBSERVED_EVENT_STATES: [&str; 6] = [
    "pending",
    "processing",
    "delivered",
    "failed",
    "ignored",
    "interrupted",
];

/// `ObservedEventInput` — what a publisher submits.
#[derive(Debug, Clone)]
pub struct EventInput {
    pub source: String,
    pub kind: String,
    pub dedupe_key: String,
    pub workspace_root: Option<String>,
    /// ISO timestamp; absent means "now" (the source's `occurredAt ?? now`).
    pub occurred_at: Option<String>,
    pub payload: Value,
}

/// `validateEvent`: three length gates in UTF-16 units (JS `.length`) and
/// the workspace-root ceiling, each with the source's message.
fn validate_event(event: &EventInput) -> Result<(), LedgerError> {
    for (label, value, max) in [
        ("source", &event.source, 64_usize),
        ("kind", &event.kind, 128_usize),
        ("dedupeKey", &event.dedupe_key, 512_usize),
    ] {
        let length = js_len(value);
        if length == 0 || length > max {
            return Err(LedgerError::Refusal(format!(
                "Observed event {label} must be 1-{max} characters."
            )));
        }
    }
    if let Some(root) = &event.workspace_root
        && js_len(root) > 32_768
    {
        return Err(LedgerError::Refusal(
            "Observed event workspace root is too long.".to_owned(),
        ));
    }
    Ok(())
}

impl LedgerWriter {
    /// `insertEvent`: validate, size-check the serialized payload
    /// (`notification` gets 32 MiB, everything else 256 KiB — UTF-8
    /// bytes, as `Buffer.byteLength` counted them), then insert with
    /// `ON CONFLICT(dedupe_key) DO NOTHING`. Returns the row id and
    /// whether this call created it; a conflict returns the existing id.
    pub fn insert_event(&self, event: &EventInput) -> Result<(String, bool), LedgerError> {
        validate_event(event)?;
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_iso();
        let occurred_at = event.occurred_at.clone().unwrap_or_else(|| now.clone());
        let payload = serde_json::to_string(&event.payload)
            .map_err(|error| LedgerError::Refusal(error.to_string()))?;
        let payload_limit = if event.source == "notification" {
            32 * 1024 * 1024
        } else {
            256 * 1024
        };
        if payload.len() > payload_limit {
            return Err(LedgerError::Refusal(
                "Observed event payload exceeded its size limit.".to_owned(),
            ));
        }
        let inserted = self.connection.execute(
            "INSERT INTO openbutler_events (
                event_id, source, kind, dedupe_key, workspace_root, occurred_at, received_at,
                status, attempts, payload_json, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', 0, ?8, ?9)
             ON CONFLICT(dedupe_key) DO NOTHING",
            rusqlite::params![
                id,
                event.source,
                event.kind,
                event.dedupe_key,
                event.workspace_root,
                occurred_at,
                now,
                payload,
                now
            ],
        )?;
        if inserted > 0 {
            return Ok((id, true));
        }
        let existing: Option<String> = self
            .connection
            .query_row(
                "SELECT event_id FROM openbutler_events WHERE dedupe_key = ?1",
                [&event.dedupe_key],
                |row| row.get(0),
            )
            .optional()?;
        Ok((existing.unwrap_or(id), false))
    }
}
