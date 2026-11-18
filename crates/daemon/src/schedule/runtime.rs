//! Durable run state for schedules (`readScheduleRuntime`,
//! `writeScheduleRunStarted`, `writeScheduleRunFinished`,
//! `writeScheduleError`, `triggerStateKey`, `readTriggerDue` in the source),
//! stored as `openbutler_state` keys owned by the ledger.
//!
//! Deliberate divergences: the pre-rename `task:` prefix fallback is dropped
//! (user 0 — no upgraded ledger exists), and the report truncation counts
//! Unicode scalar values, exactly what JS `[...str].length` counts. The
//! trigger hash covers Rust's own serialization of the trigger (field order
//! `adapter`, `options`; the options map sorts by key) — the segment is a
//! Rust-written, Rust-read namespace, so the ordering rule only has to be
//! self-consistent.

use crate::ledger::{LedgerError, LedgerWriter};
use crate::state::schedules::ScheduleState;
use sha2::{Digest, Sha256};

/// `MAX_REPORT_CHARS`: the durable report keeps this many characters.
pub const MAX_REPORT_CHARS: usize = 8_000;

/// `ScheduleRunStatus`: an unrecognized stored value reads as `Idle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
/// The durable runtime status of a schedule.
pub enum ScheduleRuntimeStatus {
    Idle,
    Running,
    Error,
}

/// `lastResult`: only these two values are stored or surfaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
/// The stored result of the last run (completed/failed).
pub enum ScheduleRunResult {
    Completed,
    Failed,
}

impl ScheduleRunResult {
    /// The status/result wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

/// Durable runtime of one schedule. Absent keys mean "never ran, idle".
/// Absent options serialize as absent (the source's `undefined` fields),
/// never as `null`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
/// Everything the runtime state carries (status, counts, report).
pub struct ScheduleRuntime {
    pub status: ScheduleRuntimeStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_result: Option<ScheduleRunResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub run_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_report: Option<String>,
}

/// The write path's outcome payload.
pub struct ScheduleRunOutcome<'a> {
    pub result: ScheduleRunResult,
    pub error: Option<&'a str>,
    pub report: Option<&'a str>,
}

/// `runtimeKey`: `schedule:{id}:run:{suffix}`.
fn runtime_key(schedule_id: &str, suffix: &str) -> String {
    format!("schedule:{schedule_id}:run:{suffix}")
}

/// JS `Number(value) > 0 ? value : undefined` over engine-written decimal
/// integers: non-numeric or non-positive reads as absent.
fn positive_number(raw: Option<String>) -> Option<i64> {
    let raw = raw?;
    let number: f64 = raw.parse().ok()?;
    if number.is_finite() && number > 0.0 {
        Some(number as i64)
    } else {
        None
    }
}

/// JS truthiness for stored strings: absent or empty stays absent.
fn non_empty(raw: Option<String>) -> Option<String> {
    raw.filter(|value| !value.is_empty())
}

/// Durable runtime of one schedule, read through any state accessor.
pub fn read_schedule_runtime(
    schedule_id: &str,
    read: impl Fn(&str) -> Option<String>,
) -> ScheduleRuntime {
    let status = match read(&runtime_key(schedule_id, "status")).as_deref() {
        Some("running") => ScheduleRuntimeStatus::Running,
        Some("error") => ScheduleRuntimeStatus::Error,
        _ => ScheduleRuntimeStatus::Idle,
    };
    let last_result = match read(&runtime_key(schedule_id, "result")).as_deref() {
        Some("completed") => Some(ScheduleRunResult::Completed),
        Some("failed") => Some(ScheduleRunResult::Failed),
        _ => None,
    };
    ScheduleRuntime {
        status,
        status_error: non_empty(read(&runtime_key(schedule_id, "statusError"))),
        last_run_at: positive_number(read(&runtime_key(schedule_id, "at"))),
        last_result,
        last_error: non_empty(read(&runtime_key(schedule_id, "error"))),
        run_count: positive_number(read(&runtime_key(schedule_id, "count"))).unwrap_or(0),
        last_report: non_empty(read(&runtime_key(schedule_id, "report"))),
    }
}

/// Marks a run started: `status=running`, the previous `statusError`
/// cleared, and the start instant stored.
pub fn write_schedule_run_started(
    writer: &LedgerWriter,
    schedule_id: &str,
    at_ms: i64,
) -> Result<(), LedgerError> {
    writer.set_state(&runtime_key(schedule_id, "status"), "running")?;
    writer.set_state(&runtime_key(schedule_id, "statusError"), "")?;
    writer.set_state(&runtime_key(schedule_id, "at"), &at_ms.to_string())?;
    Ok(())
}

/// Ends a run: reads the durable count first (the source reads the whole
/// runtime and uses only `runCount`), then writes status, result, count+1,
/// the error slot (failed with no message falls back to `Run failed.`), and
/// the report only when one is present — truncated at `MAX_REPORT_CHARS`
/// code points. `statusError` is cleared here too.
pub fn write_schedule_run_finished(
    writer: &LedgerWriter,
    schedule_id: &str,
    outcome: ScheduleRunOutcome<'_>,
) -> Result<(), LedgerError> {
    let run_count =
        positive_number(writer.state_get(&runtime_key(schedule_id, "count"))?).unwrap_or(0);
    writer.set_state(&runtime_key(schedule_id, "status"), "idle")?;
    writer.set_state(&runtime_key(schedule_id, "statusError"), "")?;
    writer.set_state(&runtime_key(schedule_id, "result"), outcome.result.as_str())?;
    writer.set_state(
        &runtime_key(schedule_id, "count"),
        &(run_count + 1).to_string(),
    )?;
    let error_text = match outcome.result {
        ScheduleRunResult::Failed => outcome.error.unwrap_or("Run failed."),
        ScheduleRunResult::Completed => "",
    };
    writer.set_state(&runtime_key(schedule_id, "error"), error_text)?;
    if let Some(report) = outcome.report {
        let kept: String = if report.chars().count() > MAX_REPORT_CHARS {
            report.chars().take(MAX_REPORT_CHARS).collect()
        } else {
            report.to_owned()
        };
        writer.set_state(&runtime_key(schedule_id, "report"), &kept)?;
    }
    Ok(())
}

/// Marks a schedule non-runnable (workspace gone). Cleared by the next run
/// start; no retry loop.
pub fn write_schedule_error(
    writer: &LedgerWriter,
    schedule_id: &str,
    error: &str,
) -> Result<(), LedgerError> {
    writer.set_state(&runtime_key(schedule_id, "status"), "error")?;
    writer.set_state(&runtime_key(schedule_id, "statusError"), error)?;
    Ok(())
}

/// `triggerStateKey`: `schedule:{id}:{sha256(trigger JSON).hex[0..16]}:{suffix}`.
pub fn trigger_state_key(schedule: &ScheduleState, suffix: &str) -> String {
    // The trigger is a struct of strings and a JSON value — serialization
    // cannot fail for any value serde_json can hold.
    let serialized = serde_json::to_string(&schedule.trigger).unwrap_or_default();
    let digest = format!("{:x}", Sha256::digest(serialized.as_bytes()));
    format!("schedule:{}:{}:{}", schedule.id, &digest[..16], suffix)
}

/// Stored next-due of a fixed-period trigger, for lists and headers. Only
/// the interval adapter stores one.
pub fn read_trigger_due(
    schedule: &ScheduleState,
    read: impl Fn(&str) -> Option<String>,
) -> Option<i64> {
    if schedule.trigger.adapter != "interval" {
        return None;
    }
    positive_number(read(&trigger_state_key(schedule, "due")))
}
