//! `settings.set-default-workspace`: the workspace bare launches use.
//!
//! Source: `cli-main.ts` `runWorkspaceCommand` (L340-352). The source
//! wrote settings from the CLI process; the write is the daemon's here.
//!
//! Payload: `{ "path": "<dir>" }` canonicalizes and saves, `{}` or
//! `{ "path": null }` answers the stored value. The path rule is
//! `canonical_scope_root` — the single normalizer every workspace path
//! already goes through — so a saved root is exactly what a session would
//! resolve from the same text.

use std::path::Path;

use openbutler_protocol::DaemonRequest;
use serde_json::{Value, json};

use crate::dispatch::Host;
use crate::state::canonical_scope_root;

/// The path's size gate — the same ceiling the `workspaceKey` carries.
const MAX_WORKSPACE_PATH_BYTES: usize = 32 * 1024;

/// Saves the default workspace and answers the stored value.
pub fn handle(host: &Host, data_dir: &Path, request: &DaemonRequest) -> Result<Value, String> {
    let Some(value) = request.get("path") else {
        return Ok(current(host));
    };
    if value.is_null() {
        return Ok(current(host));
    }
    let path = crate::dispatch::require(request.get("path"), "path", MAX_WORKSPACE_PATH_BYTES)?;
    let root = canonical_scope_root(&path)?;
    let mut loaded = crate::settings::load_settings(data_dir).map_err(|error| error.to_string())?;
    loaded.store.default_workspace_root = Some(root.clone());
    crate::settings::save_settings(data_dir, &mut loaded).map_err(|error| error.to_string())?;
    host.reload_settings(data_dir)
        .map_err(|error| error.to_string())?;
    Ok(json!({
        "workspace": root,
        "lines": [crate::shape::screen_line(
            "success",
            format!("Bare OpenButler launches will use {root}."),
        )]
    }))
}

/// The stored value, or `null` when nobody chose one (the source's
/// `defaultWorkspaceRoot ?? …` branch).
fn current(host: &Host) -> Value {
    let workspace = host
        .settings()
        .as_ref()
        .and_then(|loaded| loaded.store.default_workspace_root.clone());
    let line = match workspace.as_deref() {
        Some(root) => crate::shape::kv_line("Default workspace", root),
        None => crate::shape::kv_line(
            "Default workspace",
            "(none; bare OpenButler uses the launch directory)",
        ),
    };
    json!({ "workspace": workspace, "lines": [line] })
}
