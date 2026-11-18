//! The session-control actions — `send`, `approve`, `stop`, `compact`,
//! `queue`, `answer`, `select-model`: every arm that validates a payload
//! and reaches the turn executor through a seam. The router wraps each
//! result in the envelope; nothing here builds responses.

use openbutler_protocol::entry::UserEntrySource;
use serde_json::{Value, json};

use super::{CHAT_NOT_FOUND, DispatchState, Host, prepare_send, record_pick_in_settings, schedule};
use crate::agent::QueueAction;

pub fn send(
    host: &Host,
    data_dir: &std::path::Path,
    state: &mut DispatchState,
    request: &openbutler_protocol::DaemonRequest,
) -> Result<Value, String> {
    let message = request.get("message").cloned().unwrap_or(Value::Null);
    let parsed = super::session_send::parse_queued_message(&message)?;
    let Some(session_id) = state.current_session.clone() else {
        return Err(CHAT_NOT_FOUND.to_owned());
    };
    let source = state.client_kind.unwrap_or(UserEntrySource::Terminal);
    let turn = prepare_send(
        host,
        data_dir,
        &session_id,
        parsed.text,
        parsed.images,
        source,
    )
    .map_err(|error| format!("Could not send the message: {error}"))?;
    let Some(executor) = host.executor() else {
        return Err("Could not send the message: the host has no turn executor.".to_owned());
    };
    // The legacy contract: accept first, run in the background.
    executor.spawn_turn(session_id, turn);
    Ok(json!({ "accepted": true }))
}

pub fn approve(
    host: &Host,
    state: &mut DispatchState,
    request: &openbutler_protocol::DaemonRequest,
) -> Result<Value, String> {
    // `parseApprovalDecision` + `requireString(approvalId, 64)`
    // (daemon-host L3525-3531), then the legacy fire-and-forget
    // acceptance — a stale approval id no-ops server side.
    let decision = match request.get("decision").and_then(Value::as_str) {
        Some("allow" | "always") => Some(true),
        Some("deny") => Some(false),
        _ => None,
    };
    let Some(approved) = decision else {
        return Err("Approval decision must be allow, deny, or always.".to_owned());
    };
    let approval_id = match request.get("approvalId").and_then(Value::as_str) {
        Some(value) if !value.is_empty() && value.len() <= 64 => value.to_owned(),
        _ => {
            return Err("approvalId must be a non-empty string under 64 bytes.".to_owned());
        }
    };
    let Some(session_id) = state.current_session.clone() else {
        return Err(CHAT_NOT_FOUND.to_owned());
    };
    let Some(executor) = host.executor() else {
        return Err("The host has no turn executor.".to_owned());
    };
    let _ = executor.approve(&session_id, &approval_id, approved);
    Ok(json!({ "accepted": true }))
}

pub fn stop(
    host: &Host,
    data_dir: &std::path::Path,
    state: &mut DispatchState,
    request: &openbutler_protocol::DaemonRequest,
) -> Result<Value, String> {
    // Source (daemon-host L3552 → stopPersistentRun → stopNamedRun
    // L2271): fire-and-forget `stop` with `keepQueue`/`holdQueue` as
    // plain `=== true` checks — no validation. A persistent lane also
    // moves its stop generation (refusing a firing captured before
    // this stop) and, without `keepQueue`, its pending events go
    // `ignored` with the source's sentence.
    let Some(executor) = host.executor() else {
        return Err("The host has no turn executor.".to_owned());
    };
    if let Some(session_id) = state.current_session.clone() {
        let keep_queue = request.get("keepQueue").and_then(Value::as_bool) == Some(true);
        // The source resolves the controller back to its persistent
        // key; this build scans the schedule state map for the
        // session it minted.
        let lane_key = schedule::persistent_lane_key(data_dir, &session_id);
        if let Some(lane_key) = lane_key.as_deref() {
            if !keep_queue {
                host.bump_stop_generation(lane_key);
            }
            let _ = schedule::cancel_pending_schedule_events(data_dir, lane_key, keep_queue);
        }
        let _ = executor.stop(&session_id, keep_queue);
    }
    Ok(json!({ "accepted": true }))
}

pub fn compact(
    host: &Host,
    state: &mut DispatchState,
    request: &openbutler_protocol::DaemonRequest,
) -> Result<Value, String> {
    // Source (daemon-host L3514): `recovery` is validated when present
    // and the request is fire-and-forget — the controller owns
    // deferral (queued while a run is busy).
    let recovery = match request.get("recovery") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err("recovery must be a boolean.".to_owned()),
    };
    let Some(executor) = host.executor() else {
        return Err("The host has no turn executor.".to_owned());
    };
    if let Some(session_id) = state.current_session.clone() {
        executor.compact(session_id, recovery);
    }
    Ok(json!({ "accepted": true }))
}

pub fn queue(
    host: &Host,
    state: &mut DispatchState,
    request: &openbutler_protocol::DaemonRequest,
) -> Result<Value, String> {
    // Source (daemon-host L3544-3552): requireString(sessionId, 64) +
    // requireString(messageId, 64) + the closed `queueAction` set,
    // then manage through the seam — the runner's refusals are this
    // action's error text.
    let session_id = match request.get("sessionId").and_then(Value::as_str) {
        Some(value) if !value.is_empty() && value.len() <= 64 => value.to_owned(),
        _ => {
            return Err("sessionId must be a non-empty string under 64 bytes.".to_owned());
        }
    };
    let message_id = match request.get("messageId").and_then(Value::as_str) {
        Some(value) if !value.is_empty() && value.len() <= 64 => value.to_owned(),
        _ => {
            return Err("messageId must be a non-empty string under 64 bytes.".to_owned());
        }
    };
    let action = match request.get("queueAction").and_then(Value::as_str) {
        Some("steer") => QueueAction::Steer,
        Some("remove") => QueueAction::Remove,
        _ => return Err("Invalid queue action.".to_owned()),
    };
    if state.current_session.as_deref() != Some(session_id.as_str()) {
        return Err("The active session has changed.".to_owned());
    }
    let Some(executor) = host.executor() else {
        return Err("The host has no turn executor.".to_owned());
    };
    executor
        .manage_queue(&session_id, &message_id, action)
        .map(|()| json!({ "accepted": true }))
}

pub fn answer(
    host: &Host,
    state: &mut DispatchState,
    request: &openbutler_protocol::DaemonRequest,
) -> Result<Value, String> {
    // Source (daemon-host L3532-3544): requireString(questionId, 64) +
    // the two answer shapes, then accept immediately.
    let question_id = match request.get("questionId").and_then(Value::as_str) {
        Some(value) if !value.is_empty() && value.len() <= 64 => value.to_owned(),
        _ => {
            return Err("questionId must be a non-empty string under 64 bytes.".to_owned());
        }
    };
    let answer = match request.get("answer") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => {
            if text.len() > 10_000 {
                return Err("answer must be at most 10000 characters.".to_owned());
            }
            Some(text.clone())
        }
        Some(_) => {
            return Err("answer must be a string or null.".to_owned());
        }
    };
    let Some(executor) = host.executor() else {
        return Err("The host has no turn executor.".to_owned());
    };
    if let Some(session_id) = state.current_session.clone() {
        let _ = executor.answer(&session_id, &question_id, answer);
    }
    Ok(json!({ "accepted": true }))
}

pub fn select_model(
    host: &Host,
    data_dir: &std::path::Path,
    state: &mut DispatchState,
    request: &openbutler_protocol::DaemonRequest,
) -> Result<Value, String> {
    // Source (daemon-host L3559-3581): requireString(providerId, 256) +
    // requireString(modelId, 512), the pick waits at the turn
    // boundary, and the settings record is best effort — a failed
    // remember logs and still answers accepted.
    let provider_id = match request.get("providerId").and_then(Value::as_str) {
        Some(value) if !value.is_empty() && value.len() <= 256 => value.to_owned(),
        _ => {
            return Err("providerId must be a non-empty string under 256 bytes.".to_owned());
        }
    };
    let model_id = match request.get("modelId").and_then(Value::as_str) {
        Some(value) if !value.is_empty() && value.len() <= 512 => value.to_owned(),
        _ => {
            return Err("modelId must be a non-empty string under 512 bytes.".to_owned());
        }
    };
    if let Some(session_id) = state.current_session.clone() {
        host.queue_model_pick(&session_id, provider_id.clone(), model_id.clone());
    }
    match record_pick_in_settings(data_dir, &provider_id, &model_id) {
        Ok(()) => {
            // Refresh the host copy so the next read sees the
            // remembered list (the source reloads lazily).
            if let Err(error) = host.reload_settings(data_dir) {
                eprintln!("Could not record the picked model {provider_id}/{model_id}: {error}");
            }
        }
        Err(message) => {
            eprintln!("Could not record the picked model {provider_id}/{model_id}: {message}")
        }
    }
    Ok(json!({ "accepted": true }))
}
