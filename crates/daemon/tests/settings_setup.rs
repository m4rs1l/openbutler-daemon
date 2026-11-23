//! `settings.setup` — the wizard's atomic write: refusal wording, the
//! split-document contract (keys never touch `settings.yaml`), the
//! `withProviderFields` merge, and the defaults.
//!
//! A new contract rather than a port (the source wrote the files from the
//! CLI), so the pins are the payload validation and the pair save.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::fs;
use std::path::Path;

use common::temp_data_dir;
use openbutler_daemon::dispatch::{self, Host};
use openbutler_daemon::settings::{load_secrets, load_settings};
use openbutler_protocol::{DaemonRequest, DaemonResponse};
use serde_json::{Value, json};

const KEY: &str = "sk-fixture-do-not-leak-0123456789";
const URL: &str = "http://127.0.0.1:9999/v1";

/// One setup call on a fresh host: every run reloads from disk, the way
/// two wizard invocations would.
fn setup(dir: &Path, id: &str, payload: Value) -> DaemonResponse {
    let host = Host::new();
    let mut state = dispatch::DispatchState::new();
    let mut request = DaemonRequest::new(id, "settings.setup");
    if let Value::Object(fields) = payload {
        for (key, value) in fields {
            request.payload.insert(key, value);
        }
    }
    dispatch::handle(&host, dir, &mut state, &request)
}

fn refusal(dir: &Path, id: &str, payload: Value) -> String {
    match setup(dir, id, payload) {
        DaemonResponse::Failure { error, .. } => error,
        DaemonResponse::Success { result, .. } => panic!("expected a refusal, got {result}"),
    }
}

fn provider(id: &str, keys: Vec<Value>) -> Value {
    json!({ "id": id, "keys": keys })
}

#[test]
fn providers_must_be_a_bounded_array() {
    let dir = temp_data_dir("setup-array");
    assert_eq!(
        refusal(&dir, "s1", json!({ "providers": "mock" })),
        "settings.setup providers must be an array of at most 32 entries."
    );
    let too_many: Vec<Value> = (0..33).map(|_| provider("p", vec![json!("k")])).collect();
    assert_eq!(
        refusal(&dir, "s2", json!({ "providers": too_many })),
        "settings.setup providers must be an array of at most 32 entries."
    );
    // Validation runs before any write: a refused run creates nothing.
    assert!(
        !dir.join("settings.yaml").exists() && !dir.join("config.yaml").exists(),
        "a refused payload writes no documents"
    );
}

#[test]
fn entries_must_be_mappings_with_id_and_keys() {
    let dir = temp_data_dir("setup-entry");
    assert_eq!(
        refusal(&dir, "e1", json!({ "providers": ["mock"] })),
        "providers[0] must be a mapping with id and keys."
    );
    assert_eq!(
        refusal(&dir, "e2", json!({ "providers": [{ "keys": [KEY] }] })),
        "providers[0].id must be a non-empty string under 256 bytes."
    );
    assert_eq!(
        refusal(
            &dir,
            "e3",
            json!({ "providers": [{ "id": "  ", "keys": [KEY] }] })
        ),
        "providers[0].id must be a non-empty string under 256 bytes."
    );
    assert_eq!(
        refusal(
            &dir,
            "e4",
            json!({ "providers": [{ "id": "mock", "keys": "k" }] })
        ),
        "providers[0].keys must be a non-empty array of at most 16 strings under 4096 bytes each."
    );
    assert_eq!(
        refusal(
            &dir,
            "e5",
            json!({ "providers": [{ "id": "mock", "keys": [] }] })
        ),
        "providers[0].keys must be a non-empty array of at most 16 strings under 4096 bytes each."
    );
    assert_eq!(
        refusal(
            &dir,
            "e6",
            json!({ "providers": [{ "id": "mock", "keys": [42] }] })
        ),
        "providers[0].keys must be a non-empty array of at most 16 strings under 4096 bytes each."
    );
}

#[test]
fn bounded_fields_refuse_by_name() {
    let dir = temp_data_dir("setup-bounds");
    let long_url = "a".repeat(2049);
    assert_eq!(
        refusal(
            &dir,
            "b1",
            json!({ "providers": [{ "id": "mock", "keys": [KEY], "url": long_url }] })
        ),
        "providers[0].url must be a string under 2048 bytes."
    );
    let long_npm = "a".repeat(257);
    assert_eq!(
        refusal(
            &dir,
            "b2",
            json!({ "providers": [{ "id": "mock", "keys": [KEY], "npm": long_npm }] })
        ),
        "providers[0].npm must be a string under 256 bytes."
    );
    assert_eq!(
        refusal(
            &dir,
            "b3",
            json!({ "providers": [], "defaultProvider": "" })
        ),
        "defaultProvider must be a non-empty string under 256 bytes."
    );
    assert_eq!(
        refusal(&dir, "b4", json!({ "providers": [], "defaultModel": 7 })),
        "defaultModel must be a non-empty string under 256 bytes."
    );
}

#[test]
fn sixteen_keys_pass_the_boundary() {
    let dir = temp_data_dir("setup-boundary");
    let keys: Vec<Value> = (0..16).map(|i| json!(format!("k{i}"))).collect();
    match setup(
        &dir,
        "boundary",
        json!({ "providers": [provider("mock", keys)] }),
    ) {
        DaemonResponse::Success { .. } => {}
        DaemonResponse::Failure { error, .. } => {
            panic!("sixteen keys are inside the bound: {error}")
        }
    }
}

#[test]
fn the_pair_is_written_split_and_merges_in_memory() {
    let dir = temp_data_dir("setup-split");
    let DaemonResponse::Success { result, .. } = setup(
        &dir,
        "split-1",
        json!({
            "providers": [json!({ "id": "mock", "keys": [KEY], "url": URL })],
            "defaultProvider": "mock",
            "defaultModel": "mock/model-a",
        }),
    ) else {
        panic!("the happy path must succeed");
    };
    assert!(
        result["settingsFile"]
            .as_str()
            .unwrap()
            .ends_with("settings.yaml"),
        "the answer names the settings file"
    );
    assert!(
        result["configFile"]
            .as_str()
            .unwrap()
            .ends_with("config.yaml"),
        "the answer names the config file"
    );

    let settings_text = fs::read_to_string(dir.join("settings.yaml")).unwrap();
    let config_text = fs::read_to_string(dir.join("config.yaml")).unwrap();
    assert!(
        settings_text.contains("mock"),
        "the provider is in settings"
    );
    assert!(settings_text.contains(URL), "the url is in settings");
    assert!(
        !settings_text.contains(KEY),
        "the key never reaches settings.yaml"
    );
    assert!(config_text.contains(KEY), "the key lives in config.yaml");

    // Split on disk, merged in memory: a load reassembles both halves.
    let settings = load_settings(&dir).unwrap();
    let provider = &settings.providers["mock"];
    assert!(provider.enabled, "enabled defaults to true");
    assert_eq!(provider.url.as_deref(), Some(URL));
    assert_eq!(
        provider.keys,
        vec![KEY.to_owned()],
        "the load merges keys back"
    );
    assert_eq!(settings.provider_order, vec!["mock".to_owned()]);
    assert_eq!(settings.default_provider, "mock");
    assert_eq!(settings.default_model, "mock/model-a");
    let secrets = load_secrets(&dir).unwrap();
    assert_eq!(secrets.providers["mock"].keys, vec![KEY.to_owned()]);
}

#[test]
fn second_run_merges_appends_order_and_replaces_keys() {
    let dir = temp_data_dir("setup-merge");
    let first = setup(
        &dir,
        "merge-1",
        json!({ "providers": [json!({ "id": "alpha", "keys": ["k1"], "url": URL })] }),
    );
    assert!(matches!(first, DaemonResponse::Success { .. }));

    // A provider the order never named is appended; the untouched provider
    // keeps its url (absent field = previous value).
    let second = setup(
        &dir,
        "merge-2",
        json!({
            "providers": [provider("beta", vec![json!("k2")])],
            "defaultProvider": "beta",
        }),
    );
    assert!(matches!(second, DaemonResponse::Success { .. }));

    // Re-running the same provider replaces keys and keeps the order free
    // of duplicates; an empty url is an absent patch, not a deletion.
    let third = setup(
        &dir,
        "merge-3",
        json!({ "providers": [json!({ "id": "alpha", "keys": ["k3"], "url": "" })] }),
    );
    assert!(matches!(third, DaemonResponse::Success { .. }));

    let settings = load_settings(&dir).unwrap();
    assert_eq!(
        settings.provider_order,
        vec!["alpha".to_owned(), "beta".to_owned()],
        "append-only order, no duplicates"
    );
    let alpha = &settings.providers["alpha"];
    assert_eq!(
        alpha.url.as_deref(),
        Some(URL),
        "an empty url never deletes"
    );
    assert_eq!(
        alpha.keys,
        vec!["k3".to_owned()],
        "an incoming key list replaces"
    );
    assert_eq!(
        settings.providers["beta"].keys,
        vec!["k2".to_owned()],
        "the untouched provider keeps its keys"
    );
    assert_eq!(settings.default_provider, "beta");
}

#[test]
fn empty_url_never_reaches_the_document() {
    let dir = temp_data_dir("setup-nourl");
    let DaemonResponse::Success { .. } = setup(
        &dir,
        "nourl",
        json!({ "providers": [json!({ "id": "mock", "keys": [KEY], "url": "" })] }),
    ) else {
        panic!("an empty url is an absent patch, not an error");
    };
    let settings_text = fs::read_to_string(dir.join("settings.yaml")).unwrap();
    assert!(
        !settings_text.contains("url"),
        "no url line is written for an absent patch"
    );
    let settings = load_settings(&dir).unwrap();
    assert!(settings.providers["mock"].url.is_none());
}
