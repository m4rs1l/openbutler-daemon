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
    if let Some(root) = request.payload.get("workspaceRoot") {
        let Value::String(path) = root else {
            return Err(
                "workspaceRoot must be an absolute path on this host (got a non-string). Omit workspaceRoot to use this host's default workspace."
                    .to_owned(),
            );
        };
        if !std::path::Path::new(path).is_absolute() {
            return Err(format!(
                "workspaceRoot must be an absolute path on this host (got {path:?}). Omit workspaceRoot to use this host's default workspace."
            ));
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
        return Err("isolatedSession must be a boolean.".to_owned());
    }
    let has_chat = request.payload.get("chatSessionId").is_some();
    let has_new = request.payload.get("newChatSession") == Some(&json!(true));
    if has_chat && has_new {
        return Err("Choose an existing chat or a new chat.".to_owned());
    }
    if has_chat
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
    // Every attachment names what it attaches to (L3365): only
    // the chat path exists in this build.
    let Some(chat_value) = request.payload.get("chatSessionId") else {
        return Err("Choose a chat or a session to attach.".to_owned());
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
