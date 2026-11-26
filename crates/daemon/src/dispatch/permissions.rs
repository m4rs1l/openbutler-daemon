//! The `permissions.set-mode` action — `applyApprovalMode` (settings.ts
//! L1143): load → mutate → save through the write transaction, then
//! push the new mode to the live copy. The client renders what this
//! computes; the mode itself is host-owned state.

use openbutler_protocol::DaemonRequest;
use serde_json::json;

use super::Host;

/// `permissions.set-mode`: one message covers every rejected value;
/// the computed mode is pushed to the live settings copy.
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

/// `permissions.manage`: the `openbutler permissions` surface behind one
/// atomic write — the mode, both timeouts and the review-model role —
/// answering the four display values that screen prints plus what changed.
///
/// Source: `runPermissions` (cli-main.ts L215-309, read 2026-11-26). The
/// source wrote settings itself and pushed only the mode to a live host;
/// here the one write covers everything and the live copy reloads after it.
pub fn manage(
    host: &Host,
    data_dir: &std::path::Path,
    request: &DaemonRequest,
) -> Result<serde_json::Value, String> {
    // Parse everything before touching the store: a refused flag leaves no
    // write behind (the source validated every flag first too).
    let mode = match request.get("mode") {
        None => None,
        Some(value) => Some(match value.as_str() {
            Some("workspace") | Some("ask") => crate::settings::ApprovalMode::Workspace,
            Some("auto-review") => crate::settings::ApprovalMode::AutoReview,
            Some("full-access") => crate::settings::ApprovalMode::FullAccess,
            _ => return Err("mode must be workspace, auto-review, or full-access.".to_owned()),
        }),
    };
    let review_timeout = seconds(request, "reviewTimeoutSeconds", "--review-timeout")?;
    let approval_timeout = seconds(request, "approvalTimeoutSeconds", "--approval-timeout")?;
    const REVIEW_MODEL_USAGE: &str =
        "--review-model must be <provider>/<model>, or `none` to reuse the main model.";
    let review_model: Option<Option<(String, String)>> = match request.get("reviewModel") {
        None => None,
        Some(value) => {
            let Some(text) = value.as_str() else {
                return Err(REVIEW_MODEL_USAGE.to_owned());
            };
            if text == "none" || text.is_empty() {
                Some(None)
            } else {
                let Some((provider, model)) = text.split_once('/') else {
                    return Err(REVIEW_MODEL_USAGE.to_owned());
                };
                if provider.is_empty() || model.is_empty() {
                    return Err(REVIEW_MODEL_USAGE.to_owned());
                }
                Some(Some((provider.to_owned(), model.to_owned())))
            }
        }
    };

    let mut loaded = crate::settings::load_settings(data_dir).map_err(|error| error.to_string())?;
    let mut saved: Vec<&str> = Vec::new();
    if let Some(canonical) = mode {
        loaded.store.approval_mode = canonical;
        saved.push("mode");
    }
    if let Some(value) = review_timeout {
        loaded.store.auto_review_timeout_seconds = Some(value);
        saved.push("reviewTimeout");
    }
    if let Some(value) = approval_timeout {
        loaded.store.approval_timeout_seconds = Some(value);
        saved.push("approvalTimeout");
    }
    if let Some(binding) = review_model {
        match binding {
            None => {
                if let Some(roles) = loaded.store.roles.as_mut() {
                    roles.remove("review");
                }
                loaded.store.auto_review_provider = None;
                loaded.store.auto_review_model = None;
            }
            Some((provider, model)) => {
                loaded
                    .store
                    .roles
                    .get_or_insert_with(std::collections::BTreeMap::new)
                    .insert(
                        "review".to_owned(),
                        crate::settings::RoleBinding {
                            provider: provider.clone(),
                            model: model.clone(),
                        },
                    );
                loaded.store.auto_review_provider = Some(provider);
                loaded.store.auto_review_model = Some(model);
            }
        }
        saved.push("reviewModel");
    }
    if !saved.is_empty() {
        crate::settings::save_settings(data_dir, &mut loaded).map_err(|error| error.to_string())?;
        host.reload_settings(data_dir)
            .map_err(|error| error.to_string())?;
    }

    let store = &loaded.store;
    Ok(json!({
        "mode": mode_label(store.approval_mode),
        "reviewTimeout": store
            .auto_review_timeout_seconds
            .map(|value| format!("{value} seconds"))
            .unwrap_or_else(|| "15 seconds (default)".to_owned()),
        "approvalTimeout": store
            .approval_timeout_seconds
            .map(|value| format!("{value} seconds"))
            .unwrap_or_else(|| "120 seconds (default)".to_owned()),
        "reviewModel": match (&store.auto_review_provider, &store.auto_review_model) {
            (Some(provider), Some(model)) => format!("{provider}/{model}"),
            _ => "(main model)".to_owned(),
        },
        "saved": saved,
    }))
}

/// One timeout flag: an integer in `5..=600`, or the source's sentence.
fn seconds(request: &DaemonRequest, key: &str, flag: &str) -> Result<Option<u64>, String> {
    let Some(value) = request.get(key) else {
        return Ok(None);
    };
    let Some(number) = value.as_u64() else {
        return Err(format!(
            "{flag} must be a whole number of seconds between 5 and 600."
        ));
    };
    if !(5..=600).contains(&number) {
        return Err(format!(
            "{flag} must be a whole number of seconds between 5 and 600."
        ));
    }
    Ok(Some(number))
}

/// `formatApprovalMode` (cli-main.ts L334-338): the label the screen shows.
fn mode_label(mode: crate::settings::ApprovalMode) -> &'static str {
    match mode {
        crate::settings::ApprovalMode::AutoReview => {
            "Approve for me (Auto-review outside workspace)"
        }
        crate::settings::ApprovalMode::FullAccess => "Full access (no permission prompts)",
        crate::settings::ApprovalMode::Workspace => "Workspace (default)",
    }
}
