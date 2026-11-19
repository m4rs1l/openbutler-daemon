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

/// A fresh scratch data directory with an empty `sessions` root.
pub fn temp_data_dir(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("ob-test-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(base.join("sessions")).unwrap();
    base
}
