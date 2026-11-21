//! `schedule.manage`: the schedule actions over their own store.
//!
//! Source: `daemon-host.ts` `schedule.manage` → `schedule-service.ts`
//! `executeBackground` (read 2026-11-16, the runtime half re-read
//! 2026-11-18). This slice owns every action: capabilities, the CRUD
//! persisted in `schedules.json` (schedules left the settings file
//! entirely — owner ruling 2026-11-18), and the runtime half `run`,
//! `list`, `log`. The pump that consumes a queued `run` event is the
//! next slice.

use std::collections::BTreeSet;
use std::path::Path;

use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use serde_json::{Map, Value, json};

use super::Host;
use crate::dispatch::{envelope_failure, envelope_success};
use crate::ledger::{self, EventInput, LedgerWriter, StateReader};
use crate::schedule as engine;
use crate::shape::is_absolute_path;
use crate::state::schedules::{
    ScheduleState, ScheduleTrigger, load as load_schedules, parse_schedule_state,
    save as save_schedules,
};

use openbutler_protocol::payload::require_string;
use openbutler_protocol::{DaemonRequest, DaemonResponse};

/// The capability list, verbatim: adapter ids, descriptions, option
/// shapes — what a client shows before offering a trigger form.
const CAPABILITIES: [&str; 7] = [
    "manual",
    "interval",
    "at",
    "window",
    "daily",
    "filesystem",
    "memory_pressure",
];

/// The single dispatch point for `schedule.manage`.
pub(crate) fn handle_schedule_manage(
    host: &Host,
    data_dir: &Path,
    request: &DaemonRequest,
) -> DaemonResponse {
    let Some(input) = request.get("input").and_then(Value::as_object) else {
        return fail(request, "input must be an object.".to_owned());
    };
    let action = input_action(input);
    if action == "capabilities" {
        return success(request, json!({ "adapters": capabilities() }));
    }
    // The runtime half, in the source's order: list, log, then the
    // scheduleId-gated run — before the CRUD below.
    if action == "list" {
        return list_schedules(host, data_dir, request);
    }
    if action == "log" {
        return log_schedule(data_dir, request, input);
    }
    if action == "run" {
        return run_schedule(host, data_dir, request, input);
    }

    let mut schedules = match load_schedules(data_dir) {
        Ok(schedules) => schedules,
        Err(error) => return fail(request, error.to_string()),
    };

    match action.as_str() {
        "add" | "update" => match add_or_update(&mut schedules, input) {
            Ok(schedule) => {
                if let Err(error) = save_schedules(data_dir, &schedules) {
                    return fail(request, error.to_string());
                }
                reload_runner(host, &schedules);
                let schedule_id = schedule
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let runtime = runtime_status(host, &schedule_id);
                let mut body =
                    json!({ "status": status_word(&runtime), "schedule": schedule, "saved": true });
                attach_runtime(&mut body, runtime);
                success(request, body)
            }
            Err(error) => fail(request, error),
        },
        "remove" | "enable" | "disable" => {
            let schedule_id = match require_string(input.get("scheduleId"), "scheduleId", 64) {
                Ok(id) => id,
                Err(error) => return fail(request, error.to_string()),
            };
            let Some(index) = schedules
                .iter()
                .position(|candidate| candidate.id == schedule_id)
            else {
                return fail(request, "Schedule does not exist.".to_owned());
            };
            match action.as_str() {
                "remove" => {
                    schedules.remove(index);
                }
                "enable" => schedules[index].enabled = true,
                _ => schedules[index].enabled = false,
            }
            if let Err(error) = save_schedules(data_dir, &schedules) {
                return fail(request, error.to_string());
            }
            reload_runner(host, &schedules);
            let runtime = runtime_status(host, &schedule_id);
            let mut body = json!({ "status": status_word(&runtime), "scheduleId": schedule_id, "saved": true });
            attach_runtime(&mut body, runtime);
            success(request, body)
        }
        other => fail(request, format!("Unknown schedule action: {other}")),
    }
}

/// `String(input.action ?? '')` — a missing action is the empty string,
/// which falls through every branch below and names itself.
fn input_action(input: &Map<String, Value>) -> String {
    match input.get("action") {
        None => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

/// The seven adapters with their verbatim descriptions and option shapes.
fn capabilities() -> Vec<Value> {
    vec![
        json!({
            "adapter": "manual",
            "description": "Run only on an explicit request; no automatic observations.",
            "options": {}
        }),
        json!({
            "adapter": "interval",
            "description": "Run at a recurring fixed period. True periods only, never wall-clock times.",
            "options": { "everySeconds": "integer, 10-31536000" }
        }),
        json!({
            "adapter": "at",
            "description": "Run once at a server-local wall time.",
            "options": { "at": "ISO timestamp, server-local (an explicit offset is converted at creation)" }
        }),
        json!({
            "adapter": "window",
            "description": "Run once at the earliest server-local time in a window; a missed window runs late, never skipped.",
            "options": {
                "from": "server-local ISO timestamp",
                "to": "server-local ISO timestamp, later than from"
            }
        }),
        json!({
            "adapter": "daily",
            "description": "Run at a wall-clock time, optionally on selected weekdays. The never-drifting way to say \"every night at 02:00\".",
            "options": {
                "at": "\"HH:MM\", server-local",
                "days": "optional list of mon,tue,wed,thu,fri,sat,sun; absent means every day"
            }
        }),
        json!({
            "adapter": "filesystem",
            "description": "Observe metadata changes in an explicitly selected file or directory; file content is not collected.",
            "options": {
                "path": "absolute path",
                "recursive": "optional boolean, default true",
                "debounceSeconds": "optional integer, 1-60, default 5"
            }
        }),
        json!({
            "adapter": "memory_pressure",
            "description": "Observe a sustained free-memory threshold only when this schedule is explicitly added.",
            "options": {
                "usedPercent": "integer, 50-99",
                "sampleSeconds": "optional integer, 10-3600, default 30",
                "durationSeconds": "optional integer, 10-86400, default 60"
            }
        }),
    ]
    .into_iter()
    .zip(CAPABILITIES)
    .map(|(entry, adapter)| {
        assert_eq!(
            entry["adapter"], adapter,
            "capability order stays aligned with the contract"
        );
        entry
    })
    .collect()
}

/// add/update: parse, validate, canonicalise, insert or replace — the
/// source order, so the first failing rule is the message the client sees.
fn add_or_update(
    schedules: &mut Vec<ScheduleState>,
    input: &Map<String, Value>,
) -> Result<Value, String> {
    let Some(schedule_input) = input.get("schedule").and_then(Value::as_object) else {
        return Err("schedule must be an object.".to_owned());
    };
    let workspace_root = input.get("workspaceRoot").and_then(Value::as_str);
    let mut raw = schedule_input.clone();
    if raw.get("workspaceRoot").is_none() {
        let Some(root) = workspace_root else {
            return Err(
                "Choose a workspace with --workspace or set workspaceRoot in the schedule."
                    .to_owned(),
            );
        };
        raw.insert("workspaceRoot".to_owned(), Value::String(root.to_owned()));
    }

    let action_is_add = input.get("action").and_then(Value::as_str) == Some("add");
    let has_id = raw
        .get("id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty());
    if action_is_add && !has_id {
        let name = raw
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let taken: BTreeSet<String> = schedules
            .iter()
            .map(|candidate| candidate.id.clone())
            .collect();
        raw.insert(
            "id".to_owned(),
            Value::String(generate_schedule_id(&name, &taken)?),
        );
    }
    if raw.get("originSessionId").is_none() {
        let existing = if action_is_add {
            None
        } else {
            raw.get("id")
                .and_then(Value::as_str)
                .and_then(|id| schedules.iter().find(|c| c.id == id))
        };
        // No sender session on the wire in this build: the source falls
        // back to `senderSessionId ?? ''`, and there is no sender here.
        let origin = existing
            .map(|schedule| schedule.origin_session_id.clone())
            .unwrap_or_default();
        raw.insert("originSessionId".to_owned(), Value::String(origin));
    }

    let record: Map<String, Value> = raw;
    let mut schedule =
        parse_schedule_state(&Value::Object(record)).map_err(|error| error.to_string())?;
    schedule.trigger = normalize_schedule_trigger(schedule.trigger);
    validate_schedule_trigger(&schedule.trigger)?;
    schedule.workspace_root = crate::state::canonical_scope_root(&schedule.workspace_root)?;

    let index = schedules
        .iter()
        .position(|candidate| candidate.id == schedule.id);
    match (action_is_add, index) {
        (true, Some(_)) => return Err("A schedule with that id already exists.".to_owned()),
        (false, None) => return Err("Schedule does not exist.".to_owned()),
        (true, None) => schedules.push(schedule.clone()),
        (false, Some(at)) => schedules[at] = schedule.clone(),
    }
    serde_json::to_value(&schedule).map_err(|error| error.to_string())
}

/// `generateScheduleId`: readable slug plus four hex characters, never
/// colliding with a taken id.
fn generate_schedule_id(name: &str, taken: &BTreeSet<String>) -> Result<String, String> {
    let mut slug = String::new();
    let mut last_dash = true;
    for ch in name.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug.truncate(32);
    let slug = if slug.is_empty() { "schedule" } else { &slug };
    loop {
        let mut bytes = [0u8; 2];
        // An OS randomness failure propagates instead of inventing an id
        // from a weaker source.
        getrandom::fill(&mut bytes).map_err(|error| error.to_string())?;
        let id = format!("{slug}-{:04x}", u16::from_be_bytes(bytes));
        if !taken.contains(&id) {
            return Ok(id);
        }
    }
}

/// Normalise a submitted trigger to its stored form: an ISO value that
/// carries an explicit offset becomes a server-local `YYYY-MM-DDTHH:MM`,
/// so every later reader compares like with like.
fn normalize_schedule_trigger(mut trigger: ScheduleTrigger) -> ScheduleTrigger {
    if let Value::Object(options) = &mut trigger.options {
        for key in ["at", "from", "to"] {
            let Some(Value::String(text)) = options.get(key).cloned() else {
                continue;
            };
            if !has_offset(text.as_str()) {
                continue;
            }
            let Some(parsed) = parse_instant(text.as_str()) else {
                continue;
            };
            let local = parsed.with_timezone(&Local);
            options.insert(
                key.to_owned(),
                Value::String(local.format("%Y-%m-%dT%H:%M").to_string()),
            );
        }
    }
    trigger
}

/// The source regex `(?:Z|[+-]\d{2}:?\d{2})$`.
fn has_offset(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.last() == Some(&b'Z') {
        return true;
    }
    bytes.len() >= 6
        && matches!(bytes[bytes.len() - 6], b'+' | b'-')
        && bytes[bytes.len() - 5..].iter().all(u8::is_ascii_digit)
}

/// `Date.parse` for the formats a schedule actually carries: RFC 3339
/// with an offset, or a naive local datetime. Anything else is not a
/// timestamp here.
fn parse_instant(value: &str) -> Option<DateTime<Local>> {
    if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
        return Some(parsed.with_timezone(&Local));
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"] {
        if let Ok(parsed) = NaiveDateTime::parse_from_str(value, format) {
            return Local.from_local_datetime(&parsed).single();
        }
    }
    None
}

/// Adapter-specific trigger validation with the source's exact messages.
pub(crate) fn validate_schedule_trigger(trigger: &ScheduleTrigger) -> Result<(), String> {
    let options = match &trigger.options {
        Value::Object(options) => options.clone(),
        _ => {
            return Err(format!(
                "Unknown background trigger adapter: {}",
                trigger.adapter
            ));
        }
    };
    let allowed: &[&[&str]] = match trigger.adapter.as_str() {
        "manual" => &[&[]],
        "interval" => &[&["everySeconds"]],
        "at" => &[&["at"]],
        "window" => &[&["from", "to"]],
        "daily" => &[&["at", "days"]],
        "filesystem" => &[&["path", "recursive", "debounceSeconds"]],
        "memory_pressure" => &[&["usedPercent", "sampleSeconds", "durationSeconds"]],
        _ => {
            return Err(format!(
                "Unknown background trigger adapter: {}",
                trigger.adapter
            ));
        }
    };
    let fields = allowed[0];
    for key in options.keys() {
        if !fields.contains(&key.as_str()) {
            return Err(format!(
                "{}.{key} is not a trigger option.",
                trigger.adapter
            ));
        }
    }

    match trigger.adapter.as_str() {
        "interval" => {
            integer_in_range(&options, "everySeconds", 10, 31_536_000)?;
        }
        "at" => {
            if !options
                .get("at")
                .and_then(Value::as_str)
                .is_some_and(|value| parse_instant(value).is_some())
            {
                return Err(
                    "at must be a server-local ISO timestamp (an explicit offset is converted at creation)."
                        .to_owned(),
                );
            }
        }
        "window" => {
            let from = options.get("from").and_then(Value::as_str);
            let to = options.get("to").and_then(Value::as_str);
            let from_ms = from
                .and_then(parse_instant)
                .map(|value| value.timestamp_millis());
            let to_ms = to
                .and_then(parse_instant)
                .map(|value| value.timestamp_millis());
            if from_ms.is_none() {
                return Err("window.from must be a server-local ISO timestamp.".to_owned());
            }
            if to_ms.is_none() {
                return Err("window.to must be a server-local ISO timestamp.".to_owned());
            }
            if to_ms <= from_ms {
                return Err("window.to must be later than window.from.".to_owned());
            }
        }
        "daily" => {
            let at_ok = options
                .get("at")
                .and_then(Value::as_str)
                .is_some_and(is_hh_mm);
            if !at_ok {
                return Err("daily.at must be \"HH:MM\" in server-local wall time.".to_owned());
            }
            if let Some(days) = options.get("days") {
                let list = days.as_array().filter(|list| !list.is_empty());
                let weekdays = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
                let all_valid = list.is_some_and(|list| {
                    list.iter().all(|day| {
                        day.as_str()
                            .is_some_and(|day| weekdays.contains(&day.to_lowercase().as_str()))
                    })
                });
                if !all_valid {
                    return Err(
                        "daily.days must be a non-empty list of mon,tue,wed,thu,fri,sat,sun."
                            .to_owned(),
                    );
                }
            }
        }
        "filesystem" => {
            let path_ok = options
                .get("path")
                .and_then(Value::as_str)
                .is_some_and(is_absolute_path);
            if !path_ok {
                return Err("filesystem.path must be an absolute path.".to_owned());
            }
            if let Some(recursive) = options.get("recursive")
                && !recursive.is_boolean()
            {
                return Err("filesystem.recursive must be a boolean.".to_owned());
            }
            if options.contains_key("debounceSeconds") {
                integer_in_range(&options, "debounceSeconds", 1, 60)?;
            }
        }
        "memory_pressure" => {
            integer_in_range(&options, "usedPercent", 50, 99)?;
            if options.contains_key("sampleSeconds") {
                integer_in_range(&options, "sampleSeconds", 10, 3_600)?;
            }
            if options.contains_key("durationSeconds") {
                integer_in_range(&options, "durationSeconds", 10, 86_400)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// `requireInteger` with the source's single message shape.
fn integer_in_range(
    options: &Map<String, Value>,
    name: &str,
    min: u64,
    max: u64,
) -> Result<(), String> {
    let ok = options.get(name).is_some_and(|value| {
        value
            .as_u64()
            .is_some_and(|number| (min..=max).contains(&number))
    });
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{name} must be an integer between {min} and {max}."
        ))
    }
}

/// `^([01]\d|2[0-3]):[0-5]\d$`
fn is_hh_mm(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 5
        && bytes[2] == b':'
        && bytes.iter().filter(|byte| byte.is_ascii_digit()).count() == 4
        && (bytes[0] == b'0' || bytes[0] == b'1' || bytes[0] == b'2')
        && (bytes[0] != b'2' || bytes[1] <= b'3')
        && bytes[1].is_ascii_digit()
        && bytes[3] <= b'5'
        && bytes[4] <= b'9'
}

fn success(request: &DaemonRequest, result: Value) -> DaemonResponse {
    envelope_success(&request.id, result)
}

fn fail(request: &DaemonRequest, error: String) -> DaemonResponse {
    envelope_failure(&request.id, &error)
}

// ------------------------------------------------------- the runtime half

/// Reload the installed runner after a store change (the source awaits
/// `scheduler.reload` before reading the status back). No runner yet:
/// nothing to reload — the response then carries no `runtime`, which is
/// the source's JSON for `undefined`.
fn reload_runner(host: &Host, schedules: &[ScheduleState]) {
    let installed = host.scheduler();
    if let Some(runner) = installed.as_ref() {
        runner.reload(schedules.to_vec());
    }
}

/// The runner's status for one schedule, if the boot wired one.
fn runtime_status(host: &Host, schedule_id: &str) -> Option<engine::ScheduleStatus> {
    let installed = host.scheduler();
    installed
        .as_ref()?
        .list_statuses()
        .into_iter()
        .find(|entry| entry.id == schedule_id)
}

/// `status: runtime?.state === 'error' ? 'error' : 'completed'`.
fn status_word(runtime: &Option<engine::ScheduleStatus>) -> &'static str {
    match runtime.as_ref().map(|entry| entry.state) {
        Some(engine::TriggerState::Error) => "error",
        _ => "completed",
    }
}

/// The source's `runtime` property: present when the runner knows the
/// id, absent (undefined) when it does not.
fn attach_runtime(body: &mut Value, runtime: Option<engine::ScheduleStatus>) {
    if let Some(runtime) = runtime
        && let Ok(serialized) = serde_json::to_value(runtime)
    {
        body["runtime"] = serialized;
    }
}

/// `list`: every stored schedule with its adapter status, durable
/// runtime, and the one-line description.
fn list_schedules(host: &Host, data_dir: &Path, request: &DaemonRequest) -> DaemonResponse {
    let schedules = match load_schedules(data_dir) {
        Ok(schedules) => schedules,
        Err(error) => return fail(request, error.to_string()),
    };
    let reader = match StateReader::open(data_dir) {
        Ok(reader) => reader,
        Err(error) => return fail(request, error.to_string()),
    };
    let statuses: std::collections::HashMap<String, engine::ScheduleStatus> = {
        let installed = host.scheduler();
        installed
            .as_ref()
            .map(|runner| {
                runner
                    .list_statuses()
                    .into_iter()
                    .map(|entry| (entry.id.clone(), entry))
                    .collect()
            })
            .unwrap_or_default()
    };
    let now = chrono::Utc::now().timestamp_millis();
    let entries: Vec<Value> = schedules
        .iter()
        .map(|schedule| {
            let status = statuses.get(&schedule.id);
            let runtime = engine::read_schedule_runtime(&schedule.id, |key| reader.get(key));
            let description = engine::describe_schedule(
                schedule,
                now,
                engine::read_trigger_due(schedule, |key| reader.get(key)),
            );
            let mut entry = serde_json::to_value(schedule).unwrap_or(Value::Null);
            entry["adapterState"] = status
                .map(|entry| serde_json::to_value(entry.state).unwrap_or(Value::Null))
                .unwrap_or_else(|| Value::String("idle".to_owned()));
            if let Some(error) = status.and_then(|entry| entry.error.as_ref()) {
                entry["adapterError"] = Value::String(error.clone());
            }
            if let Ok(serialized) = serde_json::to_value(runtime) {
                entry["runtime"] = serialized;
            }
            if let Ok(serialized) = serde_json::to_value(description) {
                entry["schedule"] = serialized;
            }
            entry
        })
        .collect();
    success(request, json!({ "schedules": entries }))
}

/// `log`: the schedule's run history, newest first, with the payload's
/// `scheduledAt` surfaced when the facts carry it.
fn log_schedule(
    data_dir: &Path,
    request: &DaemonRequest,
    input: &Map<String, Value>,
) -> DaemonResponse {
    let schedule_id = match require_string(input.get("scheduleId"), "scheduleId", 64) {
        Ok(id) => id,
        Err(error) => return fail(request, error.to_string()),
    };
    let schedules = match load_schedules(data_dir) {
        Ok(schedules) => schedules,
        Err(error) => return fail(request, error.to_string()),
    };
    if !schedules
        .iter()
        .any(|candidate| candidate.id == schedule_id)
    {
        return fail(request, "Schedule does not exist.".to_owned());
    }
    let rows = match ledger::schedule_events(data_dir, &schedule_id, 50) {
        Ok(rows) => rows,
        Err(error) => return fail(request, error.to_string()),
    };
    let runs: Vec<Value> = rows
        .into_iter()
        .map(|event| {
            let mut run = json!({
                "eventId": event.id,
                "kind": event.kind,
                "status": event.status,
                "occurredAt": event.occurred_at,
                "receivedAt": event.received_at,
            });
            if let Some(scheduled_at) = event
                .payload
                .get("facts")
                .and_then(|facts| facts.get("scheduledAt"))
            {
                run["scheduledAt"] = scheduled_at.clone();
            }
            // The source's truthiness: an empty error string is omitted.
            if let Some(error) = event.error.filter(|text| !text.is_empty()) {
                run["error"] = Value::String(error);
            }
            run
        })
        .collect();
    success(request, json!({ "scheduleId": schedule_id, "runs": runs }))
}

/// `run`: queue one manual firing. The pump (next slice) consumes it;
/// this half only writes the event, exactly as `receiveEvent` did.
fn run_schedule(
    host: &Host,
    data_dir: &Path,
    request: &DaemonRequest,
    input: &Map<String, Value>,
) -> DaemonResponse {
    let schedule_id = match require_string(input.get("scheduleId"), "scheduleId", 64) {
        Ok(id) => id,
        Err(error) => return fail(request, error.to_string()),
    };
    let schedules = match load_schedules(data_dir) {
        Ok(schedules) => schedules,
        Err(error) => return fail(request, error.to_string()),
    };
    let Some(schedule) = schedules
        .iter()
        .find(|candidate| candidate.id == schedule_id)
    else {
        return fail(request, "Schedule does not exist.".to_owned());
    };
    if !schedule.enabled {
        return fail(request, "Enable the schedule before running it.".to_owned());
    }
    let writer = match LedgerWriter::open(data_dir) {
        Ok(writer) => writer,
        Err(error) => return fail(request, error.to_string()),
    };
    let event = EventInput {
        source: "schedule:manual".to_owned(),
        kind: "schedule.requested".to_owned(),
        dedupe_key: format!("schedule:{schedule_id}:manual:{}", uuid::Uuid::new_v4()),
        workspace_root: Some(schedule.workspace_root.clone()),
        occurred_at: None,
        payload: json!({
            "scheduleId": schedule_id,
            "facts": {
                "requestedAt": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            },
        }),
    };
    if let Err(error) = writer.insert_event(&event) {
        return fail(request, error.to_string());
    }
    // The queued event fires now — the pump is the only consumer
    // (legacy `receiveEvent` → `pumpEvents`).
    host.spawn_schedule_pump();
    success(
        request,
        json!({ "status": "queued", "scheduleId": schedule_id }),
    )
}

/// The lane key whose stop generation guards a schedule background
/// session — `session.stop` resolves a stopped session back to it the
/// way the source looks the controller up in `persistentSessions`.
pub(crate) fn persistent_lane_key(data_dir: &Path, session_id: &str) -> Option<String> {
    let schedules = crate::state::schedules::load(data_dir).ok()?;
    for schedule in schedules {
        let state_key = format!("session:schedule:{}", schedule.id);
        if let Ok(Some(mapped)) = crate::ledger::LedgerWriter::open(data_dir)
            .map(|writer| writer.state_get(&state_key))
            .map(|result| result.ok().flatten())
            && mapped == session_id
        {
            return Some(format!("schedule:{}", schedule.id));
        }
    }
    None
}

/// `stopNamedRun`'s pending-event cancel: without `keepQueue`, every
/// pending event of the lane goes `ignored` with the source's
/// sentence — a kept queue is left untouched.
pub(crate) fn cancel_pending_schedule_events(
    data_dir: &Path,
    lane_key: &str,
    keep_queue: bool,
) -> Result<(), String> {
    if keep_queue {
        return Ok(());
    }
    let schedule_id = lane_key.strip_prefix("schedule:").unwrap_or(lane_key);
    let writer = crate::ledger::LedgerWriter::open(data_dir).map_err(|e| e.to_string())?;
    let events = writer
        .schedule_events(schedule_id, 1000)
        .map_err(|e| e.to_string())?;
    for event in events {
        if event.status == "pending" {
            writer
                .set_event_state(
                    &event.id,
                    "ignored",
                    Some("Queued work was cancelled by the owner."),
                )
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
