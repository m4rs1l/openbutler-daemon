//! Action routing: the single dispatch point.
//!
//! `handle` validates every action against the envelope rules, routes it,
//! and answers as one protocol response — no other module constructs
//! responses. The shared record (`Host`, `DispatchState`) lives in
//! `host.rs`; per-domain actions that outgrew the router live in
//! `attach.rs`, `session_control.rs`, `permissions.rs`, `schedule.rs`
//! and `session_send.rs`.

mod attach;
mod commands;
pub mod fanout;
mod host;
mod permissions;
pub(crate) mod schedule;
mod session_control;
pub mod session_send;
pub(crate) mod session_write;

pub use host::{DispatchState, Host};

/// Fixed sentences (verbatim): the custodian refusal on load and the miss
/// every read of an unopened or vanished chat reports.
const CHAT_NOT_FOUND: &str = "Chat not found.";
const CUSTODIAN_LOAD_ERROR: &str =
    "The machine custodian is the host's own conversation; it is not a chat to open.";

/// The pre-attach partition (daemon-host.ts L3179-3311): host-wide
/// actions that answer with no attachment and need no workspaceKey.
/// `attach` is its own branch (L3312); `session.list` is host-wide
/// only while the connection is unattached (L3303).
const PRE_ATTACH: &[&str] = &[
    "status",
    "pair.create",
    "stop",
    "settings.reload",
    "permissions.set-mode",
    "team.delivery",
    "team.send",
    "team.send-file",
    "team.manage",
    "schedule.manage",
    "telegram.status",
    "telegram.pair",
    "telegram.test",
    "ledger.events",
    "ledger.actions",
    "events.retry",
];

/// The view-only allow-list (daemon-host.ts L139-149): everything else
/// is refused on a read-only attachment before its handler runs.
const VIEW_ONLY_ACTIONS: &[&str] = &[
    "session.list",
    "session.entries-range",
    "session.find-attachment",
    "session.flush",
    "session.stop",
    "session.approve",
    "session.answer",
    "session.queue",
    "session.select-model",
    "session.add-entry",
];

/// The workspaceKey size gate (daemon-host.ts L151).
const MAX_WORKSPACE_PATH_BYTES: usize = 32 * 1024;

use std::path::Path;

use openbutler_protocol::entry::UserEntrySource;
use openbutler_protocol::runtime::RuntimeViewData;
use openbutler_protocol::runtime::view::{ApprovalMode, ReasoningSupport};
use openbutler_protocol::session::{SessionView, read};
use openbutler_protocol::{
    DAEMON_PROTOCOL_VERSION, DaemonRequest, DaemonResponse, InlineAttachment, SessionEntriesRange,
    validate_request,
};
use serde_json::json;

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
    // The partition (L3179-3311 vs L3478): everything outside the
    // pre-attach set proves this connection is bound to the workspace
    // it names — session.list included, host-wide only while
    // unattached (L3303).
    let needs_binding = match request.action.as_str() {
        "attach" => false,
        "session.list" => state.binding_key.is_some(),
        action => !PRE_ATTACH.contains(&action),
    };
    if needs_binding
        && let Err(error) = binding_gate(state, request)
    {
        return failure(request, error);
    }
    // `session.command` carries a name, not an effect: the registry
    // resolves it to a request the cases below already implement, so a
    // name never grows a case of its own. Resolution runs after the
    // binding gate (L3492 follows L3478): an unattached connection
    // never reaches the registry.
    let action = if request.action == "session.command" {
        match commands::daemon_command_action(request) {
            Ok(target) => target,
            Err(error) => return failure(request, error),
        }
    } else {
        request.action.as_str()
    };
    // The view-only gate on the resolved action (L3497-3499).
    if needs_binding && state.read_only && !VIEW_ONLY_ACTIONS.contains(&action) {
        return failure(request, "This subagent session is view-only.".to_owned());
    }
    match action {
        "status" => success(
            request,
            serde_json::to_value(host.status()).unwrap_or_else(|_| json!({})),
        ),
        // Unattached: the whole picker (L3303). An attached
        // connection lists only the workspace it is bound to (L3584
        // lists the attachment's controller — one workspace).
        "session.list" => {
            let filter = match state.binding_key.as_deref() {
                Some(key) => WorkspaceFilter::Exactly(key.to_owned()),
                None => WorkspaceFilter::Any,
            };
            match state::list_sessions(data_dir, &filter) {
                Ok(sessions) => success(
                    request,
                    serde_json::to_value(sessions).unwrap_or_else(|_| json!([])),
                ),
                Err(error) => failure(request, error.to_string()),
            }
        }
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
                        // attachChat rebinding (L3633: bindingKey =
                        // chat.workspace.root): the connection belongs
                        // to the loaded chat's workspace. The root
                        // resolves through the same authority the
                        // attach snapshot uses.
                        if let Some(root) = session_write::chat_workspace_root(host, &view) {
                            state.binding_key = Some(root);
                        }
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
        "attach" => match attach::attach_chat(host, data_dir, state, request) {
            Ok(payload) => success(request, payload),
            Err(error) => failure(request, error),
        },
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
        "session.send" => match session_control::send(host, data_dir, state, request) {
            Ok(payload) => success(request, payload),
            Err(error) => failure(request, error),
        },
        "session.approve" => match session_control::approve(host, state, request) {
            Ok(payload) => success(request, payload),
            Err(error) => failure(request, error),
        },
        "session.stop" => match session_control::stop(host, data_dir, state, request) {
            Ok(payload) => success(request, payload),
            Err(error) => failure(request, error),
        },
        "session.compact" => match session_control::compact(host, state, request) {
            Ok(payload) => success(request, payload),
            Err(error) => failure(request, error),
        },
        "session.queue" => match session_control::queue(host, state, request) {
            Ok(payload) => success(request, payload),
            Err(error) => failure(request, error),
        },
        "session.answer" => match session_control::answer(host, state, request) {
            Ok(payload) => success(request, payload),
            Err(error) => failure(request, error),
        },
        "session.select-model" => {
            match session_control::select_model(host, data_dir, state, request) {
                Ok(payload) => success(request, payload),
                Err(error) => failure(request, error),
            }
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
        "permissions.set-mode" => match permissions::set_mode(host, data_dir, request) {
            Ok(payload) => success(request, payload),
            Err(error) => failure(request, error),
        },
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

/// `requireString(value, name, max)`: the protocol validator with its
/// fixed message, answered as this router's error type.
pub(crate) fn require(
    value: Option<&serde_json::Value>,
    name: &'static str,
    max_bytes: usize,
) -> Result<String, String> {
    openbutler_protocol::payload::require_string(value, name, max_bytes)
        .map_err(|error| error.to_string())
}

/// The workspaceKey gate (daemon-host.ts L3478-3486): the string gate
/// runs first, then the key must equal this connection's binding — both
/// identity keys case-folded on Windows only (scopeMapKey,
/// workspace-origin.ts L35); POSIX compares the exact string.
fn binding_gate(state: &DispatchState, request: &DaemonRequest) -> Result<(), String> {
    let key = require(
        request.payload.get("workspaceKey"),
        "workspaceKey",
        MAX_WORKSPACE_PATH_BYTES,
    )?;
    match state.binding_key.as_deref() {
        Some(binding) if workspace_key_eq(binding, &key) => Ok(()),
        _ => Err("This IPC client is not attached to that workspace.".to_owned()),
    }
}

/// scopeMapKey parity: the identity key for workspace maps.
fn workspace_key_eq(binding: &str, key: &str) -> bool {
    if cfg!(windows) {
        binding.to_lowercase() == key.to_lowercase()
    } else {
        binding == key
    }
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
pub(crate) fn record_pick_in_settings(
    data_dir: &Path,
    provider_id: &str,
    model_id: &str,
) -> Result<(), String> {
    let mut loaded = crate::settings::load_settings(data_dir).map_err(|error| error.to_string())?;
    crate::settings::remember_provider_model(&mut loaded.store, provider_id, model_id)?;
    crate::settings::save_settings(data_dir, &mut loaded).map_err(|error| error.to_string())
}

pub(crate) fn prepare_send(
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
