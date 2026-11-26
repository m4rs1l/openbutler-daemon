//! `ledger.prune`: deleting settled ledger history before an explicit
//! cutoff, with a preview that runs the very same selection.
//!
//! Source: `activity-ledger.ts` `PRUNABLE_EVENTS_SQL` / `PRUNABLE_ACTIONS_SQL`
//! / `pruneBefore` / `previewPruneBefore` and the CLI's `runLedgerPrune`
//! (read 2026-11-26). The selection is the source's: age alone never settles
//! work — only delivered/ignored events (never worker identities, never
//! one-shot schedule identities, never an event a read marker has not
//! released, never one an action or a usage row still references) and only
//! terminal actions. The write runs inside the host, so the source's "stop
//! the host before pruning" rule becomes unnecessary (the host is the one
//! writer).

use std::path::Path;

use openbutler_protocol::DaemonRequest;
use rusqlite::Connection;
use serde_json::{Value, json};

use super::db_path;

/// Terminal action states (the source's `TERMINAL_ACTION_STATES`).
const PRUNABLE_ACTIONS_SQL: &str = "
  SELECT request_id FROM openbutler_actions
  WHERE state IN ('completed', 'failed', 'cancelled', 'denied')
    AND created_at < ?1 AND updated_at < ?1
";

/// Settled events with nothing still pointing at them.
const PRUNABLE_EVENTS_SQL: &str = "
  SELECT e.event_id FROM openbutler_events e
  WHERE e.received_at < ?1 AND e.updated_at < ?1
    AND e.status IN ('delivered', 'ignored')
    AND e.source != 'worker'
    AND e.kind NOT IN ('schedule.at', 'schedule.window')
    AND (e.source != 'owner-inbox' OR EXISTS (
      SELECT 1 FROM openbutler_state
      WHERE key = 'owner-inbox:read:' || e.event_id OR key = 'owner-inbox:reply:' || e.event_id
    ))
    AND (e.source != 'agent-inbox' OR e.status = 'ignored' OR EXISTS (
      SELECT 1 FROM openbutler_state WHERE key = 'agent-inbox:read:' || e.event_id
    ))
    AND e.event_id NOT IN (SELECT event_id FROM openbutler_actions WHERE event_id IS NOT NULL)
    AND e.event_id NOT IN (SELECT event_id FROM openbutler_usage WHERE event_id IS NOT NULL)
";

/// `ledger.prune`: `{kind: "events"|"actions", olderThan: "30d", apply?}`.
pub fn prune(data_dir: &Path, request: &DaemonRequest) -> Result<Value, String> {
    let kind = request.get("kind").and_then(Value::as_str).unwrap_or("");
    if kind != "events" && kind != "actions" {
        return Err("kind must be events or actions.".to_owned());
    }
    let older: &str = request
        .get("olderThan")
        .and_then(Value::as_str)
        .unwrap_or("");
    let Some(milliseconds) = parse_duration(older) else {
        return Err(format!(
            "Invalid duration \"{older}\". Use a value like 30d, 12h, 90m, or 2w."
        ));
    };
    let apply = match request.get("apply") {
        None => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return Err("apply must be a boolean.".to_owned()),
    };
    let cutoff = (chrono::Utc::now() - chrono::Duration::milliseconds(milliseconds))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

    let path = db_path(data_dir);
    if !path.exists() {
        // Nothing recorded: an empty selection, and no file is created.
        return Ok(json!({ "events": 0, "actions": 0, "transitions": 0 }));
    }
    let connection = Connection::open(&path).map_err(|error| error.to_string())?;
    connection
        .busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|error| error.to_string())?;

    if kind == "events" {
        let sql = format!("SELECT COUNT(*) FROM ({PRUNABLE_EVENTS_SQL})");
        let selected: i64 = connection
            .query_row(&sql, [&cutoff], |row| row.get(0))
            .map_err(|error| error.to_string())?;
        let deleted = if apply && selected > 0 {
            connection
                .execute(
                    &format!(
                        "DELETE FROM openbutler_events WHERE event_id IN ({PRUNABLE_EVENTS_SQL})"
                    ),
                    [&cutoff],
                )
                .map_err(|error| error.to_string())? as i64
        } else {
            0
        };
        return Ok(json!({
            "events": if apply { deleted } else { selected },
            "actions": 0,
            "transitions": 0
        }));
    }

    let count_sql = format!(
        "WITH selected_actions AS ({PRUNABLE_ACTIONS_SQL})
         SELECT (SELECT COUNT(*) FROM selected_actions),
           (SELECT COUNT(*) FROM openbutler_action_transitions
            WHERE request_id IN (SELECT request_id FROM selected_actions))"
    );
    let (selected_actions, selected_transitions): (i64, i64) = connection
        .query_row(&count_sql, [&cutoff], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(|error| error.to_string())?;
    if !apply {
        return Ok(json!({
            "events": 0,
            "actions": selected_actions,
            "transitions": selected_transitions
        }));
    }
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    let transitions = transaction
        .execute(
            &format!(
                "DELETE FROM openbutler_action_transitions WHERE request_id IN ({PRUNABLE_ACTIONS_SQL})"
            ),
            [&cutoff],
        )
        .map_err(|error| error.to_string())? as i64;
    let actions = transaction
        .execute(
            &format!("DELETE FROM openbutler_actions WHERE request_id IN ({PRUNABLE_ACTIONS_SQL})"),
            [&cutoff],
        )
        .map_err(|error| error.to_string())? as i64;
    transaction.commit().map_err(|error| error.to_string())?;
    Ok(json!({ "events": 0, "actions": actions, "transitions": transitions }))
}

/// `parseRetentionDuration` (retention.ts L9): `<digits><m|h|d|w>`, trimmed
/// and lowercased, a positive safe integer.
fn parse_duration(text: &str) -> Option<i64> {
    let normalized = text.trim().to_lowercase();
    let (digits, unit) = normalized.split_at(normalized.len().checked_sub(1)?);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let value: i128 = digits.parse().ok()?;
    if value <= 0 {
        return None;
    }
    let unit_ms: i128 = match unit {
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        "w" => 604_800_000,
        _ => return None,
    };
    let milliseconds = value * unit_ms;
    // The source requires a safe integer; everything else is a refusal.
    if milliseconds > 9_007_199_254_740_991 {
        return None;
    }
    i64::try_from(milliseconds).ok()
}
