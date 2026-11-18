//! The `permissions.set-mode` action — `applyApprovalMode` (settings.ts
//! L1143): load → mutate → save through the write transaction, then
//! push the new mode to the live copy. The client renders what this
//! computes; the mode itself is host-owned state.

use openbutler_protocol::DaemonRequest;
use serde_json::json;

use super::Host;

pub fn set_mode(
    host: &Host,
    data_dir: &std::path::Path,
    request: &DaemonRequest,
) -> Result<serde_json::Value, String> {
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
        return Err("mode must be workspace, auto-review, or full-access.".to_owned());
    };
    // `applyApprovalMode(mode, persist)`: load → mutate → save,
    // then push to the live runtimes — this build has no runtimes
    // yet, so the live half is the host's settings copy.
    let mut loaded = crate::settings::load_settings(data_dir).map_err(|error| error.to_string())?;
    loaded.store.approval_mode = canonical;
    crate::settings::save_settings(data_dir, &mut loaded).map_err(|error| error.to_string())?;
    host.reload_settings(data_dir)
        .map_err(|error| error.to_string())?;
    Ok(json!({ "mode": canonical.as_str() }))
}
