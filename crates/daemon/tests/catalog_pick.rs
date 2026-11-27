//! `catalog.listProviders` / `catalog.listModels` pins: the setup pickers read
//! the host's catalog, answer before any attachment, and never guess when the
//! catalog is missing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use openbutler_daemon::catalog::{CatalogStore, FetchFuture, Transport, write_catalog_snapshot};
use openbutler_daemon::dispatch::{DispatchState, Host, handle};
use openbutler_protocol::{DaemonRequest, DaemonResponse};
use serde_json::{Value, json};

/// The picker actions never fetch: a download is another slice's job.
struct NoNetwork;

impl Transport for NoNetwork {
    fn fetch(&self, _url: &'static str) -> FetchFuture {
        Box::pin(std::future::ready(
            Err("no network in this test".to_owned()),
        ))
    }
}

fn seeded(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ob-catalog-pick-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_catalog_snapshot(&dir, &payload(), 1_700_000_000_000)
        .expect("writer accepts the fixture");
    dir
}

fn payload() -> Value {
    json!({
        "providers": {
            "mock": {
                "name": "Mock Provider",
                "npm": "@ai-sdk/mock",
                "env": ["MOCK_API_KEY"],
                "models": {
                    "mock-large": {
                        "name": "Mock Large",
                        "limit": { "context": 200000, "output": 64000 }
                    },
                    "mock-small": { "tool_call": false }
                }
            },
            "other": {
                "name": "Other Provider",
                "models": { "other-model": {} }
            }
        }
    })
}

fn call(host: &Host, dir: &Path, action: &str, payload: Value) -> DaemonResponse {
    let mut state = DispatchState::new();
    let mut request = DaemonRequest::new("c1", action);
    request.payload = payload.as_object().cloned().unwrap_or_default();
    handle(host, dir, &mut state, &request)
}

fn result(response: DaemonResponse) -> Value {
    match response {
        DaemonResponse::Success { result, .. } => result,
        DaemonResponse::Failure { error, .. } => panic!("expected success, got: {error}"),
    }
}

fn error(response: DaemonResponse) -> String {
    match response {
        DaemonResponse::Failure { error, .. } => error,
        DaemonResponse::Success { result, .. } => panic!("expected a failure, got: {result}"),
    }
}

#[test]
fn providers_carry_id_name_and_model_count() {
    let dir = seeded("providers");
    let host = Host::new();
    host.set_catalog(Arc::new(CatalogStore::with_transport(
        &dir,
        Arc::new(NoNetwork),
    )));
    let listing = result(call(&host, &dir, "catalog.listProviders", json!({})));
    assert_eq!(listing["loaded"], true);
    assert_eq!(listing["loading"], false);
    let providers = listing["providers"].as_array().expect("providers array");
    assert_eq!(providers.len(), 2);
    let mock = providers
        .iter()
        .find(|row| row["id"] == "mock")
        .expect("mock is listed");
    assert_eq!(mock["name"], "Mock Provider");
    assert_eq!(mock["models"], 2);
    assert_eq!(mock["npm"], "@ai-sdk/mock");
    let other = providers
        .iter()
        .find(|row| row["id"] == "other")
        .expect("other is listed");
    assert_eq!(other["name"], "Other Provider");
    assert_eq!(other["models"], 1);
}

#[test]
fn models_carry_ids_names_and_declared_limits() {
    let dir = seeded("models");
    let host = Host::new();
    host.set_catalog(Arc::new(CatalogStore::with_transport(
        &dir,
        Arc::new(NoNetwork),
    )));
    let answer = result(call(
        &host,
        &dir,
        "catalog.listModels",
        json!({ "provider": "mock" }),
    ));
    assert_eq!(answer["provider"], "mock");
    let models = answer["models"].as_array().expect("models array");
    assert_eq!(models.len(), 2);
    let large = models
        .iter()
        .find(|row| row["id"] == "mock-large")
        .expect("mock-large is listed");
    assert_eq!(large["name"], "Mock Large");
    assert_eq!(large["context"], 200_000);
    assert_eq!(large["output"], 64_000);
    let small = models
        .iter()
        .find(|row| row["id"] == "mock-small")
        .expect("mock-small is listed");
    // A model with no declared name falls back to its id — never to a guess.
    assert_eq!(small["name"], "mock-small");
    assert!(small["context"].is_null());
}

#[test]
fn an_unknown_provider_is_named_not_invented() {
    let dir = seeded("unknown");
    let host = Host::new();
    host.set_catalog(Arc::new(CatalogStore::with_transport(
        &dir,
        Arc::new(NoNetwork),
    )));
    let message = error(call(
        &host,
        &dir,
        "catalog.listModels",
        json!({ "provider": "nope" }),
    ));
    assert_eq!(
        message,
        "the catalog does not carry a provider called nope."
    );
}

#[test]
fn a_missing_provider_field_is_refused() {
    let dir = seeded("missing");
    let host = Host::new();
    host.set_catalog(Arc::new(CatalogStore::with_transport(
        &dir,
        Arc::new(NoNetwork),
    )));
    let message = error(call(&host, &dir, "catalog.listModels", json!({})));
    assert!(message.contains("provider"), "{message}");
}

#[test]
fn no_catalog_answers_not_loaded_instead_of_empty() {
    let dir = std::env::temp_dir().join(format!("ob-catalog-pick-empty-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let host = Host::new();
    let listing = result(call(&host, &dir, "catalog.listProviders", json!({})));
    assert_eq!(listing["loaded"], false);
    assert_eq!(listing["providers"].as_array().map(Vec::len), Some(0));
    let message = error(call(
        &host,
        &dir,
        "catalog.listModels",
        json!({ "provider": "mock" }),
    ));
    assert_eq!(message, "the catalog is not available on this host yet.");
}
