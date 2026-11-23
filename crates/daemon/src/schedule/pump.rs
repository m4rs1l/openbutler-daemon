//! Sched-C: the event pump — `pumpEvents`'s schedule lane feeding
//! `processScheduleEvent` (schedule-service.ts L80-150, re-read
//! 2026-11-18).
//!
//! One authority for consuming pending schedule events: claim, run the
//! wake turn in the origin session or the schedule's persistent
//! background session, and land every outcome in the durable event and
//! runtime state. The pump is the only writer of `delivered`.
//!
//! The writer rides in a `Mutex` (the runner's own shape): rusqlite's
//! connection is `!Sync`, so no guard may span the turn's await — the
//! firing is therefore split into a synchronous prepare and a
//! synchronous finalize around the one await.

use std::path::Path;
use std::sync::Mutex;

use chrono::Utc;
use openbutler_protocol::entry::UserEntrySource;
use openbutler_protocol::ledger::LedgerEvent;
use serde_json::Value;

use crate::agent::executor::TurnExecutor;
use crate::dispatch::Host;
use crate::ledger::LedgerWriter;
use crate::schedule::runtime::{
    read_schedule_runtime, read_trigger_due, write_schedule_error, write_schedule_run_finished,
    write_schedule_run_started,
};
use crate::schedule::{WakeAssembly, assemble_wake_text, describe_schedule};
use crate::state;
use crate::state::schedules::ScheduleState;

/// A firing prepared for the turn: everything synchronous is done and
/// the owned pieces are all the await needs.
struct PreparedFiring {
    executor: std::sync::Arc<dyn TurnExecutor>,
    target: String,
    request: crate::agent::TurnRequest,
    schedule_id: String,
}

/// One pass over every pending schedule event (legacy `pumpEvents`,
/// schedule lane only — this build produces schedule events alone, so
/// the source's lane caps and cross-lane routing have nothing to
/// route). Sequential: each event's turn settles before the next is
/// claimed, and `claim_event` stays idempotent if two passes ever
/// overlap.
pub async fn pump_once(host: &Host, data_dir: &Path) -> Result<(), String> {
    let shutdown = host.subscribe_shutdown();
    if *shutdown.borrow() {
        return Ok(());
    }
    let writer = Mutex::new(LedgerWriter::open(data_dir).map_err(|error| error.to_string())?);
    let schedules = state::schedules::load(data_dir).map_err(|error| error.to_string())?;
    // The source opens with `pendingEvents(1000)` — every pending
    // schedule-sourced row, oldest first (each per-schedule read comes
    // back newest-first, so it would have to reverse them; this read
    // already walks ascending).
    let pending = writer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .pending_schedule_events(1000)
        .map_err(|error| error.to_string())?;
    for event in pending {
        if *shutdown.borrow() {
            return Ok(());
        }
        // A second claim loses the race and skips — `claim_event` is
        // the single-pending gate.
        let claimed = {
            let writer = writer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            writer
                .claim_event(&event.id)
                .map_err(|error| error.to_string())?
        };
        let Some(event) = claimed else {
            continue;
        };
        let Some(schedule_id) = event
            .payload
            .get("scheduleId")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            // Every schedule event this build writes carries its
            // schedule id; anything else cannot name a schedule and
            // stays pending exactly like the source (its
            // `event:{id}` lane never reaches this handler).
            continue;
        };
        // The source captures the lane's stop generation before any
        // schedule lookup (L2326): a stop that lands after this point
        // refuses the firing with `Run cancelled before execution.`.
        let lane_key = format!("schedule:{schedule_id}");
        let stop_snapshot = host.stop_generation(&lane_key);
        let Some(schedule) = schedules
            .iter()
            .find(|candidate| candidate.id == schedule_id)
        else {
            // The source's `!schedule` arm: an unknown schedule
            // silently ignores the event (no refusal text).
            writer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .set_event_state(&event.id, "ignored", None)
                .map_err(|error| error.to_string())?;
            continue;
        };
        if !schedule.enabled {
            // The source's `!schedule.enabled` arm: stored, never
            // fired, ignored without a message.
            writer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .set_event_state(&event.id, "ignored", None)
                .map_err(|error| error.to_string())?;
            continue;
        }

        // Sync half 1: everything up to the turn (the guard lives in
        // this block and never crosses the await).
        let prepared = {
            let writer = writer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            prepare_firing(host, data_dir, &writer, &event, schedule, stop_snapshot)
        };
        let turn_result = match prepared {
            // The workspace is gone or setup refused: the state is
            // already final inside prepare (failed event, schedule
            // error) or the pump's catch marks it.
            Ok(None) => continue,
            Ok(Some(prepared)) => {
                let PreparedFiring {
                    executor,
                    target,
                    request,
                    schedule_id,
                } = prepared;
                // The source awaits `controller.sendMessage` — the
                // completion settles when this enqueued turn ends.
                let turn = executor
                    .clone()
                    .spawn_turn_for_event(target.clone(), request)
                    .await;
                let outcome = match turn {
                    Ok(result) => result,
                    Err(_closed) => Err("The turn ended before reporting.".to_owned()),
                };
                Some(((target, schedule_id), outcome))
            }
            Err(error) => {
                // The source's `.catch`: a closing host interrupts, a
                // running one fails the event visibly.
                let closing = *shutdown.borrow();
                let state = if closing { "interrupted" } else { "failed" };
                writer
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .set_event_state(&event.id, state, Some(&error))
                    .map_err(|err| err.to_string())?;
                continue;
            }
        };
        // Sync half 2: the outcome lands in runtime and event state.
        if let Some(((target, schedule_id), turn_result)) = turn_result {
            let writer = writer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            finalize_firing(
                &writer,
                data_dir,
                &event,
                &target,
                &schedule_id,
                turn_result,
            )?;
        }
    }
    Ok(())
}

/// Everything up to the awaited turn: workspace gate, wake text,
/// origin-or-background session, run-started, and the prepared
/// request. `Ok(None)` means the event is already final (the missing
/// workspace wrote both the schedule error and the failed state).
fn prepare_firing(
    host: &Host,
    data_dir: &Path,
    writer: &LedgerWriter,
    event: &LedgerEvent,
    schedule: &ScheduleState,
    stop_snapshot: u32,
) -> Result<Option<PreparedFiring>, String> {
    // The workspace is gone: fail visibly, mark the schedule, never
    // retry-loop — and return Ok, because the event state is already
    // final (the source returns before any throw path).
    let Some(workspace_root) = canonical_workspace(&schedule.workspace_root) else {
        let message = format!("Workspace is missing: {}", schedule.workspace_root);
        write_schedule_error(writer, &schedule.id, &message).map_err(|error| error.to_string())?;
        writer
            .set_event_state(&event.id, "failed", Some(&message))
            .map_err(|error| error.to_string())?;
        return Ok(None);
    };
    let facts = event.payload.get("facts").cloned().unwrap_or(Value::Null);
    let facts_record = facts.as_object();
    let scheduled_at_ms = facts_record
        .and_then(|record| record.get("scheduledAt"))
        .and_then(Value::as_str)
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|parsed| parsed.timestamp_millis());
    let now_ms = Utc::now().timestamp_millis();
    let runtime = read_schedule_runtime(&schedule.id, |key| writer.state_get(key).ok().flatten());
    let description = describe_schedule(
        schedule,
        now_ms,
        read_trigger_due(schedule, |key| writer.state_get(key).ok().flatten()),
    );
    let clock_shift = facts_record
        .and_then(|record| record.get("clockShift"))
        .is_some_and(|value| value == &Value::Bool(true));
    let text = assemble_wake_text(&WakeAssembly {
        schedule,
        schedule_human: description.human,
        run_number: runtime.run_count + 1,
        last_result: runtime.last_result,
        last_run_at_ms: runtime.last_run_at,
        scheduled_at_ms,
        now_ms,
        clock_shift,
        facts: facts.clone(),
    });

    let executor = host
        .executor()
        .ok_or_else(|| "The host has no turn executor.".to_owned())?;
    let origin = &schedule.origin_session_id;
    let (target, opening) = if !origin.is_empty() && executor.has_live_session(origin) {
        // The report appears where the schedule was asked for.
        (origin.clone(), text)
    } else {
        let background = persistent_session_id(writer, data_dir, schedule)?;
        let opening = if origin.is_empty() {
            text
        } else {
            format!(
                "Created in session {origin}, which no longer exists. Continuing in the \
                 schedule session; earlier runs remain readable with session tools.\n\n{text}"
            )
        };
        (background, opening)
    };

    // The source's isClosing throw, before any turn work: a closing
    // host fails the event and the pump marks it interrupted.
    if *host.subscribe_shutdown().borrow() {
        return Err("Host is shutting down.".to_owned());
    }
    // The source's generation guard (schedule-service L138): a stop
    // that landed after the pump captured this lane refuses the
    // firing before the run starts; the pump's catch marks it failed.
    if host.stop_generation(&format!("schedule:{}", schedule.id)) != stop_snapshot {
        return Err("Run cancelled before execution.".to_owned());
    }

    write_schedule_run_started(writer, &schedule.id, now_ms).map_err(|error| error.to_string())?;

    // The origin may have no record yet in tests; production sessions
    // exist because a client sent from them. The background session is
    // created on first use, like the source's createPrivateSession —
    // bound to the schedule's own workspace (the source's
    // `getWorkspace(configuredRoot)` feeds exactly this session).
    if state::record_path(data_dir, &target).is_none() {
        let mut record = crate::dispatch::session_write::create_draft(host, data_dir);
        record.meta.id = target.clone();
        record.meta.workspace_root = Some(workspace_root.clone());
        state::save_session(data_dir, &record, true).map_err(|error| error.to_string())?;
    }

    let request = crate::dispatch::prepare_send(
        host,
        data_dir,
        &target,
        opening,
        Vec::new(),
        UserEntrySource::Schedule,
    )?;
    Ok(Some(PreparedFiring {
        executor,
        target,
        request,
        schedule_id: schedule.id.clone(),
    }))
}

/// The awaited turn's outcome: durable runtime, then the event state
/// (delivered or failed with the turn's error).
fn finalize_firing(
    writer: &LedgerWriter,
    data_dir: &Path,
    event: &LedgerEvent,
    target: &str,
    schedule_id: &str,
    turn_result: Result<(), String>,
) -> Result<(), String> {
    let error = turn_result.err();
    let report = if error.is_none() {
        state::record_path(data_dir, target)
            .and_then(|path| state::read_session_record(&path, target))
            .map(|record| state::last_assistant_text(&record.entries))
            .filter(|text| !text.trim().is_empty())
    } else {
        None
    };
    let (result, event_state) = match &error {
        None => (crate::schedule::ScheduleRunResult::Completed, "delivered"),
        Some(_) => (crate::schedule::ScheduleRunResult::Failed, "failed"),
    };
    write_schedule_run_finished(
        writer,
        schedule_id,
        crate::schedule::ScheduleRunOutcome {
            result,
            error: error.as_deref(),
            report: report.as_deref(),
        },
    )
    .map_err(|err| err.to_string())?;
    writer
        .set_event_state(&event.id, event_state, error.as_deref())
        .map_err(|err| err.to_string())?;
    Ok(())
}

/// The schedule's persistent background session id — the source's
/// `getPersistentSession` state map: `session:{key}` in the ledger
/// state holds the UUID across runs; a first firing mints one and
/// stores it before the turn opens the record.
fn persistent_session_id(
    writer: &LedgerWriter,
    data_dir: &Path,
    schedule: &ScheduleState,
) -> Result<String, String> {
    let state_key = format!("session:schedule:{}", schedule.id);
    let saved = writer
        .state_get(&state_key)
        .map_err(|error| error.to_string())?;
    if let Some(session_id) = saved
        && let Some(path) = state::record_path(data_dir, &session_id)
        && state::read_session_record(&path, &session_id).is_some()
    {
        return Ok(session_id);
    }
    let session_id = uuid::Uuid::new_v4().to_string();
    writer
        .set_state(&state_key, &session_id)
        .map_err(|error| error.to_string())?;
    Ok(session_id)
}

/// `canonicalWorkspaceRoot` (workspace-origin.ts): resolve the path,
/// require a directory. Absent means the workspace is gone.
fn canonical_workspace(root: &str) -> Option<String> {
    let resolved = std::fs::canonicalize(root).ok()?;
    if !resolved.is_dir() {
        return None;
    }
    // Windows canonical paths carry the `\\?\` prefix the source's
    // realpathSync never produced; strip it so the root reads back the
    // way the schedule recorded it.
    let text = resolved.to_string_lossy();
    let stripped = text
        .strip_prefix(r"\\?\")
        .map(str::to_owned)
        .unwrap_or_else(|| text.into_owned());
    Some(stripped)
}
