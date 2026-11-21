//! The `attach` action — the chat path of `attachChat` (daemon-host.ts
//! L3312-3365), moved out of the router because it outgrew it. Returns
//! the response payload; the router wraps it in the envelope.

use openbutler_protocol::DaemonRequest;
use openbutler_protocol::entry::UserEntrySource;
use openbutler_protocol::runtime::LocalClientKind;
use openbutler_protocol::session::AttachResult;
use serde_json::{Value, json};

use super::{CHAT_NOT_FOUND, DispatchState, Host, load_view};
use crate::state::{self, WorkspaceFilter};

/// The interim the documented M2 gap answers with — deliberately not a
/// source sentence: the source always has the runtime behind these paths
/// (`createPrivateSession`, `getPersistentSession`).
const RUNTIME_INTERIM: &str = "The session runtime is not available in this build.";

/// The chat path of `attachChat` — the two local client kinds are the
/// protocol's own (`LocalClientKind`).
pub fn attach_chat(
    host: &Host,
    data_dir: &std::path::Path,
    state: &mut DispatchState,
    request: &DaemonRequest,
) -> Result<serde_json::Value, String> {
    // Source: daemon-host.ts attach handler (L3312), chat path.
    // Isolated/schedule/worker/new-chat attachment paths need the
    // runtime and land with it (M2); without one of them this
    // request falls to the source's own refusal.
    let client_kind = match request.payload.get("clientKind") {
        None => LocalClientKind::Terminal,
        Some(value) if *value == json!("terminal") => LocalClientKind::Terminal,
        Some(value) if *value == json!("web") => LocalClientKind::Web,
        Some(_) => {
            return Err("clientKind must be terminal or web.".to_owned());
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
                    return Err("entriesWindow must be an integer between 1 and 2000.".to_owned());
                }
            }
        }
    };
    // validateAttachWorkspaceRoot (L4200): the string gate first, then
    // the absolute-path sentence.
    if let Some(root) = request.payload.get("workspaceRoot") {
        let path = super::require(Some(root), "workspaceRoot", super::MAX_WORKSPACE_PATH_BYTES)?;
        if !std::path::Path::new(&path).is_absolute() {
            return Err(format!(
                "workspaceRoot must be an absolute path on this host (got {path:?}). Omit workspaceRoot to use this host's default workspace."
            ));
        }
    }
    // isolatedSession: any boolean or absent (L3327).
    if let Some(value) = request.payload.get("isolatedSession")
        && !value.is_boolean()
    {
        return Err("isolatedSession must be a boolean.".to_owned());
    }
    // newChatSession: exactly `true` or absent (L3346).
    if let Some(value) = request.payload.get("newChatSession")
        && value != &json!(true)
    {
        return Err("newChatSession must be true.".to_owned());
    }
    let has_chat = request.payload.get("chatSessionId").is_some();
    let has_new = request.payload.get("newChatSession") == Some(&json!(true));
    if has_chat && has_new {
        return Err("Choose an existing chat or a new chat.".to_owned());
    }
    // The chat branch carries its exclusions (L3335) and the projectId
    // shape (L3342): a new chat is the chat branch too (L3330).
    if (has_chat || has_new)
        && (request.payload.get("isolatedSession").is_some()
            || request.payload.get("scheduleId").is_some()
            || request.payload.get("workerSessionId").is_some()
            || request.payload.get("runningWorkerOf").is_some())
    {
        return Err(
            "A chat attachment cannot also select a custodian, worker, or background session."
                .to_owned(),
        );
    }
    if (has_chat || has_new)
        && request
            .payload
            .get("projectId")
            .is_some_and(|value| !value.is_null() && !value.is_string())
    {
        return Err("Invalid project ID.".to_owned());
    }
    // Non-chat attachments run the source's variant ladder (L3365-3440):
    // selection, custodian workspace, worker views, schedule lookup.
    if !has_chat && !has_new {
        return attach_non_chat(host, data_dir, request);
    }
    // Chat path: an existing chat by id. `newChatSession` opens through
    // `createSession` (L1377), which needs the M2 runtime.
    let Some(chat_value) = request.payload.get("chatSessionId") else {
        return Err(RUNTIME_INTERIM.to_owned());
    };
    let chat_session_id = super::require(Some(chat_value), "chatSessionId", 64)?;
    let Some(view) = load_view(data_dir, &chat_session_id) else {
        return Err(CHAT_NOT_FOUND.to_owned());
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
        super::session_write::chat_snapshot(host, view, entries_window);
    // The connection's binding (L3461: bindingKey = workspace.root):
    // the key the response hands out and every later action must echo.
    state.binding_key = workspace_root.clone();
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
        saved_sessions: state::list_sessions(data_dir, &WorkspaceFilter::Any).unwrap_or_default(),
    };
    super::session_write::queue_attach_event(state, snapshot, runtime);
    serde_json::to_value(result).map_err(|_| "The attach response could not be serialized.".into())
}

/// The non-chat ladder (daemon-host.ts L3365-3440): every attachment
/// names what it attaches to, then the custodian workspace, the
/// worker-view gates, the worker id ladders, and the schedule lookup.
/// Validation is the source's, in the source's order; the success paths
/// need the M2 session runtime and answer with `RUNTIME_INTERIM` once
/// every gate has passed. No attachment state is touched here — every
/// variant either refuses or stops at the runtime gap.
fn attach_non_chat(
    host: &Host,
    data_dir: &std::path::Path,
    request: &DaemonRequest,
) -> Result<Value, String> {
    // scopeRoot (L3326): the requested root, else the configured default,
    // else the launch directory — all through the one canonicalizer.
    let requested = match request.payload.get("workspaceRoot").and_then(Value::as_str) {
        Some(text) => text.to_owned(),
        None => {
            let configured = host
                .settings()
                .as_ref()
                .and_then(|loaded| loaded.store.default_workspace_root.clone());
            match configured {
                Some(text) => text,
                None => std::env::current_dir()
                    .map_err(|error| error.to_string())?
                    .to_string_lossy()
                    .into_owned(),
            }
        }
    };
    let scope_root = state::canonical_scope_root(&requested)?;

    // Every attachment names what it is attached to (L3365):
    // `isolatedSession !== true` selects nothing.
    let isolated = request.payload.get("isolatedSession") == Some(&json!(true));
    let schedule_id = request.payload.get("scheduleId");
    let worker_session = request.payload.get("workerSessionId");
    let running_of = request.payload.get("runningWorkerOf");
    let selected =
        isolated || schedule_id.is_some() || worker_session.is_some() || running_of.is_some();
    if !selected {
        return Err("Choose a chat or a session to attach.".to_owned());
    }
    // The custodian is the host's own conversation, reached through the
    // channels that drive it (L3371): the data directory never opens as a
    // workspace.
    let custodian_root = data_dir.join("custodian");
    if super::workspace_key_eq(&scope_root, &custodian_root.to_string_lossy()) {
        return Err(
            "The OpenButler custodian data directory cannot be selected as a workspace.".to_owned(),
        );
    }
    // The worker-view gates (L3376-3384).
    if worker_session.is_some() && running_of.is_some() {
        return Err("Choose one worker view mode.".to_owned());
    }
    let worker_view = worker_session.is_some() || running_of.is_some();
    if worker_view && (isolated || schedule_id.is_some()) {
        return Err("A worker view cannot also open an isolated or schedule session.".to_owned());
    }
    if schedule_id.is_some() && isolated {
        return Err("A schedule session cannot also be an isolated one-shot session.".to_owned());
    }
    // The worker id ladders (L3388-3404): no live worker exists without
    // the M2 runtime, so the settled path decides — and this build has no
    // worker ledger, which is exactly `readWorkerRecord` returning null.
    if let Some(worker_id) = worker_session {
        super::require(Some(worker_id), "workerSessionId", 128)?;
        return Err("That session is not a subagent worker.".to_owned());
    }
    if let Some(parent_id) = running_of {
        super::require(Some(parent_id), "runningWorkerOf", 128)?;
        // `runningWorkerId` (worker-service.ts L286) scans the workers
        // map; nothing in this build runs a worker of any parent.
        return Err("That subagent is not running right now.".to_owned());
    }
    // The isolated variant opens a private session (L3414) — M2.
    if isolated {
        return Err(RUNTIME_INTERIM.to_owned());
    }
    // The schedule lookup (L3420-3436): the id limit, then the record,
    // then the workspace compare — and a passing lookup opens the
    // persistent session (L3436), which is M2.
    if let Some(value) = schedule_id {
        let id = super::require(Some(value), "scheduleId", 64)?;
        let schedules = state::schedules::load(data_dir).map_err(|error| error.to_string())?;
        let belongs = schedules
            .iter()
            .find(|schedule| schedule.id == id)
            .map(|schedule| {
                state::canonical_scope_root(&schedule.workspace_root)
                    .map(|root| super::workspace_key_eq(&root, &scope_root))
            })
            .transpose()?
            .unwrap_or(false);
        if !belongs {
            return Err("The schedule does not belong to the requested workspace.".to_owned());
        }
        return Err(RUNTIME_INTERIM.to_owned());
    }
    // The selection gate guarantees one of the four names — this is the
    // source answer for anything else (unreachable, defensively).
    Err("Choose a chat or a session to attach.".to_owned())
}
