//! `config.report` pins: the manager screen's whole state computed by the
//! daemon — MCP rows, the two kv lines, the provider table (order, kind,
//! keys, tool counts), the default chain, the reasoning description, and
//! readiness.
//!
//! Sources: `cli-main.ts` `runConfig` (L169-213) / `showMcpServers`
//! (L380-396) / `formatApprovalMode` (L334) / `describeReasoningPreference`
//! (L158), `config-edit.ts` `summarizeProviders` (L50) and `readiness`
//! (L94-123), `usage/default-model.ts` (`resolveDefaultModel`), read
//! 2026-11-24.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::temp_data_dir;
use openbutler_daemon::catalog::{CatalogStore, FetchFuture, Transport, write_catalog_snapshot};
use openbutler_daemon::dispatch::{DispatchState, Host, handle};
use openbutler_daemon::ledger::{LedgerWriter, StepUsageReport, UsageInput, UsageTokens};
use openbutler_protocol::{DaemonRequest, DaemonResponse};
use serde_json::{Value, json};

/// A transport that must never be reached by these tests.
struct DeadTransport;

impl Transport for DeadTransport {
    fn fetch(&self, _url: &'static str) -> FetchFuture {
        Box::pin(async { Err("no network in config tests".to_owned()) })
    }
}

/// One real download's worth of providers, shaped like models.dev.
fn catalog_payload() -> Value {
    json!({
        "providers": {
            "mock": {
                "name": "Mock Provider",
                "npm": "@ai-sdk/mock",
                "env": ["MOCK_API_KEY"],
                "models": {
                    "tool-a": { "name": "Tool A", "tool_call": true },
                    "tool-b": { "name": "Tool B", "tool_call": true },
                    "no-tools": { "name": "No Tools", "tool_call": false }
                }
            }
        }
    })
}

/// A transport that answers once with the payload and counts its calls.
struct CountingTransport {
    calls: Arc<std::sync::atomic::AtomicUsize>,
    payload: Vec<u8>,
}

impl Transport for CountingTransport {
    fn fetch(&self, _url: &'static str) -> FetchFuture {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let payload = self.payload.clone();
        Box::pin(async move { Ok(payload) })
    }
}

fn write_settings(dir: &Path, yaml: &str) {
    std::fs::write(dir.join("settings.yaml"), yaml).unwrap();
}

fn request() -> DaemonRequest {
    DaemonRequest::new("r1", "config.report")
}

fn call(host: &Host, dir: &Path, state: &mut DispatchState) -> DaemonResponse {
    handle(host, dir, state, &request())
}

fn report_of(response: DaemonResponse) -> Value {
    let DaemonResponse::Success { result, .. } = response else {
        panic!("expected success, got {response:?}");
    };
    result
}

fn host_over(dir: &Path) -> Host {
    let host = Host::new();
    host.reload_settings(dir).expect("settings load");
    host
}

/// One usage row for `provider/model` at `at_ms`, written through the
/// ledger's own writer (the table is the contract).
fn seed_usage(dir: &Path, provider: &str, model: &str, at_ms: i64) {
    let mut ledger = LedgerWriter::open(dir).expect("ledger opens");
    let usage = UsageInput {
        session_id: "11111111-1111-4111-8111-111111111111".to_owned(),
        event_id: None,
        run_kind: "chat",
        workspace_root: None,
        provider_id: provider.to_owned(),
        model_id: model.to_owned(),
        prices: None,
        report: StepUsageReport {
            started_at_ms: at_ms,
            ended_at_ms: at_ms + 10,
            attempt: 1,
            status: "ok",
            tokens: UsageTokens {
                input_tokens: Some(10),
                output_tokens: Some(5),
                total_tokens: Some(15),
                ..UsageTokens::default()
            },
            usage_raw: None,
            provider_metadata: None,
            response_id: None,
        },
    };
    ledger.record_usage(&usage.row()).expect("usage writes");
}

#[test]
fn mcp_rows_and_the_two_kv_lines_match_the_source() {
    let dir = temp_data_dir("cfg-mcp");
    let workspace = dir.to_string_lossy().into_owned();
    write_settings(
        &dir,
        &format!(
            "mcpServers:\n\
         \x20 local:\n\
         \x20   transport: stdio\n\
         \x20   command: npx\n\
         \x20   args:\n\
         \x20     - -y\n\
         \x20     - server-files\n\
         \x20 remote:\n\
         \x20   transport: sse\n\
         \x20   url: https://mcp.example/sse\n\
         defaultWorkspaceRoot: {}\n\
         approvalMode: auto-review\n",
            workspace
        ),
    );
    let host = host_over(&dir);
    let report = report_of(call(&host, &dir, &mut DispatchState::new()));

    let mcp = report["mcpServers"].as_array().unwrap();
    // Sorted id order (our store is a BTreeMap; documented divergence).
    assert_eq!(mcp[0]["id"], "local");
    assert_eq!(mcp[0]["transport"], "stdio");
    assert_eq!(
        mcp[0]["endpoint"], "npx -y server-files",
        "a stdio endpoint is command plus args, space-joined"
    );
    assert_eq!(mcp[1]["id"], "remote");
    assert_eq!(mcp[1]["transport"], "sse");
    assert_eq!(mcp[1]["endpoint"], "https://mcp.example/sse");
    assert_eq!(report["defaultWorkspace"], workspace.as_str());
    assert_eq!(
        report["toolAccess"],
        "Approve for me (Auto-review outside workspace)"
    );
}

#[test]
fn provider_rows_follow_the_stored_order_and_carry_kind_and_keys() {
    let dir = temp_data_dir("cfg-rows");
    write_settings(
        &dir,
        "providerOrder:\n\
         \x20 - second\n\
         \x20 - first\n\
         providers:\n\
         \x20 first:\n\
         \x20   enabled: true\n\
         \x20   url: http://127.0.0.1:9/v1\n\
         \x20 second:\n\
         \x20   enabled: false\n\
         defaultProvider: first\n\
         defaultModel: m1\n",
    );
    std::fs::write(
        dir.join("config.yaml"),
        "providers:\n  first:\n    keys:\n      - k1\n      - k2\n",
    )
    .unwrap();
    let host = host_over(&dir);
    let report = report_of(call(&host, &dir, &mut DispatchState::new()));

    let rows = report["providers"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["id"], "second", "providerOrder decides the rows");
    assert_eq!(rows[1]["id"], "first");

    assert_eq!(rows[1]["keyCount"], 2);
    assert_eq!(rows[1]["custom"], true, "a url makes a provider custom");
    assert_eq!(rows[1]["url"], "http://127.0.0.1:9/v1");
    assert_eq!(rows[1]["isDefault"], true);
    assert_eq!(rows[1]["defaultModel"], "m1");
    assert_eq!(rows[1]["ready"], true);

    assert_eq!(rows[0]["enabled"], false);
    assert_eq!(rows[0]["custom"], false);
    assert_eq!(rows[0]["isDefault"], false);
    assert_eq!(
        rows[0]["defaultModel"], "",
        "only the default row names a model"
    );
    assert_eq!(rows[0]["ready"], false, "disabled is never ready");
}

#[test]
fn without_a_catalog_the_tool_count_is_unknown_not_zero() {
    let dir = temp_data_dir("cfg-unknown");
    write_settings(&dir, "providers:\n  mock:\n    enabled: true\n");
    std::fs::write(
        dir.join("config.yaml"),
        "providers:\n  mock:\n    keys:\n      - k\n",
    )
    .unwrap();
    let host = host_over(&dir);
    let report = report_of(call(&host, &dir, &mut DispatchState::new()));

    assert_eq!(report["catalogLoaded"], false);
    let rows = report["providers"].as_array().unwrap();
    assert!(
        rows[0]["toolCapableCount"].is_null(),
        "no catalog means no list to count: null, never zero"
    );
}

#[test]
fn a_loaded_catalog_counts_tool_capable_models_for_catalog_providers_only() {
    let dir = temp_data_dir("cfg-counts");
    write_catalog_snapshot(&dir, &catalog_payload(), 1_700_000_000_000).unwrap();
    write_settings(
        &dir,
        "providers:\n\
         \x20 mock:\n\
         \x20   enabled: true\n\
         \x20 local:\n\
         \x20   enabled: true\n\
         \x20   url: http://127.0.0.1:9/v1\n",
    );
    std::fs::write(
        dir.join("config.yaml"),
        "providers:\n  mock:\n    keys:\n      - k\n  local:\n    keys:\n      - k\n",
    )
    .unwrap();
    let host = host_over(&dir);
    host.set_catalog(Arc::new(CatalogStore::with_transport(
        &dir,
        Arc::new(DeadTransport),
    )));
    let report = report_of(call(&host, &dir, &mut DispatchState::new()));

    assert_eq!(report["catalogLoaded"], true);
    let rows = report["providers"].as_array().unwrap();
    let mock = rows.iter().find(|row| row["id"] == "mock").unwrap();
    let local = rows.iter().find(|row| row["id"] == "local").unwrap();
    assert_eq!(
        mock["toolCapableCount"], 2,
        "two of the three catalog models declare tool_call"
    );
    assert!(
        local["toolCapableCount"].is_null(),
        "a custom provider has no catalog list to count"
    );
}

#[test]
fn no_provider_is_the_source_refusal_pair() {
    let dir = temp_data_dir("cfg-empty");
    write_settings(&dir, "reasoning: high\n");
    let host = host_over(&dir);
    let report = report_of(call(&host, &dir, &mut DispatchState::new()));

    assert_eq!(report["hasProviders"], false);
    assert_eq!(report["readiness"]["ready"], false);
    assert_eq!(
        report["readiness"]["problems"],
        json!(["No LLM provider is enabled with an API key."])
    );
    assert_eq!(
        report["readiness"]["hints"],
        json!(["Configure a provider and key: openbutler setup"])
    );
    assert!(report["default"].is_null());
}

#[test]
fn a_configured_provider_without_a_catalog_still_owes_the_catalog() {
    let dir = temp_data_dir("cfg-catalog-gap");
    write_settings(&dir, "providers:\n  mock:\n    enabled: true\n");
    std::fs::write(
        dir.join("config.yaml"),
        "providers:\n  mock:\n    keys:\n      - k\n",
    )
    .unwrap();
    let host = host_over(&dir);
    let report = report_of(call(&host, &dir, &mut DispatchState::new()));

    assert_eq!(report["readiness"]["ready"], false);
    assert_eq!(
        report["readiness"]["problems"],
        json!(["The model catalog has not been loaded, so no model can be verified."])
    );
    assert_eq!(
        report["readiness"]["hints"][0],
        "Load it: openbutler config  (needs network or a warm cache)"
    );
    assert_eq!(
        report["readiness"]["hints"][1],
        "No default model is selected, so openbutler run will not start."
    );
}

#[test]
fn a_custom_provider_with_a_default_pair_is_ready() {
    let dir = temp_data_dir("cfg-ready");
    write_settings(
        &dir,
        "providers:\n\
         \x20 mock:\n\
         \x20   enabled: true\n\
         \x20   url: http://127.0.0.1:9/v1\n\
         defaultProvider: mock\n\
         defaultModel: test-model\n",
    );
    std::fs::write(
        dir.join("config.yaml"),
        "providers:\n  mock:\n    keys:\n      - k\n",
    )
    .unwrap();
    let host = host_over(&dir);
    let report = report_of(call(&host, &dir, &mut DispatchState::new()));

    assert_eq!(report["readiness"]["ready"], true);
    assert_eq!(report["readiness"]["problems"], json!([]));
    assert_eq!(report["default"]["providerId"], "mock");
    assert_eq!(report["default"]["modelId"], "test-model");
    assert_eq!(report["default"]["source"], "settings");
}

#[test]
fn an_unset_default_falls_back_to_the_most_used_usable_model() {
    let dir = temp_data_dir("cfg-usage");
    write_settings(
        &dir,
        "providers:\n\
         \x20 mock:\n\
         \x20   enabled: true\n\
         \x20   url: http://127.0.0.1:9/v1\n",
    );
    std::fs::write(
        dir.join("config.yaml"),
        "providers:\n  mock:\n    keys:\n      - k\n",
    )
    .unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    seed_usage(&dir, "mock", "seldom", now - 30_000);
    seed_usage(&dir, "mock", "favoured", now - 20_000);
    seed_usage(&dir, "mock", "favoured", now - 10_000);
    let host = host_over(&dir);
    let report = report_of(call(&host, &dir, &mut DispatchState::new()));

    assert_eq!(report["default"]["modelId"], "favoured");
    assert_eq!(report["default"]["source"], "usage");
    assert!(
        report["readiness"]["hints"]
            .as_array()
            .unwrap()
            .iter()
            .any(|hint| hint == "Default model comes from usage: mock / favoured."),
        "the usage-sourced hint names the pair: {:?}",
        report["readiness"]["hints"]
    );
}

#[test]
fn an_explicit_default_beats_recorded_usage() {
    let dir = temp_data_dir("cfg-explicit");
    write_settings(
        &dir,
        "providers:\n\
         \x20 mock:\n\
         \x20   enabled: true\n\
         \x20   url: http://127.0.0.1:9/v1\n\
         defaultProvider: mock\n\
         defaultModel: chosen\n",
    );
    std::fs::write(
        dir.join("config.yaml"),
        "providers:\n  mock:\n    keys:\n      - k\n",
    )
    .unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    seed_usage(&dir, "mock", "popular", now - 5_000);
    seed_usage(&dir, "mock", "popular", now - 4_000);
    let host = host_over(&dir);
    let report = report_of(call(&host, &dir, &mut DispatchState::new()));

    assert_eq!(report["default"]["modelId"], "chosen");
    assert_eq!(report["default"]["source"], "settings");
}

#[test]
fn the_reasoning_line_normalizes_auto_and_names_the_effective_variant() {
    let dir = temp_data_dir("cfg-reasoning");
    write_settings(
        &dir,
        "providers:\n\
         \x20 mock:\n\
         \x20   enabled: true\n\
         \x20   url: http://127.0.0.1:9/v1\n\
         reasoning: auto\n\
         showReasoning: false\n",
    );
    std::fs::write(
        dir.join("config.yaml"),
        "providers:\n  mock:\n    keys:\n      - k\n",
    )
    .unwrap();
    let host = host_over(&dir);
    let report = report_of(call(&host, &dir, &mut DispatchState::new()));
    assert_eq!(report["reasoning"], "default", "auto normalizes to default");
    assert_eq!(report["showReasoning"], false);

    // With a resolvable default pair, a stored effort echoes itself plus the
    // variant this build can express.
    write_settings(
        &dir,
        "providers:\n\
         \x20 mock:\n\
         \x20   enabled: true\n\
         \x20   url: http://127.0.0.1:9/v1\n\
         reasoning: high\n\
         defaultProvider: mock\n\
         defaultModel: m\n",
    );
    host.reload_settings(&dir).unwrap();
    let report = report_of(call(&host, &dir, &mut DispatchState::new()));
    assert_eq!(report["reasoning"], "high (this model: default)");
    assert_eq!(report["showReasoning"], true, "absent means on");
}

#[test]
fn a_read_only_attachment_may_read_the_report() {
    let dir = temp_data_dir("cfg-viewonly");
    write_settings(&dir, "providers:\n  mock:\n    enabled: true\n");
    std::fs::write(
        dir.join("config.yaml"),
        "providers:\n  mock:\n    keys:\n      - k\n",
    )
    .unwrap();
    let host = host_over(&dir);
    let mut state = DispatchState::new();
    state.binding_key = Some("/ob/workspace".to_owned());
    state.read_only = true;
    let report = report_of(call(&host, &dir, &mut state));
    assert_eq!(report["hasProviders"], true);
}

#[tokio::test]
async fn the_first_report_starts_the_download_and_the_next_one_sees_it() {
    let dir = temp_data_dir("cfg-warm");
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let transport = CountingTransport {
        calls: Arc::clone(&calls),
        payload: serde_json::to_vec(&catalog_payload()).unwrap(),
    };
    write_settings(&dir, "providers:\n  mock:\n    enabled: true\n");
    std::fs::write(
        dir.join("config.yaml"),
        "providers:\n  mock:\n    keys:\n      - k\n",
    )
    .unwrap();
    let store = Arc::new(CatalogStore::with_transport(&dir, Arc::new(transport)));
    let host = host_over(&dir);
    host.set_catalog(Arc::clone(&store));

    let first = report_of(call(&host, &dir, &mut DispatchState::new()));
    assert_eq!(first["catalogLoaded"], false);
    assert_eq!(
        first["catalogLoading"], true,
        "the first report starts the download and says so"
    );

    let mut loaded = false;
    for _ in 0..200 {
        if store.is_loaded() {
            loaded = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(loaded, "the background download finishes");
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "exactly one download, never one per report"
    );

    let second = report_of(call(&host, &dir, &mut DispatchState::new()));
    assert_eq!(second["catalogLoaded"], true);
    let rows = second["providers"].as_array().unwrap();
    assert_eq!(rows[0]["toolCapableCount"], 2);
}

#[test]
fn a_fresh_draft_starts_on_the_usage_default() {
    let dir = temp_data_dir("cfg-draft");
    write_settings(
        &dir,
        "providers:\n\
         \x20 mock:\n\
         \x20   enabled: true\n\
         \x20   url: http://127.0.0.1:9/v1\n",
    );
    std::fs::write(
        dir.join("config.yaml"),
        "providers:\n  mock:\n    keys:\n      - k\n",
    )
    .unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    seed_usage(&dir, "mock", "remembered", now - 1_000);
    let host = host_over(&dir);
    let mut state = DispatchState::new();
    state.binding_key = Some("/ob/workspace".to_owned());
    let response = handle(
        &host,
        &dir,
        &mut state,
        &DaemonRequest::new("r2", "session.new").with("workspaceKey", json!("/ob/workspace")),
    );
    assert!(matches!(response, DaemonResponse::Success { .. }));

    let id = state
        .current_session()
        .expect("the draft is current")
        .to_owned();
    let path: PathBuf = openbutler_daemon::state::record_path(&dir, &id).unwrap();
    let record = openbutler_daemon::state::read_session_record(&path, &id).unwrap();
    assert_eq!(
        record.meta.provider_id, "mock",
        "the screen and a run agree on the default"
    );
    assert_eq!(record.meta.model_id, "remembered");
}
