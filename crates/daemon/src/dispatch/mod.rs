//! Action routing and the host record.
//!
//! `Host` is the authority for what `status` reports (identity, start time,
//! live connection count, shutdown signal). `DispatchState` is what one
//! connection carries between requests — the chat it has opened. `handle` is
//! the single dispatch point: every action is routed here, validated against
//! the envelope rules, and answered as one protocol response — no other
//! module constructs responses.

mod schedule;
pub mod session_send;

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};

use openbutler_protocol::entry::UserEntrySource;
use openbutler_protocol::session::{SessionView, read};
use openbutler_protocol::{
    DAEMON_PROTOCOL_VERSION, DaemonRequest, DaemonResponse, DaemonStatus, InlineAttachment,
    SessionEntriesRange, validate_request,
};
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::agent::providers::TeamInfo;
use crate::agent::{TurnExecutor, TurnRequest};
use crate::auth::PairingCodes;
use crate::ledger;
use crate::settings::schema::TeamRole;
use crate::settings::{LoadedSettings, SettingsError, load_settings};
use crate::state::{self, WorkspaceFilter};

/// The chat this connection has opened. `session.load` records it; the
/// paging and attachment reads address it — the per-connection stand-in for
/// a host-wide current controller (there is no runtime in this build,
/// so the record on disk is the whole truth).
#[derive(Debug, Default)]
pub struct DispatchState {
    current_session: Option<String>,
    /// The client kind the connection attached with; until `attach` sets
    /// it, sends read as terminal (the source's default channel).
    client_kind: Option<UserEntrySource>,
}

impl DispatchState {
    /// A fresh connection holds no chat yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The id `session.load` last succeeded with (tests and diagnostics).
    pub fn current_session(&self) -> Option<&str> {
        self.current_session.as_deref()
    }
}

/// Fixed sentences (verbatim): the custodian refusal on load and the miss
/// every read of an unopened or vanished chat reports.
const CHAT_NOT_FOUND: &str = "Chat not found.";
const CUSTODIAN_LOAD_ERROR: &str =
    "The machine custodian is the host's own conversation; it is not a chat to open.";

/// What the daemon reports about itself and what keeps it alive.
pub struct Host {
    started_at: String,
    connected_clients: AtomicU32,
    shutdown: watch::Sender<bool>,
    pairing: PairingCodes,
    /// The live settings copy every runtime read goes through. Loaded and
    /// reloaded only by `reload_settings` — one writer for the host.
    settings: RwLock<Option<LoadedSettings>>,
    /// The host-wide turn executor (door + runners). Set once at boot;
    /// `session.send` refuses without it.
    executor: RwLock<Option<Arc<dyn TurnExecutor>>>,
}

impl Host {
    /// Identity and clock start now; the shutdown channel opens closed.
    pub fn new() -> Self {
        let started_at = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned());
        let (shutdown, _) = watch::channel(false);
        Self {
            started_at,
            connected_clients: AtomicU32::new(0),
            shutdown,
            pairing: PairingCodes::new(),
            settings: RwLock::new(None),
            executor: RwLock::new(None),
        }
    }

    /// The live settings copy, if it has been loaded yet. `None` means
    /// nobody called `reload_settings` — the server boot path always does.
    pub fn settings(&self) -> std::sync::RwLockReadGuard<'_, Option<LoadedSettings>> {
        self.settings
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Refresh the live copy from disk under the same lock every reader
    /// uses. A failed load leaves the previous copy untouched: a broken
    /// file must never evict working settings.
    pub fn reload_settings(&self, data_dir: &Path) -> Result<(), SettingsError> {
        let fresh = load_settings(data_dir)?;
        *self
            .settings
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(fresh);
        Ok(())
    }

    /// The shared pairing state: `pair.create` issues through it, the
    /// admission gate redeems through it.
    pub fn pairing(&self) -> &PairingCodes {
        &self.pairing
    }

    /// Install the turn executor (boot wiring or a test double).
    pub fn set_executor(&self, executor: Arc<dyn TurnExecutor>) {
        *self
            .executor
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(executor);
    }

    /// The installed executor, if any.
    pub fn executor(&self) -> Option<Arc<dyn TurnExecutor>> {
        self.executor
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The current status view (`status` action).
    pub fn status(&self) -> DaemonStatus {
        DaemonStatus {
            pid: std::process::id(),
            started_at: self.started_at.clone(),
            // Workspace runtimes are not implemented yet: an honest zero,
            // not a fabricated count.
            workspace_count: 0,
            connected_clients: self.connected_clients.load(Ordering::SeqCst),
            schedules: None,
            telegram: None,
            unresponsive: None,
            entry: None,
            // A source run reports an explicit null (contract: null = source
            // run, absent = older host).
            build_id: Some(None),
        }
    }

    /// Live IPC connections are counted by RAII guards in the server.
    pub(crate) fn client_enter(&self) {
        self.connected_clients.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn client_exit(&self) {
        self.connected_clients.fetch_sub(1, Ordering::SeqCst);
    }

    /// Subscribe to the shutdown signal (serve loop + tests).
    pub fn subscribe_shutdown(&self) -> watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    /// Ask the serve loop to stop; idempotent.
    pub fn request_shutdown(&self) {
        let _ = self.shutdown.send(true);
    }
}

impl Default for Host {
    fn default() -> Self {
        Self::new()
    }
}

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
    match request.action.as_str() {
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
        other => failure(request, format!("Unknown action: {other}")),
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
    let meta = record.meta;
    let settings = host.settings();
    let loaded = settings
        .as_ref()
        .ok_or_else(|| "settings are not loaded".to_owned())?;
    let provider_url = loaded
        .store
        .providers
        .get(&meta.provider_id)
        .and_then(|provider| provider.url.clone());
    let workspace_root = meta
        .workspace_root
        .clone()
        .or_else(|| loaded.store.default_workspace_root.clone());
    let team = loaded.store.team.as_ref().map(|team| TeamInfo {
        team_name: team.team_name.clone(),
        member_name: team.member_name.clone(),
        hub: matches!(team.role, TeamRole::Hub),
    });
    drop(settings);
    let secrets = crate::settings::load_secrets(data_dir).map_err(|error| error.to_string())?;
    let provider_api_key = secrets
        .providers
        .get(&meta.provider_id)
        .and_then(|secret| secret.keys.first().cloned());
    crate::agent::context::prepare_turn(
        data_dir,
        crate::agent::context::TurnInputs {
            meta: &meta,
            workspace_root,
            provider_url,
            provider_api_key,
            team,
            home: None,
            source,
            text,
            images,
        },
    )
}
