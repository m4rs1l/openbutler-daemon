//! Action routing: the single dispatch point.
//!
//! `handle` validates every action against the envelope rules, routes it,
//! and answers as one protocol response — no other module constructs
//! responses. The shared record (`Host`, `DispatchState`) lives in
//! `host.rs`; per-domain actions that outgrew the router live in
//! `schedule.rs` and `session_send.rs`.

mod commands;
pub mod fanout;
mod host;
mod schedule;
pub mod session_send;
mod session_write;

pub use host::{DispatchState, Host};

/// Fixed sentences (verbatim): the custodian refusal on load and the miss
/// every read of an unopened or vanished chat reports.
const CHAT_NOT_FOUND: &str = "Chat not found.";
const CUSTODIAN_LOAD_ERROR: &str =
    "The machine custodian is the host's own conversation; it is not a chat to open.";

use std::path::Path;

use openbutler_protocol::entry::UserEntrySource;
use openbutler_protocol::runtime::LocalClientKind;
use openbutler_protocol::runtime::RuntimeViewData;
use openbutler_protocol::runtime::view::{ApprovalMode, ReasoningSupport};
use openbutler_protocol::session::{AttachResult, SessionView, read};
use openbutler_protocol::{
    DAEMON_PROTOCOL_VERSION, DaemonRequest, DaemonResponse, InlineAttachment, SessionEntriesRange,
    validate_request,
};
use serde_json::{Value, json};

use crate::agent::TurnRequest;
use crate::ledger;
use crate::state::{self, WorkspaceFilter};

/// The one dispatch point: validate the envelope, route the action, answer.
pub fn handle(
    host: &Host,
    data_dir: &Path,
    state: &mut DispatchState,
    request: &DaemonRequest,
) -> DaemonResponse {
    if let Err(error) = validate_request(request) {
        return failure(request, error.to_string());
    }
    // `session.command` carries a name, not an effect: the registry
    // resolves it to a request the cases below already implement, so a
    // name never grows a case of its own (legacy L3492: resolution
    // before the gate and the switch).
    let action = if request.action == "session.command" {
        match commands::daemon_command_action(request) {
            Ok(target) => target,
            Err(error) => return failure(request, error),
        }
    } else {
        request.action.as_str()
    };
    match action {
        "status" => success(
            request,
            serde_json::to_value(host.status()).unwrap_or_else(|_| json!({})),
        ),
        "session.list" => match state::list_sessions(data_dir, &WorkspaceFilter::Any) {
            Ok(sessions) => success(
                request,
                serde_json::to_value(sessions).unwrap_or_else(|_| json!([])),
            ),
            Err(error) => failure(request, error.to_string()),
        },
        // Disk-authoritative reads: the saved record is the whole truth in
        // this build — there is no live session to prefer.
        "session.load" => match read::load_session_id(&request.payload) {
            Err(error) => failure(request, error.to_string()),
            Ok(session_id) => {
                if session_id == state::CUSTODIAN_SESSION_ID {
                    return failure(request, CUSTODIAN_LOAD_ERROR.to_owned());
                }
                match load_view(data_dir, &session_id) {
                    Some(view) => {
                        state.current_session = Some(session_id);
                        success(
                            request,
                            serde_json::to_value(&view).unwrap_or_else(|_| json!({})),
                        )
                    }
                    // A failed load leaves the connection's chat untouched,
                    // as a failed chat switch leaves the current chat.
                    None => failure(request, CHAT_NOT_FOUND.to_owned()),
                }
            }
        },
        "attach" => {
            // Source: daemon-host.ts attach handler (L3312), chat path.
            // Isolated/schedule/worker/new-chat attachment paths need the
            // runtime and land with it (M2); without one of them this
            // request falls to the source's own refusal.
            let client_kind = match request.payload.get("clientKind") {
                None => LocalClientKind::Terminal,
                Some(value) if *value == json!("terminal") => LocalClientKind::Terminal,
                Some(value) if *value == json!("web") => LocalClientKind::Web,
                Some(_) => {
                    return failure(request, "clientKind must be terminal or web.".to_owned());
                }
            };
            // Only a web client may window its attach (L3316).
            let entries_window = match request.payload.get("entriesWindow") {
                _ if client_kind != LocalClientKind::Web => None,
                None | Some(Value::Null) => None,
                Some(value) => {
                    let window = value.as_i64().filter(|value| (1..=2000).contains(value));
                    match window {
                        Some(window) => Some(window),
                        None => {
                            return failure(
                                request,
                                "entriesWindow must be an integer between 1 and 2000.".to_owned(),
                            );
                        }
                    }
                }
            };
            if let Some(root) = request.payload.get("workspaceRoot") {
                let Value::String(path) = root else {
                    return failure(
                        request,
                        "workspaceRoot must be an absolute path on this host (got a non-string). Omit workspaceRoot to use this host's default workspace."
                            .to_owned(),
                    );
                };
                if !std::path::Path::new(path).is_absolute() {
                    return failure(
                        request,
                        format!(
                            "workspaceRoot must be an absolute path on this host (got {path:?}). Omit workspaceRoot to use this host's default workspace."
                        ),
                    );
                }
            }
            if request
                .payload
                .get("isolatedSession")
                .is_some_and(|value| *value != json!(true))
                || request
                    .payload
                    .get("newChatSession")
                    .is_some_and(|value| *value != json!(true))
            {
                return failure(request, "isolatedSession must be a boolean.".to_owned());
            }
            let has_chat = request.payload.get("chatSessionId").is_some();
            let has_new = request.payload.get("newChatSession") == Some(&json!(true));
            if has_chat && has_new {
                return failure(request, "Choose an existing chat or a new chat.".to_owned());
            }
            if has_chat
                && (request.payload.get("isolatedSession").is_some()
                    || request.payload.get("scheduleId").is_some()
                    || request.payload.get("workerSessionId").is_some()
                    || request.payload.get("runningWorkerOf").is_some())
            {
                return failure(
                    request,
                    "A chat attachment cannot also select a custodian, worker, or background session."
                        .to_owned(),
                );
            }
            // Every attachment names what it attaches to (L3365): only
            // the chat path exists in this build.
            let Some(chat_value) = request.payload.get("chatSessionId") else {
                return failure(request, "Choose a chat or a session to attach.".to_owned());
            };
            let chat_session_id = chat_value.as_str().unwrap_or_default().to_owned();
            if chat_session_id.is_empty() || chat_session_id.len() > 64 {
                return failure(
                    request,
                    "chatSessionId must be a non-empty string under 64 bytes.".to_owned(),
                );
            }
            let Some(view) = load_view(data_dir, &chat_session_id) else {
                return failure(request, CHAT_NOT_FOUND.to_owned());
            };

            // Adopt the chat: subscribe this connection's sink to its
            // fan-out, replacing any prior subscription (attachChat's
            // detachClient). A connection without a sink (the sync test
            // surface) answers but never receives events.
            if let Some(previous) = state.subscription.take() {
                host.fanout().unsubscribe(previous);
            }
            if let Some(sink) = state.sink.clone() {
                state.subscription = Some(host.fanout().subscribe(&chat_session_id, sink));
            }
            state.current_session = Some(chat_session_id.clone());
            state.client_kind = Some(match client_kind {
                LocalClientKind::Web => UserEntrySource::Web,
                LocalClientKind::Terminal => UserEntrySource::Terminal,
            });

            let (snapshot, runtime, workspace_root) =
                session_write::chat_snapshot(host, view, entries_window);
            let result = AttachResult {
                host_entry: std::env::current_exe()
                    .ok()
                    .map(|path| path.display().to_string()),
                host_build_id: Some(None), // source run, no build id
                session_attachments: Some(true),
                workspace_key: workspace_root.clone(),
                client_kind: Some(client_kind),
                snapshot: snapshot.clone(),
                runtime: runtime.clone(),
                saved_sessions: state::list_sessions(data_dir, &WorkspaceFilter::Any)
                    .unwrap_or_default(),
            };
            session_write::queue_attach_event(state, snapshot, runtime);
            success(
                request,
                serde_json::to_value(result).unwrap_or_else(|_| json!({})),
            )
        }
        "session.entries-range" => match read::entries_range(&request.payload) {
            Err(error) => failure(request, error.to_string()),
            Ok(range) => match state
                .current_session
                .as_deref()
                .and_then(|id| load_view(data_dir, id))
            {
                None => failure(request, CHAT_NOT_FOUND.to_owned()),
                Some(view) => {
                    let total = view.entries.len() as u64;
                    let (start, end) = range.resolve(total);
                    let page = SessionEntriesRange {
                        from: start as i64,
                        to: end as i64,
                        total: total as i64,
                        entries: view.entries[start as usize..end as usize].to_vec(),
                    };
                    success(
                        request,
                        serde_json::to_value(page).unwrap_or_else(|_| json!({})),
                    )
                }
            },
        },
        "session.find-attachment" => match read::find_attachment_id(&request.payload) {
            Err(error) => failure(request, error.to_string()),
            Ok(attachment_id) => match state
                .current_session
                .as_deref()
                .and_then(|id| load_view(data_dir, id))
            {
                None => failure(request, CHAT_NOT_FOUND.to_owned()),
                Some(view) => match read::find_inline_attachment(&view.entries, &attachment_id) {
                    Some(file) => success(
                        request,
                        serde_json::to_value(InlineAttachment {
                            path: file.path.clone(),
                            media_type: file.media_type.clone(),
                        })
                        .unwrap_or_else(|_| json!({})),
                    ),
                    // Present-but-not-inline and absent ids both answer an
                    // explicit null, never a failure.
                    None => success(request, serde_json::Value::Null),
                },
            },
        },
        "telegram.status" => success(
            request,
            json!({ "state": "disabled", "paired": false, "username": null, "error": null }),
        ),
        // No telegram service exists in this build, which is exactly the
        // source's `telegram?.getStatus() ?? disabled` and its
        // not-configured guard: setup, pairing and the test message
        // arrive with the telegram slice.
        "telegram.pair" | "telegram.test" => failure(
            request,
            "Telegram is not configured. Run `openbutler telegram setup` first.".to_owned(),
        ),
        "session.new" => match session_write::new_chat(host, data_dir, state, request) {
            Ok(()) => success(request, json!({ "accepted": true })),
            Err(error) => failure(request, error),
        },
        "session.select-project" => {
            match session_write::select_project(host, data_dir, state, request) {
                Ok(()) => success(request, json!({ "accepted": true })),
                Err(error) => failure(request, error),
            }
        }
        "session.flush" => match session_write::flush(data_dir, state) {
            Ok(()) => success(request, json!({ "saved": true })),
            Err(error) => failure(request, error),
        },
        "session.add-entry" => match session_write::add_entry(data_dir, state, request) {
            Ok(()) => success(request, json!({ "accepted": true })),
            Err(error) => failure(request, error),
        },
        "runtime.save-reasoning" => match session_write::save_reasoning(data_dir, state, request) {
            Ok(()) => success(request, json!({ "saved": true })),
            Err(error) => failure(request, error),
        },
        "runtime.save-show-reasoning" => {
            match session_write::save_show_reasoning(data_dir, state, request) {
                Ok(()) => success(request, json!({ "saved": true })),
                Err(error) => failure(request, error),
            }
        }
        "runtime.save-approval-mode" => {
            match session_write::save_approval_mode(data_dir, host, request) {
                Ok(()) => success(request, json!({ "saved": true })),
                Err(error) => failure(request, error),
            }
        }
        "session.send" => {
            let message = request.get("message").cloned().unwrap_or(Value::Null);
            let parsed = match session_send::parse_queued_message(&message) {
                Ok(parsed) => parsed,
                Err(error) => return failure(request, error),
            };
            let Some(session_id) = state.current_session.clone() else {
                return failure(request, CHAT_NOT_FOUND.to_owned());
            };
            let source = state.client_kind.unwrap_or(UserEntrySource::Terminal);
            let turn = match prepare_send(
                host,
                data_dir,
                &session_id,
                parsed.text,
                parsed.images,
                source,
            ) {
                Ok(turn) => turn,
                Err(error) => {
                    return failure(request, format!("Could not send the message: {error}"));
                }
            };
            let Some(executor) = host.executor() else {
                return failure(
                    request,
                    "Could not send the message: the host has no turn executor.".to_owned(),
                );
            };
            // The legacy contract: accept first, run in the background.
            executor.spawn_turn(session_id, turn);
            success(request, json!({ "accepted": true }))
        }
        "session.approve" => {
            // `parseApprovalDecision` + `requireString(approvalId, 64)`
            // (daemon-host L3525-3531), then the legacy fire-and-forget
            // acceptance — a stale approval id no-ops server side.
            let decision = match request.get("decision").and_then(Value::as_str) {
                Some("allow" | "always") => Some(true),
                Some("deny") => Some(false),
                _ => None,
            };
            let Some(approved) = decision else {
                return failure(
                    request,
                    "Approval decision must be allow, deny, or always.".to_owned(),
                );
            };
            let approval_id = match request.get("approvalId").and_then(Value::as_str) {
                Some(value) if !value.is_empty() && value.len() <= 64 => value.to_owned(),
                _ => {
                    return failure(
                        request,
                        "approvalId must be a non-empty string under 64 bytes.".to_owned(),
                    );
                }
            };
            let Some(session_id) = state.current_session.clone() else {
                return failure(request, CHAT_NOT_FOUND.to_owned());
            };
            let Some(executor) = host.executor() else {
                return failure(request, "The host has no turn executor.".to_owned());
            };
            let _ = executor.approve(&session_id, &approval_id, approved);
            success(request, json!({ "accepted": true }))
        }
        "session.stop" => {
            // Source (daemon-host L3552): fire-and-forget `stop` with
            // `keepQueue`/`holdQueue` as plain `=== true` checks — no
            // validation; `keepQueue` decides whether the queued sends
            // survive (the source's stop handler clears them otherwise).
            let Some(executor) = host.executor() else {
                return failure(request, "The host has no turn executor.".to_owned());
            };
            if let Some(session_id) = state.current_session.clone() {
                let keep_queue = request.get("keepQueue").and_then(Value::as_bool) == Some(true);
                let _ = executor.stop(&session_id, keep_queue);
            }
            success(request, json!({ "accepted": true }))
        }
        "session.compact" => {
            // Source (daemon-host L3514): `recovery` is validated when
            // present and the request is fire-and-forget — the
            // controller owns deferral (queued while a run is busy).
            let recovery = match request.get("recovery") {
                None => false,
                Some(Value::Bool(value)) => *value,
                Some(_) => return failure(request, "recovery must be a boolean.".to_owned()),
            };
            let Some(executor) = host.executor() else {
                return failure(request, "The host has no turn executor.".to_owned());
            };
            if let Some(session_id) = state.current_session.clone() {
                executor.compact(session_id, recovery);
            }
            success(request, json!({ "accepted": true }))
        }
        "session.queue" => {
            // Source (daemon-host L3544-3552): requireString(sessionId,
            // 64) + requireString(messageId, 64) + the closed
            // `queueAction` set, then manage through the seam — the
            // runner's refusals are this action's error text.
            let session_id = match request.get("sessionId").and_then(Value::as_str) {
                Some(value) if !value.is_empty() && value.len() <= 64 => value.to_owned(),
                _ => {
                    return failure(
                        request,
                        "sessionId must be a non-empty string under 64 bytes.".to_owned(),
                    );
                }
            };
            let message_id = match request.get("messageId").and_then(Value::as_str) {
                Some(value) if !value.is_empty() && value.len() <= 64 => value.to_owned(),
                _ => {
                    return failure(
                        request,
                        "messageId must be a non-empty string under 64 bytes.".to_owned(),
                    );
                }
            };
            let action = match request.get("queueAction").and_then(Value::as_str) {
                Some("steer") => crate::agent::QueueAction::Steer,
                Some("remove") => crate::agent::QueueAction::Remove,
                _ => return failure(request, "Invalid queue action.".to_owned()),
            };
            if state.current_session.as_deref() != Some(session_id.as_str()) {
                return failure(request, "The active session has changed.".to_owned());
            }
            let Some(executor) = host.executor() else {
                return failure(request, "The host has no turn executor.".to_owned());
            };
            match executor.manage_queue(&session_id, &message_id, action) {
                Ok(()) => success(request, json!({ "accepted": true })),
                Err(message) => failure(request, message),
            }
        }
        "session.answer" => {
            // Source (daemon-host L3532-3544): requireString(questionId,
            // 64) + the two answer shapes, then accept immediately.
            let question_id = match request.get("questionId").and_then(Value::as_str) {
                Some(value) if !value.is_empty() && value.len() <= 64 => value.to_owned(),
                _ => {
                    return failure(
                        request,
                        "questionId must be a non-empty string under 64 bytes.".to_owned(),
                    );
                }
            };
            let answer = match request.get("answer") {
                None | Some(Value::Null) => None,
                Some(Value::String(text)) => {
                    if text.len() > 10_000 {
                        return failure(
                            request,
                            "answer must be at most 10000 characters.".to_owned(),
                        );
                    }
                    Some(text.clone())
                }
                Some(_) => {
                    return failure(request, "answer must be a string or null.".to_owned());
                }
            };
            let Some(executor) = host.executor() else {
                return failure(request, "The host has no turn executor.".to_owned());
            };
            if let Some(session_id) = state.current_session.clone() {
                let _ = executor.answer(&session_id, &question_id, answer);
            }
            success(request, json!({ "accepted": true }))
        }
        "session.select-model" => {
            // Source (daemon-host L3559-3581): requireString(providerId,
            // 256) + requireString(modelId, 512), the pick waits at the
            // turn boundary, and the settings record is best effort — a
            // failed remember logs and still answers accepted.
            let provider_id = match request.get("providerId").and_then(Value::as_str) {
                Some(value) if !value.is_empty() && value.len() <= 256 => value.to_owned(),
                _ => {
                    return failure(
                        request,
                        "providerId must be a non-empty string under 256 bytes.".to_owned(),
                    );
                }
            };
            let model_id = match request.get("modelId").and_then(Value::as_str) {
                Some(value) if !value.is_empty() && value.len() <= 512 => value.to_owned(),
                _ => {
                    return failure(
                        request,
                        "modelId must be a non-empty string under 512 bytes.".to_owned(),
                    );
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
                        eprintln!(
                            "Could not record the picked model {provider_id}/{model_id}: {error}"
                        );
                    }
                }
                Err(message) => eprintln!(
                    "Could not record the picked model {provider_id}/{model_id}: {message}"
                ),
            }
            success(request, json!({ "accepted": true }))
        }
        "stop" => {
            host.request_shutdown();
            success(request, json!({ "stopping": true }))
        }
        "settings.reload" => match host.reload_settings(data_dir) {
            // The source handler answers a successful reload with `status()`.
            Ok(()) => success(
                request,
                serde_json::to_value(host.status()).unwrap_or_else(|_| json!({})),
            ),
            Err(error) => failure(request, error.to_string()),
        },
        "permissions.set-mode" => {
            // One message covers every rejected value, like the source
            // check: no typeof branch, four accepted spellings.
            let mode = request.get("mode").and_then(|value| value.as_str());
            let canonical = match mode {
                Some("workspace") => Some(crate::settings::ApprovalMode::Workspace),
                Some("auto-review") => Some(crate::settings::ApprovalMode::AutoReview),
                Some("full-access") => Some(crate::settings::ApprovalMode::FullAccess),
                // The original v1 value migrates to the folder-scoped
                // default on the way into the store.
                Some("ask") => Some(crate::settings::ApprovalMode::Workspace),
                _ => None,
            };
            let Some(canonical) = canonical else {
                return failure(
                    request,
                    "mode must be workspace, auto-review, or full-access.".to_owned(),
                );
            };
            // `applyApprovalMode(mode, persist)`: load → mutate → save,
            // then push to the live runtimes — this build has no runtimes
            // yet, so the live half is the host's settings copy.
            let mut loaded = match crate::settings::load_settings(data_dir) {
                Ok(loaded) => loaded,
                Err(error) => return failure(request, error.to_string()),
            };
            loaded.store.approval_mode = canonical;
            if let Err(error) = crate::settings::save_settings(data_dir, &mut loaded) {
                return failure(request, error.to_string());
            }
            if let Err(error) = host.reload_settings(data_dir) {
                return failure(request, error.to_string());
            }
            success(request, json!({ "mode": canonical.as_str() }))
        }
        "pair.create" => match host.pairing().issue(crate::auth::now_ms()) {
            Ok((code, expires_at)) => {
                success(request, json!({ "code": code, "expiresAt": expires_at }))
            }
            Err(error) => failure(request, error.to_string()),
        },
        "schedule.manage" => schedule::handle_schedule_manage(host, data_dir, request),
        "ledger.events" => match ledger::list_events(data_dir, request.get("limit")) {
            Ok(rows) => success(
                request,
                serde_json::to_value(rows).unwrap_or_else(|_| json!([])),
            ),
            Err(error) => failure(request, error.to_string()),
        },
        "ledger.actions" => match ledger::list_actions(data_dir, request.get("limit")) {
            Ok(rows) => success(
                request,
                serde_json::to_value(rows).unwrap_or_else(|_| json!([])),
            ),
            Err(error) => failure(request, error.to_string()),
        },
        "events.retry" => {
            let event_id = match openbutler_protocol::payload::require_string(
                request.get("eventId"),
                "eventId",
                64,
            ) {
                Ok(id) => id,
                Err(error) => return failure(request, error.to_string()),
            };
            match ledger::retry_event(data_dir, &event_id) {
                Ok(queued) => success(request, json!({ "queued": queued })),
                Err(error) => failure(request, error.to_string()),
            }
        }
        other => {
            let _ = other;
            failure(request, format!("Unknown IPC action: {}", request.action))
        }
    }
}

/// Success envelope at the current protocol version, echoing the request id.
/// Shared by the dispatcher and the admission gate: every response frame in
/// the daemon is built here.
pub(crate) fn envelope_success(id: &str, result: serde_json::Value) -> DaemonResponse {
    DaemonResponse::Success {
        version: DAEMON_PROTOCOL_VERSION,
        id: id.to_owned(),
        ok: true,
        result,
    }
}

/// Failure envelope at the current protocol version, echoing the request id.
pub(crate) fn envelope_failure(id: &str, error: &str) -> DaemonResponse {
    DaemonResponse::Failure {
        version: DAEMON_PROTOCOL_VERSION,
        id: id.to_owned(),
        ok: false,
        error: error.to_owned(),
    }
}

/// Success envelope for a dispatched request.
fn success(request: &DaemonRequest, result: serde_json::Value) -> DaemonResponse {
    envelope_success(&request.id, result)
}

/// Reads one saved record for the three disk-authoritative reads (location
/// rule + full parse); `None` covers an invalid id, a missing or oversized
/// or damaged file, or a mismatched identity — all "not found" to a client.
/// The bootstrap runtime view (documented in `agent-loop.md`): the model
/// facts come from the record this turn or attach opens; catalog-backed
/// fields stay empty rather than guessed. One builder for both callers —
/// `session.send` and `attach`.
fn bootstrap_runtime_view(
    meta: &openbutler_protocol::session::SessionMeta,
    workspace_root: Option<&str>,
    approval_mode: crate::settings::schema::ApprovalMode,
) -> RuntimeViewData {
    RuntimeViewData {
        supports_session_attachments: Some(true),
        supports_projects: Some(false),
        workspace_root: workspace_root.unwrap_or_default().to_owned(),
        model_name: meta.model_id.clone(),
        provider_name: None,
        provider_id: meta.provider_id.clone(),
        model_id: meta.model_id.clone(),
        context_window_tokens: None,
        custodian_context_limit_tokens: None,
        supports_vision: false,
        reasoning_variants: Vec::new(),
        reasoning_metadata_source: None,
        reasoning_support: ReasoningSupport::ModelUnsupported,
        reasoning_default: String::new(),
        reasoning_preference: String::new(),
        show_reasoning_default: false,
        reasoning_titles: None,
        approval_mode: Some(match approval_mode {
            crate::settings::schema::ApprovalMode::Workspace => ApprovalMode::Workspace,
            crate::settings::schema::ApprovalMode::AutoReview => ApprovalMode::AutoReview,
            crate::settings::schema::ApprovalMode::FullAccess => ApprovalMode::FullAccess,
        }),
    }
}

fn load_view(data_dir: &Path, session_id: &str) -> Option<SessionView> {
    let path = state::record_path(data_dir, session_id)?;
    state::read_session_view(&path, session_id)
}

/// Failure envelope for a dispatched request.
fn failure(request: &DaemonRequest, error: String) -> DaemonResponse {
    envelope_failure(&request.id, &error)
}

/// Resolves everything an accepted send needs: the chat's record, the
/// live settings (provider url, team, default workspace), the provider
/// secret, and the assembled system prompt.
/// The source's remember half (daemon-host L3574-3580): load fresh,
/// remember the pick, save through the write transaction — best effort;
/// the caller logs and still answers accepted.
fn record_pick_in_settings(
    data_dir: &Path,
    provider_id: &str,
    model_id: &str,
) -> Result<(), String> {
    let mut loaded = crate::settings::load_settings(data_dir).map_err(|error| error.to_string())?;
    crate::settings::remember_provider_model(&mut loaded.store, provider_id, model_id)?;
    crate::settings::save_settings(data_dir, &mut loaded).map_err(|error| error.to_string())
}

fn prepare_send(
    host: &Host,
    data_dir: &Path,
    session_id: &str,
    text: String,
    images: Vec<crate::agent::QueuedImage>,
    source: UserEntrySource,
) -> Result<TurnRequest, String> {
    let path = state::record_path(data_dir, session_id).ok_or_else(|| CHAT_NOT_FOUND.to_owned())?;
    let record =
        state::read_session_record(&path, session_id).ok_or_else(|| CHAT_NOT_FOUND.to_owned())?;
    let mut meta = record.meta;
    // The pick waits at the turn boundary: `prepare_send` reads it for
    // this request, and the runner takes it into the record when the
    // turn starts (legacy `selectModel` — never under a running request).
    if let Some((provider_id, model_id)) = host.peek_model_pick(session_id) {
        meta.provider_id = provider_id;
        meta.model_id = model_id;
    }
    let context = crate::agent::context::resolve_turn_context(host, data_dir, &meta)?;
    // The runtime view every published frame carries, until the full
    // `toRuntimeView` port lands with `session.attach`. Model facts come
    // from this turn's own choice; catalog-backed fields stay empty
    // rather than guessed (documented bootstrap in agent-loop.md).
    host.set_runtime_view(bootstrap_runtime_view(
        &meta,
        context.workspace_root.as_deref(),
        context.approval_mode,
    ));
    crate::agent::context::prepare_turn(
        data_dir,
        crate::agent::context::TurnInputs {
            meta: &meta,
            workspace_root: context.workspace_root,
            provider_url: context.provider_url,
            provider_api_key: context.provider_api_key,
            team: context.team,
            home: None,
            source,
            text,
            images,
            approval_mode: context.approval_mode,
            command_shell: context.command_shell,
            approval_timeout_seconds: context.approval_timeout_seconds,
        },
    )
}
