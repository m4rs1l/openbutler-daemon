//! The disk-side chat mutations: `session.new`, `session.flush`,
//! `session.add-entry` and the three `runtime.save-*` preference arms.
//!
//! Source: legacy `daemon-host.ts` L3643-3702, `parseSessionEntry`
//! (L4127), `agent-session.ts` `addEntry` / `flushSave` /
//! `setReasoningPreference` (read 2026-11-18). One adaptation: this
//! build has no deferred save queue — every mutation writes here and
//! now, so failures surface at the arm instead of `sessionSaveError`.

use std::path::Path;

use openbutler_protocol::entry::{NoticeTone, SessionEntry};
use openbutler_protocol::session::SessionMeta;
use serde_json::{Map, Value};

use super::{CHAT_NOT_FOUND, DispatchState, Host};
use crate::settings::schema::ApprovalMode;
use crate::settings::{load_settings, save_settings};
use crate::state::{self, SessionRecord};

/// The entry parser (`parseSessionEntry`, L4127) — the whitelist is the
/// contract: fields the source does not copy never enter the record, and
/// an unknown kind is refused with the source's sentence.
pub fn parse_session_entry(value: &Value) -> Result<SessionEntry, String> {
    let Some(entry) = value.as_object() else {
        return Err("Session entry is invalid.".to_owned());
    };
    let kind = entry
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| "Session entry is invalid.".to_owned())?;
    match kind {
        "tool" => parse_tool(entry),
        "user" => parse_user(entry),
        "notice" => {
            let text = entry
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| "Session entry kind is not supported.".to_owned())?;
            let detail = entry
                .get("detail")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(|value| slice_utf16(value, 2000));
            let tone = match entry.get("tone").and_then(Value::as_str) {
                Some("warn") => Some(NoticeTone::Warn),
                Some("error") => Some(NoticeTone::Error),
                _ => None,
            };
            Ok(SessionEntry::Notice {
                meta: Default::default(),
                text: text.to_owned(),
                detail,
                tone,
                cause: None,
            })
        }
        "assistant" | "reasoning" => {
            let text = entry
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| "Session entry kind is not supported.".to_owned())?;
            // The typed union has no `detail` field on these two kinds
            // (`session-store.ts` L74/86) — unlike the source's untyped
            // spread, it is dropped here.
            if kind == "assistant" {
                Ok(SessionEntry::Assistant {
                    meta: Default::default(),
                    text: text.to_owned(),
                })
            } else {
                Ok(SessionEntry::Reasoning {
                    meta: Default::default(),
                    text: text.to_owned(),
                    duration_ms: None,
                })
            }
        }
        _ => Err("Session entry kind is not supported.".to_owned()),
    }
}

fn parse_tool(entry: &Map<String, Value>) -> Result<SessionEntry, String> {
    let input_is_record = entry.get("input").is_some_and(Value::is_object);
    let command_is_string = entry.get("command").is_some_and(Value::is_string);
    let tool_is_string = entry.get("tool").is_some_and(Value::is_string);
    let shape_ok = command_is_string || (input_is_record && tool_is_string);
    let bad = !shape_ok
        || (entry.contains_key("input") && !input_is_record)
        || !entry.get("result").is_some_and(Value::is_string)
        || (entry.contains_key("running") && !entry.get("running").is_some_and(Value::is_boolean))
        || (entry.contains_key("preparing")
            && !entry.get("preparing").is_some_and(Value::is_boolean))
        || (entry.contains_key("tool") && !tool_is_string)
        || (entry.contains_key("toolTitle")
            && !entry.get("toolTitle").is_some_and(Value::is_string))
        || (entry.contains_key("session") && !entry.get("session").is_some_and(Value::is_string));
    if bad {
        return Err("Tool session entry is invalid.".to_owned());
    }
    let input = input_is_record.then(|| {
        entry
            .get("input")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    });
    let command = (!input_is_record).then(|| {
        entry
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    });
    Ok(SessionEntry::Tool {
        meta: Default::default(),
        command,
        input,
        result: entry["result"].as_str().unwrap_or_default().to_owned(),
        running: entry
            .get("running")
            .and_then(Value::as_bool)
            .filter(|value| *value)
            .map(|_| true),
        preparing: entry
            .get("preparing")
            .and_then(Value::as_bool)
            .filter(|value| *value)
            .map(|_| true),
        tool: entry.get("tool").and_then(Value::as_str).map(str::to_owned),
        tool_title: entry
            .get("toolTitle")
            .and_then(Value::as_str)
            .map(str::to_owned),
        session: entry
            .get("session")
            .and_then(Value::as_str)
            .map(str::to_owned),
        duration_ms: None,
        outcome: None,
    })
}

fn parse_user(entry: &Map<String, Value>) -> Result<SessionEntry, String> {
    let text = entry.get("text").and_then(Value::as_str);
    let workspace_root = entry.get("workspaceRoot");
    let image_count = entry.get("imageCount");
    let workspace_ok = workspace_root.is_none_or(Value::is_string);
    let count_ok = image_count.is_none_or(|value| {
        value
            .as_i64()
            .is_some_and(|count| (1..=i64::from(u32::MAX)).contains(&count))
    });
    let (Some(text), true, true) = (text, workspace_ok, count_ok) else {
        return Err("User session entry is invalid.".to_owned());
    };
    Ok(SessionEntry::User {
        meta: Default::default(),
        text: text.to_owned(),
        workspace_root: workspace_root.and_then(Value::as_str).map(str::to_owned),
        image_count: image_count
            .and_then(Value::as_u64)
            .map(|count| count as u32),
        attachments: None,
        source: None,
        event_id: None,
    })
}

/// JS `slice(0, 2000)` semantics — UTF-16 code units, not bytes/chars.
fn slice_utf16(text: &str, units: usize) -> String {
    let mut out = String::new();
    let mut taken = 0;
    for ch in text.chars() {
        let width = ch.len_utf16();
        if taken + width > units {
            break;
        }
        taken += width;
        out.push(ch);
    }
    out
}

/// The shared timestamp authority (`ledger::now_iso`) — this module
/// joins it rather than keeping a second copy.
use crate::ledger::now_iso;

/// The connection's current chat record — `None` on disk reads back as
/// the router's fixed miss sentence.
fn current_record(data_dir: &Path, state: &DispatchState) -> Result<SessionRecord, String> {
    let id = state
        .current_session
        .as_deref()
        .ok_or_else(|| CHAT_NOT_FOUND.to_owned())?;
    let path = state::record_path(data_dir, id).ok_or_else(|| CHAT_NOT_FOUND.to_owned())?;
    state::read_session_record(&path, id).ok_or_else(|| CHAT_NOT_FOUND.to_owned())
}

/// `createSession` (session-store.ts L254): the fresh draft — hidden from
/// the list until it carries a user message, exactly like the source's
/// unsaved controller from the reader's side.
pub(crate) fn create_draft(host: &Host) -> SessionRecord {
    let (provider_id, model_id, workspace_root) = host
        .settings()
        .as_ref()
        .map(|loaded| {
            (
                loaded.store.default_provider.clone(),
                loaded.store.default_model.clone(),
                loaded.store.default_workspace_root.clone(),
            )
        })
        .unwrap_or_else(|| (String::new(), String::new(), None));
    let now = now_iso();
    SessionRecord {
        meta: SessionMeta {
            project: None,
            version: 1,
            id: uuid::Uuid::new_v4().to_string(),
            title: "New session".to_owned(),
            created_at: now.clone(),
            updated_at: now.clone(),
            last_activity_at: Some(now.clone()),
            provider_id,
            model_id,
            reasoning: None,
            show_reasoning: None,
            reasoning_variants: None,
            supports_vision: None,
            reasoning_support: None,
            reasoning_metadata_source: None,
            reasoning_titles: None,
            workspace_root,
            context_tokens: None,
            context_estimate_tokens: None,
            context_window_tokens: None,
            key_positions: Some(Map::new()),
            has_user_message: None,
            automatic_wake_held: None,
            preview: None,
            first_user_message: None,
            last_assistant_message: None,
        },
        entries: Vec::new(),
        conversation: Vec::new(),
        conversation_archive: None,
    }
}

/// `session.new` — busy gate, project id check, draft on disk, the
/// connection points at it, and the attach snapshot rides behind the
/// answer (the source's `attachChat`, L1459).
pub fn new_chat(
    host: &Host,
    data_dir: &Path,
    state: &mut DispatchState,
    request: &openbutler_protocol::DaemonRequest,
) -> Result<(), String> {
    if let Some(executor) = host.executor()
        && state
            .current_session
            .as_deref()
            .is_some_and(|id| executor.is_busy(id))
    {
        return Err("Wait for the current run before creating a new chat.".to_owned());
    }
    if let Some(project_id) = request.get("projectId")
        && !project_id.is_null()
        && !project_id.is_string()
    {
        return Err("Invalid project ID.".to_owned());
    }
    let draft = create_draft(host);
    let id = draft.meta.id.clone();
    state::save_session(data_dir, &draft, true).map_err(|error| error.to_string())?;
    state.current_session = Some(id.clone());
    // The attach snapshot event for the fresh chat (shared builder).
    let path = state::record_path(data_dir, &id).ok_or_else(|| CHAT_NOT_FOUND.to_owned())?;
    let view = state::read_session_view(&path, &id).ok_or_else(|| CHAT_NOT_FOUND.to_owned())?;
    let (snapshot, runtime, _workspace_root) = chat_snapshot(host, view, None);
    queue_attach_event(state, snapshot, runtime);
    Ok(())
}

/// `session.flush` — there is no deferred write to force in this build
/// (every arm writes synchronously); the arm reports the open chat's
/// health: an unreadable or vanished record surfaces the fixed miss.
pub fn flush(data_dir: &Path, state: &DispatchState) -> Result<(), String> {
    current_record(data_dir, state).map(|_| ())
}

/// `session.add-entry` — parse, view-only gate (notices stay
/// postable), stamp, append, persist immediately.
pub fn add_entry(
    data_dir: &Path,
    state: &mut DispatchState,
    request: &openbutler_protocol::DaemonRequest,
) -> Result<(), String> {
    let raw = request
        .get("entry")
        .ok_or_else(|| "Session entry is invalid.".to_owned())?;
    let entry = parse_session_entry(raw)?;
    if state.read_only && !matches!(entry, SessionEntry::Notice { .. }) {
        return Err("This subagent session is view-only.".to_owned());
    }
    let mut record = current_record(data_dir, state)?;
    let mut entry = entry;
    let stamp = entry.meta_mut();
    if stamp.at.is_none() {
        stamp.at = Some(chrono::Utc::now().timestamp_millis());
    }
    let is_user = matches!(entry, SessionEntry::User { .. });
    record.entries.push(entry);
    if is_user {
        record.meta.last_activity_at = Some(now_iso());
    }
    state::save_session(data_dir, &record, true)
        .map_err(|error| error.to_string())
        .map(|_| ())
}

fn require_variant(
    request: &openbutler_protocol::DaemonRequest,
    name: &str,
    max_bytes: usize,
) -> Result<String, String> {
    let message = format!("{name} must be a non-empty string under {max_bytes} bytes.");
    match request.get(name) {
        Some(value) => match value.as_str() {
            Some(text) if !text.is_empty() && text.len() <= max_bytes => Ok(text.to_owned()),
            _ => Err(message),
        },
        None => Err(message),
    }
}

/// `runtime.save-reasoning` — session-scoped by design; a repeat value
/// writes nothing (the source's early return keeps `updatedAt` put).
pub fn save_reasoning(
    data_dir: &Path,
    state: &DispatchState,
    request: &openbutler_protocol::DaemonRequest,
) -> Result<(), String> {
    let variant = require_variant(request, "variantId", 256)?;
    let mut record = current_record(data_dir, state)?;
    if record.meta.reasoning.as_deref() == Some(variant.as_str()) {
        return Ok(());
    }
    record.meta.reasoning = Some(variant);
    state::save_session(data_dir, &record, true)
        .map_err(|error| error.to_string())
        .map(|_| ())
}

/// `runtime.save-show-reasoning` — the same session-scoped write for the
/// visibility toggle.
pub fn save_show_reasoning(
    data_dir: &Path,
    state: &DispatchState,
    request: &openbutler_protocol::DaemonRequest,
) -> Result<(), String> {
    let Some(Value::Bool(visible)) = request.get("visible") else {
        return Err("visible must be a boolean.".to_owned());
    };
    let visible = *visible;
    let mut record = current_record(data_dir, state)?;
    if record.meta.show_reasoning == Some(visible) {
        return Ok(());
    }
    record.meta.show_reasoning = Some(visible);
    state::save_session(data_dir, &record, true)
        .map_err(|error| error.to_string())
        .map(|_| ())
}

/// `runtime.save-approval-mode` — machine-wide: load, set, save, and
/// refresh the host's live copy (`applyApprovalMode(mode, true)`).
pub fn save_approval_mode(
    data_dir: &Path,
    host: &Host,
    request: &openbutler_protocol::DaemonRequest,
) -> Result<(), String> {
    let mode = match request.get("mode").and_then(Value::as_str) {
        Some("workspace") => ApprovalMode::Workspace,
        Some("auto-review") => ApprovalMode::AutoReview,
        Some("full-access") => ApprovalMode::FullAccess,
        _ => {
            return Err("mode must be workspace, auto-review, or full-access.".to_owned());
        }
    };
    let mut loaded = load_settings(data_dir)
        .map_err(|error| format!("Could not save the approval mode: {}", error.0))?;
    loaded.store.approval_mode = mode;
    save_settings(data_dir, &mut loaded).map_err(|error| error.0)?;
    host.reload_settings(data_dir).map_err(|error| error.0)?;
    Ok(())
}

/// The workspace a chat opens in: the record's own root, the configured
/// default, then this daemon's working directory — the source's
/// `resolveInteractiveScope({cwd: process.cwd()})` fallback chain. One
/// authority: the attach snapshot, the attach binding and the
/// `session.load` rebind all resolve through here.
pub(super) fn chat_workspace_root(
    host: &Host,
    view: &openbutler_protocol::SessionView,
) -> Option<String> {
    view.meta
        .workspace_root
        .clone()
        .or_else(|| {
            host.settings()
                .as_ref()
                .and_then(|loaded| loaded.store.default_workspace_root.clone())
        })
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|path| path.display().to_string())
        })
}

/// The attach snapshot + runtime: workspace resolution, the tail window
/// (`windowedSnapshotData`, L3824) and a settled run state — the one
/// builder `attach` and `session.new` share.
pub(super) fn chat_snapshot(
    host: &Host,
    view: openbutler_protocol::SessionView,
    entries_window: Option<i64>,
) -> (
    openbutler_protocol::session::SessionSnapshotData,
    openbutler_protocol::runtime::RuntimeViewData,
    Option<String>,
) {
    let approval_mode = host
        .settings()
        .as_ref()
        .map(|loaded| loaded.store.approval_mode)
        .unwrap_or(ApprovalMode::Workspace);
    let workspace_root = chat_workspace_root(host, &view);
    let runtime = host.runtime_view().unwrap_or_else(|| {
        super::bootstrap_runtime_view(&view.meta, workspace_root.as_deref(), approval_mode)
    });

    let total = view.entries.len() as i64;
    let from = entries_window
        .map(|window| (total - window).max(0))
        .unwrap_or(0);
    let mut windowed = view;
    if entries_window.is_some() {
        let tail = windowed.entries.split_off(from as usize);
        windowed.entries = tail;
    }
    let snapshot = openbutler_protocol::session::SessionSnapshotData {
        session: windowed,
        run: openbutler_protocol::session::RunState {
            busy: false,
            running_turn_id: None,
            pending_approval: None,
            pending_approval_id: None,
            pending_question: None,
            queued_message_count: 0,
            first_queued_message: None,
            queued_messages: Vec::new(),
            session_save_error: None,
            streaming_assistant_index: None,
        },
        entries_from: entries_window.map(|_| from),
        total_entries: entries_window.map(|_| total),
    };
    (snapshot, runtime, workspace_root)
}

/// The attach snapshot event that rides behind the answer (L1459):
/// queued as a pending frame, flushed by the connection after the
/// response is written.
pub(super) fn queue_attach_event(
    state: &mut DispatchState,
    snapshot: openbutler_protocol::session::SessionSnapshotData,
    runtime: openbutler_protocol::runtime::RuntimeViewData,
) {
    if state.sink.is_some() {
        let event = openbutler_protocol::DaemonEvent::Snapshot {
            version: openbutler_protocol::DAEMON_PROTOCOL_VERSION,
            snapshot,
            runtime,
        };
        if let Ok(mut bytes) = serde_json::to_vec(&event) {
            bytes.push(b'\n');
            state.pending_events.push(bytes);
        }
    }
}

/// `session.select-project` (`daemon-host.ts` L3636 + `agent-session.ts`
/// `selectProject` L927): bind a chat that has not started to a project.
///
/// Adaptations, both documented: the source keeps the binding in memory
/// until the first message, this build persists it here (a draft stays
/// list-invisible either way, so the reader sees the same chat); the
/// source's queued-turn clause has no equivalent without a runtime and
/// arrives with it. The `!attachment.interactive` gate has no wire shape
/// yet either - every chat path in this build is interactive until
/// worker attachments arrive.
pub fn select_project(
    host: &Host,
    data_dir: &Path,
    state: &mut DispatchState,
    request: &openbutler_protocol::DaemonRequest,
) -> Result<(), String> {
    // The arm's shape gate: an absent key (`undefined`) throws too -
    // only a literal null or a string passes to the controller.
    let project_id = match request.get("projectId") {
        None => return Err("Invalid project ID.".to_owned()),
        Some(Value::Null) => None,
        Some(value) => match value.as_str() {
            Some(text) => Some(text.to_owned()),
            None => return Err("Invalid project ID.".to_owned()),
        },
    };
    let session_id = require_variant(request, "sessionId", 64)?;
    if state.current_session.as_deref() != Some(session_id.as_str()) {
        return Err("The active chat changed. Try again.".to_owned());
    }
    let mut record = current_record(data_dir, state)?;
    // `sessionHasUserMessage` is derived from entries in the source
    // (agent-session.ts L909), not from the stamped flag.
    let started = record
        .entries
        .iter()
        .any(|entry| matches!(entry, SessionEntry::User { .. }));
    if started
        || record
            .meta
            .project
            .as_ref()
            .is_some_and(|project| project.locked)
    {
        return Err(
            "This chat's project was fixed by its first message. Start a new chat.".to_owned(),
        );
    }
    if host
        .executor()
        .is_some_and(|executor| executor.is_busy(&session_id))
    {
        return Err("Wait for the running task to finish before choosing a project.".to_owned());
    }
    // "No project" means where the runtime already points: this build's
    // runtime pointer is the settings default scope; the connection's
    // own launch scope arrives with the client-scope plumbing.
    let fallback = host
        .settings()
        .as_ref()
        .and_then(|loaded| loaded.store.default_workspace_root.clone());
    let project = state::session_project(project_id.as_deref(), fallback.as_deref(), data_dir)?;
    record.meta.project = Some(project);
    state::save_session(data_dir, &record, true).map_err(|error| error.to_string())?;
    // The source's `publish(false)`: the connection sees the re-bound
    // chat after its response, through the same pending queue the
    // attach flow uses.
    let path = state::record_path(data_dir, &session_id).ok_or(CHAT_NOT_FOUND)?;
    let view = state::read_session_view(&path, &session_id).ok_or(CHAT_NOT_FOUND)?;
    let (snapshot, runtime, _workspace_root) = chat_snapshot(host, view, None);
    queue_attach_event(state, snapshot, runtime);
    Ok(())
}
