//! `settings.set-default-workspace` pins: the query answers `null` until
//! someone chose a root, a set canonicalizes through the single workspace
//! normalizer, writes settings.yaml and survives a reload — and the
//! canonicalizer's own refusals come back verbatim.
//!
//! Source: `cli-main.ts` `runWorkspaceCommand` (L340-352), read 2026-11-26.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;

use common::temp_data_dir;
use openbutler_daemon::dispatch::{DispatchState, Host, handle};
use openbutler_protocol::{DaemonRequest, DaemonResponse};
use serde_json::{Value, json};

fn call(host: &Host, dir: &Path, payload: Value) -> DaemonResponse {
    let mut request = DaemonRequest::new("r1", "settings.set-default-workspace");
    if let Value::Object(fields) = payload {
        request.payload = fields;
    }
    handle(host, dir, &mut DispatchState::new(), &request)
}

fn result_of(response: DaemonResponse) -> Value {
    let DaemonResponse::Success { result, .. } = response else {
        panic!("expected success, got {response:?}");
    };
    result
}

fn error_of(response: DaemonResponse) -> String {
    let DaemonResponse::Failure { error, .. } = response else {
        panic!("expected a refusal, got {response:?}");
    };
    error
}

/// The platform's canonical form of `dir`, minus the `\\?\` prefix —
/// what `canonical_scope_root` answers.
fn canonical(dir: &Path) -> String {
    let real = std::fs::canonicalize(dir).unwrap();
    let text = real.display().to_string();
    text.strip_prefix(r"\\?\")
        .map(str::to_owned)
        .unwrap_or(text)
}

fn host_over(dir: &Path) -> Host {
    let host = Host::new();
    host.reload_settings(dir).expect("settings load");
    host
}

#[test]
fn the_query_answers_null_until_a_root_is_chosen() {
    let dir = temp_data_dir("ws-default-none");
    let host = host_over(&dir);
    let result = result_of(call(&host, &dir, json!({})));
    assert!(result["workspace"].is_null());
}

#[test]
fn a_set_canonicalizes_saves_and_survives_a_reload() {
    let dir = temp_data_dir("ws-default-set");
    let workspace = temp_data_dir("ws-default-target");
    let host = host_over(&dir);

    let result = result_of(call(&host, &dir, json!({ "path": workspace })));
    let expected = canonical(&workspace);
    assert_eq!(result["workspace"], json!(expected));

    // The file is the record: a fresh host reads the same root back.
    let text = std::fs::read_to_string(dir.join("settings.yaml")).unwrap();
    assert!(text.contains("defaultWorkspaceRoot:"), "{text}");
    let fresh = host_over(&dir);
    let queried = result_of(call(&fresh, &dir, json!({})));
    assert_eq!(queried["workspace"], json!(expected));
}

#[test]
fn the_canonicalizers_refusals_come_back_verbatim() {
    let dir = temp_data_dir("ws-default-refusals");
    let host = host_over(&dir);

    let empty = error_of(call(&host, &dir, json!({ "path": "   " })));
    assert_eq!(empty, "Workspace path cannot be empty.");

    let missing = temp_data_dir("ws-default-missing");
    let missing = missing.join("nope");
    let error = error_of(call(&host, &dir, json!({ "path": missing })));
    // The canonicalize failure's message is the OS's own; it must be
    // non-empty and never a silent success.
    assert!(!error.is_empty(), "{error}");
}

#[test]
fn a_non_string_path_is_refused_with_the_envelope_wording() {
    let dir = temp_data_dir("ws-default-type");
    let host = host_over(&dir);
    let error = error_of(call(&host, &dir, json!({ "path": 7 })));
    assert_eq!(error, "path must be a non-empty string under 32768 bytes.");
}
