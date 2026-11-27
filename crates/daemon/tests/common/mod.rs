//! Shared fixtures for integration tests.
//!
//! One copy of the on-disk builders: every test file that needs a session
//! record or a scratch data directory uses these, so a change to the writer
//! protocol lands in one place.
#![allow(dead_code)] // each test binary uses a subset

use std::fs;
use std::path::{Path, PathBuf};

use openbutler_daemon::dispatch::DispatchState;
use openbutler_protocol::DaemonRequest;
use serde_json::json;

/// The key a test client attached with. Every post-attach action must
/// echo it back (daemon-host.ts L3478).
pub const WORKSPACE_KEY: &str = "/ob/test-workspace";

/// A connection that has already attached but has not opened a chat.
/// Actions outside the pre-attach partition pass the binding gate; the
/// ones that address a chat still answer the connection's own miss.
pub fn bound_state() -> DispatchState {
    let mut state = DispatchState::new();
    state.binding_key = Some(WORKSPACE_KEY.to_owned());
    state
}

/// Echoes the connection's stored binding key on a request, the way a
/// real client returns the key `attach` answered with. An explicit key
/// always wins, so gate-refusal tests keep their own value.
pub fn with_binding(state: &DispatchState, mut request: DaemonRequest) -> DaemonRequest {
    if let Some(key) = state.binding_key.as_deref()
        && request.payload.get("workspaceKey").is_none()
    {
        request = request.with("workspaceKey", json!(key));
    }
    request
}

/// A record as `session-writer.ts` writes it: two-space pretty JSON with a
/// trailing newline and the big arrays last.
pub fn pretty_session(
    id: &str,
    workspace: Option<&str>,
    title: &str,
    has_user_message: bool,
) -> String {
    let workspace_field = match workspace {
        Some(root) => format!("    \"workspaceRoot\": {root:?},\n"),
        None => String::new(),
    };
    format!(
        "{{\n  \"version\": 1,\n  \"id\": \"{id}\",\n  \"title\": \"{title}\",\n  \"createdAt\": \"2026-11-15T20:00:00.000Z\",\n  \"updatedAt\": \"2026-11-15T20:00:00.000Z\",\n  \"lastActivityAt\": \"2026-11-15T20:05:00.000Z\",\n  \"providerId\": \"vercel\",\n  \"modelId\": \"xiaomi/mimo-v2.6-flash\",\n{workspace_field}  \"hasUserMessage\": {has_user_message},\n  \"preview\": \"earlier turn\",\n  \"firstUserMessage\": \"start\",\n  \"lastAssistantMessage\": \"done\",\n  \"entries\": [\n    {{\n      \"kind\": \"user\",\n      \"text\": \"start\",\n      \"source\": \"terminal\"\n    }}\n  ],\n  \"conversation\": [\n    {{ \"role\": \"user\", \"content\": \"start\" }}\n  ]\n}}\n"
    )
}

pub fn uuid(seed: u8) -> String {
    format!("{seed:08x}-0000-4000-8000-000000000001")
}

pub fn write_session(root: &Path, id: &str, body: &str) {
    let dir = root.join("sessions").join(id);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(format!("{id}.json")), body).unwrap();
}

/// The spelling the host stores for a workspace path: the resolved path
/// with the Windows extended-length prefix removed. A CI runner keeps its
/// temp directory behind an 8.3 short name, so a test that compares the
/// raw spelling fails there and only there.
pub fn canonical_root(path: &Path) -> String {
    let resolved = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = resolved.to_string_lossy().into_owned();
    match text.strip_prefix(r"\\?\") {
        Some(rest) => rest.to_owned(),
        None => text,
    }
}

/// One process-wide sweep of abandoned scratch trees. A test that panics or is
/// cancelled cannot remove its directory, and gate runs would otherwise pile
/// thousands of `ob-*` trees in the system temp area. Anything older than a day
/// is by definition not a run in progress.
fn sweep_stale_scratch() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
            return;
        };
        let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 60 * 60);
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with("ob-") {
                continue;
            }
            let abandoned = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .map(|stamp| stamp < cutoff)
                .unwrap_or(false);
            if abandoned && entry.path().is_dir() {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    });
}

/// A fresh scratch data directory with an empty `sessions` root.
pub fn temp_data_dir(tag: &str) -> PathBuf {
    sweep_stale_scratch();
    let base = std::env::temp_dir().join(format!("ob-test-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(base.join("sessions")).unwrap();
    base
}

/// The turn seam's test double: busy on demand, nothing else. One copy for
/// new tests; the per-file doubles in the older suites predate this helper.
#[derive(Clone, Default)]
pub struct RecordingExecutor {
    pub busy: std::sync::Arc<std::sync::Mutex<bool>>,
    /// Every baseline the attach path handed over (session id + snapshot).
    pub seeds: std::sync::Arc<
        std::sync::Mutex<Vec<(String, openbutler_daemon::agent::events::PublishSnapshot)>>,
    >,
}

impl openbutler_daemon::agent::TurnExecutor for RecordingExecutor {
    fn seed_baseline(
        &self,
        session_id: &str,
        snapshot: openbutler_daemon::agent::events::PublishSnapshot,
    ) {
        self.seeds
            .lock()
            .unwrap()
            .push((session_id.to_owned(), snapshot));
    }

    fn spawn_turn(
        self: std::sync::Arc<Self>,
        _session_id: String,
        _request: openbutler_daemon::agent::TurnRequest,
    ) {
    }

    fn approve(&self, _session_id: &str, _approval_id: &str, _approved: bool) -> bool {
        false
    }

    fn stop(&self, _session_id: &str, _keep_queue: bool) -> bool {
        false
    }

    fn manage_queue(
        &self,
        _session_id: &str,
        _message_id: &str,
        _action: openbutler_daemon::agent::QueueAction,
    ) -> Result<(), String> {
        Ok(())
    }

    fn answer(&self, _session_id: &str, _question_id: &str, _answer: Option<String>) -> bool {
        false
    }

    fn is_busy(&self, _session_id: &str) -> bool {
        *self.busy.lock().unwrap()
    }

    fn compact(self: std::sync::Arc<Self>, _session_id: String, _recovery: bool) {}

    fn has_live_session(&self, _session_id: &str) -> bool {
        false
    }

    fn spawn_turn_for_event(
        self: std::sync::Arc<Self>,
        _session_id: String,
        _request: openbutler_daemon::agent::TurnRequest,
    ) -> tokio::sync::oneshot::Receiver<Result<(), String>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = tx.send(Ok(()));
        rx
    }
}
