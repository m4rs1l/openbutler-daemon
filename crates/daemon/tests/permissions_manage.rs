//! `permissions.manage` — the `openbutler permissions` surface behind one
//! atomic write: mode, review/approval timeouts and the review-model role.
//!
//! Source: cli-main.ts L215-309 (`runPermissions`, read 2026-11-26): the
//! four `kv` lines it printed, the two info sentences, the validation
//! sentences for the flags, and `assignRoleModel('review', …)` with its
//! legacy mirrors. The source wrote settings itself and called
//! `permissions.set-mode` on a live host; the host owns the whole write
//! here (backend-owned), and this test pins the one answer the CLI renders.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;

use common::temp_data_dir;
use openbutler_daemon::dispatch::{self, DispatchState, Host};
use openbutler_daemon::settings::load_settings;
use openbutler_protocol::{DaemonRequest, DaemonResponse};
use serde_json::{Value, json};

fn manage(dir: &Path, host: &Host, payload: Value) -> Result<Value, String> {
    let mut state = DispatchState::new();
    let mut request = DaemonRequest::new("perm-1", "permissions.manage");
    if let Value::Object(fields) = payload {
        for (key, value) in fields {
            request.payload.insert(key, value);
        }
    }
    match dispatch::handle(host, dir, &mut state, &request) {
        DaemonResponse::Success { result, .. } => Ok(result),
        DaemonResponse::Failure { error, .. } => Err(error),
    }
}

#[test]
fn the_default_report_names_the_source_displays() {
    let dir = temp_data_dir("perm-defaults");
    let host = Host::new();
    let report = manage(&dir, &host, json!({})).unwrap();
    assert_eq!(
        report,
        json!({
            "mode": "Workspace (default)",
            "reviewTimeout": "15 seconds (default)",
            "approvalTimeout": "120 seconds (default)",
            "reviewModel": "(main model)",
            "saved": []
        })
    );
}

#[test]
fn setting_the_mode_persists_and_reaches_the_live_copy() {
    let dir = temp_data_dir("perm-mode");
    let host = Host::new();
    host.reload_settings(&dir).unwrap();
    let report = manage(&dir, &host, json!({ "mode": "auto-review" })).unwrap();
    assert_eq!(
        report["mode"],
        json!("Approve for me (Auto-review outside workspace)")
    );
    assert_eq!(report["saved"], json!(["mode"]));
    let stored = load_settings(&dir).unwrap();
    assert_eq!(
        stored.store.approval_mode.as_str(),
        "auto-review",
        "the pair save must land on disk"
    );
    let live_mode = {
        let live = host.settings();
        live.as_ref()
            .expect("settings loaded")
            .store
            .approval_mode
            .as_str()
    };
    assert_eq!(live_mode, "auto-review", "the live copy must follow");
    // `ask` is the accepted legacy spelling of the workspace default.
    let report = manage(&dir, &host, json!({ "mode": "ask" })).unwrap();
    assert_eq!(report["mode"], json!("Workspace (default)"));
}

#[test]
fn timeouts_are_validated_with_the_source_sentences_and_persisted() {
    let dir = temp_data_dir("perm-timeouts");
    let host = Host::new();
    let report = manage(
        &dir,
        &host,
        json!({ "reviewTimeoutSeconds": 45, "approvalTimeoutSeconds": 30 }),
    )
    .unwrap();
    assert_eq!(report["reviewTimeout"], json!("45 seconds"));
    assert_eq!(report["approvalTimeout"], json!("30 seconds"));
    assert_eq!(report["saved"], json!(["reviewTimeout", "approvalTimeout"]));
    let stored = load_settings(&dir).unwrap();
    assert_eq!(stored.store.auto_review_timeout_seconds, Some(45));
    assert_eq!(stored.store.approval_timeout_seconds, Some(30));

    assert_eq!(
        manage(&dir, &host, json!({ "reviewTimeoutSeconds": 4 })).unwrap_err(),
        "--review-timeout must be a whole number of seconds between 5 and 600."
    );
    assert_eq!(
        manage(&dir, &host, json!({ "approvalTimeoutSeconds": 601 })).unwrap_err(),
        "--approval-timeout must be a whole number of seconds between 5 and 600."
    );
    assert_eq!(
        manage(&dir, &host, json!({ "mode": "nonsense" })).unwrap_err(),
        "mode must be workspace, auto-review, or full-access."
    );
}

#[test]
fn the_review_model_writes_the_role_and_its_legacy_mirrors() {
    let dir = temp_data_dir("perm-review-model");
    let host = Host::new();
    let report = manage(&dir, &host, json!({ "reviewModel": "mock/model-x" })).unwrap();
    assert_eq!(report["reviewModel"], json!("mock/model-x"));
    assert_eq!(report["saved"], json!(["reviewModel"]));
    let stored = load_settings(&dir).unwrap();
    let role = stored
        .store
        .roles
        .as_ref()
        .and_then(|roles| roles.get("review"))
        .expect("the review role must be bound");
    assert_eq!(role.provider, "mock");
    assert_eq!(role.model, "model-x");
    assert_eq!(stored.store.auto_review_provider.as_deref(), Some("mock"));
    assert_eq!(stored.store.auto_review_model.as_deref(), Some("model-x"));

    // `none` clears the role and its mirrors: the main model is reused.
    let report = manage(&dir, &host, json!({ "reviewModel": "none" })).unwrap();
    assert_eq!(report["reviewModel"], json!("(main model)"));
    let stored = load_settings(&dir).unwrap();
    assert!(
        stored
            .store
            .roles
            .as_ref()
            .is_none_or(|roles| !roles.contains_key("review"))
    );
    assert!(stored.store.auto_review_provider.is_none());
    assert!(stored.store.auto_review_model.is_none());

    assert_eq!(
        manage(&dir, &host, json!({ "reviewModel": "no-slash" })).unwrap_err(),
        "--review-model must be <provider>/<model>, or `none` to reuse the main model."
    );
}
