//! The derived view (owner design): configured providers and models
//! joined with their catalog lines — an override, not a copy, keyed by
//! both revisions so a stale derived answer cannot exist.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use common::temp_data_dir;
use openbutler_daemon::catalog::FetchFuture;
use openbutler_daemon::catalog::{
    CatalogStore, ModelSource, ProviderSource, Transport, write_catalog_snapshot,
};
use openbutler_daemon::settings::{ProviderModelSpec, ProviderState, default_settings};
use serde_json::{Value, json};

/// A transport that must never be called by these tests.
struct DeadTransport;

impl Transport for DeadTransport {
    fn fetch(&self, _url: &'static str) -> FetchFuture {
        Box::pin(async { Err("no network in view tests".to_owned()) })
    }
}

fn catalog_payload() -> Value {
    json!({
        "providers": {
            "mock": {
                "name": "Mock Provider",
                "npm": "@ai-sdk/mock",
                "api": "https://api.mock.dev/v1",
                "env": ["MOCK_API_KEY"],
                "models": {
                    "mock-large": {
                        "name": "Mock Large",
                        "reasoning": true,
                        "tool_call": true,
                        "limit": { "context": 200000, "output": 64000 },
                        "cost": { "input": 2.5, "output": 10.0 }
                    }
                }
            }
        }
    })
}

fn provider_state(keys: &[&str], models: &[&str], url: Option<&str>) -> ProviderState {
    ProviderState {
        enabled: true,
        keys: keys.iter().map(|key| (*key).to_owned()).collect(),
        models: if models.is_empty() {
            None
        } else {
            Some(models.iter().map(|model| (*model).to_owned()).collect())
        },
        url: url.map(str::to_owned),
        npm: None,
        options: None,
        routing: None,
        model_routing: None,
        payload: None,
        model_specs: None,
    }
}

fn spec(context: Option<u64>, output: Option<u64>, name: Option<&str>) -> ProviderModelSpec {
    ProviderModelSpec {
        name: name.map(str::to_owned),
        context,
        output,
        reasoning_efforts: None,
        payload: None,
    }
}

fn store_over(dir: &Path) -> CatalogStore {
    write_catalog_snapshot(
        dir,
        &catalog_payload(),
        1_700_000_000_000,
        1_700_000_000_000,
    )
    .unwrap();
    CatalogStore::with_transport(dir, Arc::new(DeadTransport))
}

#[test]
fn catalog_facts_merge_under_settings_overrides() {
    let dir = temp_data_dir("view-merge");
    let store = store_over(&dir);

    let mut settings = default_settings();
    let mut state = provider_state(&["sk-key"], &["mock-large"], Some("http://local/v1"));
    state.model_specs = Some(BTreeMap::from([(
        "mock-large".to_owned(),
        spec(Some(999_999), None, None),
    )]));
    settings.providers.insert("mock".to_owned(), state);
    settings.provider_order = vec!["mock".to_owned()];

    let view = store.view(&settings, "s1");
    let provider = &view.providers[0];
    assert_eq!(provider.id, "mock");
    assert_eq!(provider.name, "Mock Provider", "catalog display name");
    assert!(provider.enabled);
    assert_eq!(
        provider.url.as_deref(),
        Some("http://local/v1"),
        "the settings url override wins"
    );
    assert_eq!(provider.npm.as_deref(), Some("@ai-sdk/mock"), "catalog npm");
    assert_eq!(provider.env, vec!["MOCK_API_KEY".to_owned()]);
    assert!(provider.credentialed);
    assert_eq!(provider.source, ProviderSource::Catalog);

    let model = &provider.models[0];
    assert_eq!(model.id, "mock-large");
    assert_eq!(model.name, "Mock Large");
    assert_eq!(
        model.context_window,
        Some(999_999),
        "the spec context beats the catalog window (one authority, per field)"
    );
    assert_eq!(
        model.output_limit,
        Some(64_000),
        "a field the spec omits falls back to the catalog"
    );
    assert_eq!(model.reasoning, Some(true));
    assert_eq!(model.tool_call, Some(true));
    assert_eq!(model.cost.as_ref().unwrap().input, Some(2.5));
    assert_eq!(model.source, ModelSource::Catalog);
}

#[test]
fn a_custom_provider_falls_back_to_its_settings() {
    let dir = temp_data_dir("view-custom");
    let store = store_over(&dir);

    let mut settings = default_settings();
    let mut state = provider_state(&["local-key"], &["m1"], Some("http://127.0.0.1:8080/v1"));
    state.model_specs = Some(BTreeMap::from([(
        "m1".to_owned(),
        spec(Some(32_000), Some(4_096), Some("My Model")),
    )]));
    settings.providers.insert("local".to_owned(), state);
    settings.provider_order = vec!["local".to_owned()];

    let view = store.view(&settings, "s1");
    let provider = &view.providers[0];
    assert_eq!(provider.source, ProviderSource::Settings);
    assert_eq!(
        provider.name, "local",
        "no catalog name: the id, never a guess"
    );
    assert_eq!(provider.url.as_deref(), Some("http://127.0.0.1:8080/v1"));
    assert_eq!(provider.npm, None);
    assert!(provider.env.is_empty());

    let model = &provider.models[0];
    assert_eq!(model.source, ModelSource::Settings);
    assert_eq!(model.name, "My Model");
    assert_eq!(model.context_window, Some(32_000));
    assert_eq!(model.output_limit, Some(4_096));
    assert_eq!(model.tool_call, None, "no catalog facts for a custom model");
}

#[test]
fn an_unknown_default_model_is_marked_not_invented() {
    let dir = temp_data_dir("view-unknown");
    let store = store_over(&dir);

    let mut settings = default_settings();
    settings
        .providers
        .insert("mock".to_owned(), provider_state(&["k"], &[], None));
    settings.provider_order = vec!["mock".to_owned()];
    settings.default_provider = "mock".to_owned();
    settings.default_model = "ghost-model".to_owned();

    let view = store.view(&settings, "s1");
    let model = &view.providers[0].models[0];
    assert_eq!(
        model.id, "ghost-model",
        "the default pair registers the model"
    );
    assert_eq!(model.source, ModelSource::Unknown);
    assert_eq!(model.name, "ghost-model", "the untouched id, never split");
}

#[test]
fn provider_order_governs_and_missing_entries_append() {
    let dir = temp_data_dir("view-order");
    let store = store_over(&dir);

    let mut settings = default_settings();
    for id in ["alpha", "beta", "gamma"] {
        settings
            .providers
            .insert(id.to_owned(), provider_state(&["k"], &[], None));
    }
    settings.provider_order = vec!["beta".to_owned(), "alpha".to_owned()];

    let view = store.view(&settings, "s1");
    let ids: Vec<&str> = view.providers.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["beta", "alpha", "gamma"],
        "order first, then the ids the order never named"
    );
}

#[test]
fn credentialed_counts_settings_keys_only() {
    let dir = temp_data_dir("view-env");
    let store = store_over(&dir);

    let mut settings = default_settings();
    // The catalog declares MOCK_API_KEY, but the view never reads ambient
    // env (a cached answer must not depend on state that moves no
    // revision — readiness reads env itself).
    settings
        .providers
        .insert("mock".to_owned(), provider_state(&[], &[], None));
    settings.provider_order = vec!["mock".to_owned()];

    let view = store.view(&settings, "s1");
    assert!(!view.providers[0].credentialed);
}

#[test]
fn the_view_follows_both_revisions() {
    let dir = temp_data_dir("view-revisions");
    let store = store_over(&dir);

    let mut settings = default_settings();
    settings.providers.insert(
        "mock".to_owned(),
        provider_state(&["k"], &["mock-large"], None),
    );
    settings.provider_order = vec!["mock".to_owned()];

    let first = store.view(&settings, "s1");
    let again = store.view(&settings, "s1");
    assert!(
        Arc::ptr_eq(&first, &again),
        "the same revisions answer the same cached copy"
    );

    // A settings revision move rebuilds.
    let mut changed = settings.clone();
    changed.providers.get_mut("mock").unwrap().models =
        Some(vec!["mock-large".to_owned(), "mock-mini".to_owned()]);
    let second = store.view(&changed, "s2");
    assert!(
        !Arc::ptr_eq(&first, &second),
        "settings moved: a fresh view"
    );
    assert_eq!(second.providers[0].models.len(), 2);

    // A catalog replacement rebuilds too, under the same settings revision.
    let mut payload = catalog_payload();
    payload["providers"]["mock"]["name"] = json!("Mock Renamed");
    write_catalog_snapshot(&dir, &payload, 1_700_000_001_000, 1_700_000_001_000).unwrap();
    let third = store.view(&changed, "s2");
    assert!(
        !Arc::ptr_eq(&second, &third),
        "the catalog moved: a fresh view"
    );
    assert_eq!(third.providers[0].name, "Mock Renamed");
}
