//! `keys.list` / `keys.add` / `keys.remove` pins: the store semantics from
//! `cli/keys.ts` (read 2026-11-26) — append without duplicates, drop
//! keeping the entry, display tails only, the classification refusals and
//! the local-only write gate — plus the split that puts keys in
//! config.yaml and never in settings.yaml.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;

use common::temp_data_dir;
use openbutler_daemon::dispatch::{DispatchState, Host, handle};
use openbutler_protocol::{DaemonRequest, DaemonResponse};
use serde_json::{Value, json};

fn call(host: &Host, dir: &Path, action: &str, payload: Value) -> DaemonResponse {
    let mut request = DaemonRequest::new("r1", action);
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

fn host_over(dir: &Path) -> Host {
    let host = Host::new();
    host.reload_settings(dir).expect("settings load");
    host
}

/// A settings file with one configured provider and one MCP server.
fn seed(dir: &Path) {
    std::fs::write(
        dir.join("settings.yaml"),
        "providers:\n  mock:\n    enabled: true\n    keys: []\nmcpServers:\n  local:\n    transport: stdio\n    command: npx\n",
    )
    .unwrap();
}

#[test]
fn list_without_providers_answers_an_empty_row_set() {
    let dir = temp_data_dir("keys-empty");
    let host = host_over(&dir);
    let result = result_of(call(&host, &dir, "keys.list", json!({})));
    assert_eq!(result["providers"], json!([]));
}

#[test]
fn add_creates_the_entry_persists_and_shows_only_tails() {
    let dir = temp_data_dir("keys-add");
    // The source refuses unknown providers: the entry exists (empty) or
    // the catalog knows the id.
    std::fs::write(
        dir.join("settings.yaml"),
        "providers:\n  acme:\n    enabled: true\n    keys: []\n",
    )
    .unwrap();
    let host = host_over(&dir);
    let result = result_of(call(
        &host,
        &dir,
        "keys.add",
        json!({ "provider": "acme", "keys": ["  sk-first  ", "sk-second", "sk-first"] }),
    ));
    assert_eq!(result["added"], json!(2));

    let listed = result_of(call(&host, &dir, "keys.list", json!({})));
    let row = &listed["providers"][0];
    assert_eq!(row["id"], "acme");
    assert_eq!(row["enabled"], json!(true));
    assert_eq!(row["keyCount"], json!(2));
    // Display fragments, never the key.
    assert_eq!(row["tails"], json!(["…irst", "…cond"]));
    assert!(!listed.to_string().contains("sk-first"));
}

#[test]
fn add_to_an_existing_provider_appends_without_duplicates() {
    let dir = temp_data_dir("keys-append");
    seed(&dir);
    let host = host_over(&dir);
    assert_eq!(
        result_of(call(
            &host,
            &dir,
            "keys.add",
            json!({ "provider": "mock", "keys": ["sk-one"] }),
        ))["added"],
        json!(1)
    );
    // The same key again is not new; a fresh one is.
    assert_eq!(
        result_of(call(
            &host,
            &dir,
            "keys.add",
            json!({ "provider": "mock", "keys": ["sk-one", "sk-two"] }),
        ))["added"],
        json!(1)
    );
    let listed = result_of(call(&host, &dir, "keys.list", json!({})));
    assert_eq!(listed["providers"][0]["keyCount"], json!(2));
}

#[test]
fn remove_clears_the_keys_and_keeps_the_entry() {
    let dir = temp_data_dir("keys-remove");
    std::fs::write(
        dir.join("settings.yaml"),
        "providers:\n  acme:\n    enabled: true\n    keys: []\n",
    )
    .unwrap();
    let host = host_over(&dir);
    call(
        &host,
        &dir,
        "keys.add",
        json!({ "provider": "acme", "keys": ["sk-a", "sk-b"] }),
    );
    let result = result_of(call(
        &host,
        &dir,
        "keys.remove",
        json!({ "provider": "acme" }),
    ));
    assert_eq!(result["removed"], json!(2));
    let listed = result_of(call(&host, &dir, "keys.list", json!({})));
    assert_eq!(listed["providers"][0]["id"], "acme");
    assert_eq!(listed["providers"][0]["keyCount"], json!(0));
    // Nothing left to drop on a second call.
    assert_eq!(
        result_of(call(
            &host,
            &dir,
            "keys.remove",
            json!({ "provider": "acme" }),
        ))["removed"],
        json!(0)
    );
}

#[test]
fn an_mcp_server_id_is_refused_with_the_source_sentence() {
    let dir = temp_data_dir("keys-mcp");
    seed(&dir);
    let host = host_over(&dir);
    let error = error_of(call(
        &host,
        &dir,
        "keys.add",
        json!({ "provider": "local", "keys": ["sk-x"] }),
    ));
    assert!(error.starts_with("\"local\" is an MCP server, not a model provider."));
    assert!(error.contains("openbutler mcp"));
}

#[test]
fn an_unknown_provider_is_refused_with_the_source_sentence() {
    let dir = temp_data_dir("keys-unknown");
    seed(&dir);
    let host = host_over(&dir);
    let error = error_of(call(
        &host,
        &dir,
        "keys.add",
        json!({ "provider": "nope", "keys": ["sk-x"] }),
    ));
    assert_eq!(
        error,
        "Unknown provider \"nope\". Configure it first with openbutler setup."
    );
}

#[test]
fn keys_must_be_an_array_of_strings() {
    let dir = temp_data_dir("keys-shape");
    seed(&dir);
    let host = host_over(&dir);
    for payload in [
        json!({ "provider": "mock" }),
        json!({ "provider": "mock", "keys": "sk-x" }),
        json!({ "provider": "mock", "keys": [7] }),
    ] {
        let error = error_of(call(&host, &dir, "keys.add", payload.clone()));
        assert_eq!(error, "keys must be an array of strings.", "{payload}");
    }
}

#[test]
fn a_signed_in_remote_session_may_write_the_store() {
    let dir = temp_data_dir("keys-remote");
    seed(&dir);
    let host = host_over(&dir);
    let mut state = DispatchState::new();
    state.set_remote(true);
    let mut request = DaemonRequest::new("r1", "keys.add");
    request.payload.insert("provider".to_owned(), json!("mock"));
    request
        .payload
        .insert("keys".to_owned(), json!(["sk-remote"]));
    // A remote connection only exists after the admin signed in, so this write
    // is the admin's own; the local-only rule covers identity and process
    // control, not the provider store.
    let response = handle(&host, &dir, &mut state, &request);
    assert!(
        !matches!(response, DaemonResponse::Failure { .. }),
        "{response:?}"
    );
    let secrets = std::fs::read_to_string(dir.join("config.yaml")).unwrap();
    assert!(secrets.contains("sk-remote"), "{secrets}");
}

#[test]
fn keys_land_in_config_yaml_and_never_in_settings_yaml() {
    let dir = temp_data_dir("keys-split");
    std::fs::write(
        dir.join("settings.yaml"),
        "providers:\n  acme:\n    enabled: true\n    keys: []\n",
    )
    .unwrap();
    let host = host_over(&dir);
    call(
        &host,
        &dir,
        "keys.add",
        json!({ "provider": "acme", "keys": ["sk-super-secret-value"] }),
    );
    let settings = std::fs::read_to_string(dir.join("settings.yaml")).unwrap();
    assert!(
        !settings.contains("sk-super-secret-value"),
        "settings.yaml must never carry a key: {settings}"
    );
    let secrets = std::fs::read_to_string(dir.join("config.yaml")).unwrap();
    assert!(
        secrets.contains("sk-super-secret-value"),
        "config.yaml is the key store: {secrets}"
    );
}
