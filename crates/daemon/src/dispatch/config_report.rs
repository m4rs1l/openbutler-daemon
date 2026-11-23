//! `config.report`: the manager screen's whole state, computed here.
//!
//! Sources (read 2026-11-24): `cli-main.ts` `runConfig` (L169-213) and
//! `showMcpServers` (L380-396), `config-edit.ts` `summarizeProviders`
//! (L50-67) and `readiness` (L94-123), `cli-main.ts` `formatApprovalMode`
//! (L334-338) and `describeReasoningPreference` (L158-167).
//!
//! The daemon answers with decisions — labels, counts, sentences — and the
//! CLI only places them on the page. Payload: none.
//!
//! Deliberate divergences, each one recorded in the CLI's surface doc:
//! 1. The source awaited the catalog download inside the command and died
//!    with it on a cold, offline first run. Dispatch here is synchronous
//!    and never blocks a connection on the network: the report *starts*
//!    the download (`catalogLoading`) and says so; a second run shows the
//!    counts.
//! 2. `openbutler --ask` became `openbutler run` (owner ruling), so the
//!    no-default hint names `run`.
//! 3. MCP servers list in sorted id order — our store is a `BTreeMap`; the
//!    source listed YAML insertion order. Provider rows do follow the
//!    stored `providerOrder`, the source's insertion order.
//! 4. The reasoning description names only variants this build can
//!    express (`default`): the adapter layer that maps effort levels onto
//!    provider options is the runtime slice's work, so the
//!    `(this model: …)` suffix is always `default` until it lands. The
//!    stored preference is still shown as stored.

use std::path::Path;

use serde_json::{Value, json};

use super::Host;
use crate::catalog::CatalogStore;
use crate::default_model::{USAGE_WINDOW_MS, provider_is_configured, resolve_default_model};
use crate::settings::schema::{ApprovalMode, McpServerState, SettingsStore};

/// Builds the whole report. `Err` carries a client-visible sentence.
pub fn report(host: &Host, data_dir: &Path) -> Result<Value, String> {
    let settings = host.settings();
    let Some(loaded) = settings.as_ref() else {
        return Err("settings are not loaded".to_owned());
    };
    let store: &SettingsStore = &loaded.store;
    let catalog = host.catalog();
    let catalog_ref = catalog.as_deref();
    let catalog_loaded = catalog_ref.is_some_and(CatalogStore::is_loaded);
    let catalog_loading = if catalog_loaded {
        false
    } else {
        host.warm_catalog()
    };
    // The usage window feeds the default fallback; an unreadable ledger is
    // the same as an empty one (the source opens its read-only copy the
    // same best-effort way).
    let rows = crate::ledger::usage_pick_rows(data_dir, crate::ledger::now_ms() - USAGE_WINDOW_MS)
        .unwrap_or_default();

    let mcp_servers: Vec<Value> = store
        .mcp_servers
        .iter()
        .map(|(id, server)| {
            let (transport, endpoint) = match server {
                McpServerState::Stdio { command, args, .. } => (
                    "stdio",
                    std::iter::once(command.clone())
                        .chain(args.iter().cloned())
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
                McpServerState::Http { transport, url, .. } => (*transport, url.clone()),
            };
            json!({ "id": id, "transport": transport, "endpoint": endpoint })
        })
        .collect();

    let mut providers: Vec<Value> = Vec::new();
    for id in &store.provider_order {
        let Some(state) = store.providers.get(id) else {
            continue;
        };
        let custom = state.npm.is_some() || state.url.is_some();
        // `listCatalogModels(…).filter(canUseTools)` (config-edit.ts L54):
        // a count only when the catalog is in hand and the provider is not
        // custom; `null` says "there is no list to count", never zero.
        let tool_capable_count = if catalog_loaded && !custom {
            let count = catalog_ref
                .and_then(|catalog| catalog.get_provider(id))
                .map(|provider| {
                    // A model counts when `toolCallMode` is `native`; with the
                    // catalog entry in hand that is exactly `tool_call: true`
                    // (registry.ts L943-949).
                    provider
                        .models
                        .values()
                        .filter(|model| model.tool_call == Some(true))
                        .count()
                })
                .unwrap_or(0);
            Some(count)
        } else {
            None
        };
        let is_default = store.default_provider == *id;
        let default_model = if is_default {
            if store.default_model.is_empty() {
                "(none)".to_owned()
            } else {
                store.default_model.clone()
            }
        } else {
            String::new()
        };
        providers.push(json!({
            "id": id,
            "enabled": state.enabled,
            "keyCount": state.keys.len(),
            "toolCapableCount": tool_capable_count,
            "isDefault": is_default,
            "ready": state.enabled && provider_is_configured(id, Some(state), catalog_ref),
            "custom": custom,
            "url": state.url.clone().unwrap_or_default(),
            "defaultModel": default_model,
        }));
    }

    let default_choice = resolve_default_model(store, catalog_ref, &rows);
    let default_json = match &default_choice {
        Some(choice) => json!({
            "providerId": choice.provider_id,
            "modelId": choice.model_id,
            "source": choice.source.as_str(),
        }),
        None => Value::Null,
    };

    let readiness = readiness(store, catalog_ref, catalog_loaded, default_choice.as_ref());

    Ok(json!({
        "mcpServers": mcp_servers,
        "defaultWorkspace": store.default_workspace_root,
        "toolAccess": format_approval_mode(store.approval_mode),
        "hasProviders": !store.providers.is_empty(),
        "catalogLoaded": catalog_loaded,
        "catalogLoading": catalog_loading,
        "providers": providers,
        "default": default_json,
        "reasoning": describe_reasoning(store),
        "showReasoning": store.show_reasoning != Some(false),
        "readiness": readiness,
    }))
}

/// `formatApprovalMode` (cli-main.ts L334): the human label for the mode.
fn format_approval_mode(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::AutoReview => "Approve for me (Auto-review outside workspace)",
        ApprovalMode::FullAccess => "Full access (no permission prompts)",
        ApprovalMode::Workspace => "Workspace (default)",
    }
}

/// `describeReasoningPreference` (cli-main.ts L158): the stored preference,
/// with the variant this model actually uses when they differ.
///
/// The variant list this build can express is `default` alone (no adapter
/// layer yet), so the effective variant is always `default`; the stored
/// value is echoed as stored, exactly like the source.
fn describe_reasoning(store: &SettingsStore) -> String {
    let stored = normalize_reasoning_variant(store.reasoning.as_deref().unwrap_or("default"));
    let resolves = !store.default_provider.is_empty()
        && !store.default_model.is_empty()
        && store.providers.contains_key(&store.default_provider);
    if !resolves {
        return stored;
    }
    let effective = "default";
    if effective == stored {
        stored
    } else {
        format!("{stored} (this model: {effective})")
    }
}

/// `normalizeReasoningVariantId` (registry.ts L739): `auto` was the implicit
/// variant before it became `default`; stored files may still say `auto`.
fn normalize_reasoning_variant(value: &str) -> String {
    if value == "auto" {
        "default".to_owned()
    } else {
        value.to_owned()
    }
}

/// `readiness` (config-edit.ts L94-123): what still stands between this
/// configuration and a working run.
fn readiness(
    store: &SettingsStore,
    catalog: Option<&CatalogStore>,
    catalog_loaded: bool,
    default_choice: Option<&crate::default_model::DefaultModelChoice>,
) -> Value {
    let mut problems: Vec<String> = Vec::new();
    let mut hints: Vec<String> = Vec::new();

    let ready_ids: Vec<&String> = store
        .providers
        .iter()
        .filter(|(id, state)| state.enabled && provider_is_configured(id, Some(state), catalog))
        .map(|(id, _)| id)
        .collect();
    if ready_ids.is_empty() {
        problems.push("No LLM provider is enabled with an API key.".to_owned());
        hints.push("Configure a provider and key: openbutler setup".to_owned());
        return json!({ "ready": problems.is_empty(), "problems": problems, "hints": hints });
    }

    let custom_configured = ready_ids.iter().any(|id| {
        store
            .providers
            .get(*id)
            .is_some_and(|state| state.npm.is_some() || state.url.is_some())
    });
    if !catalog_loaded && !custom_configured {
        problems
            .push("The model catalog has not been loaded, so no model can be verified.".to_owned());
        hints.push("Load it: openbutler config  (needs network or a warm cache)".to_owned());
    }

    match default_choice {
        None => {
            hints.push("No default model is selected, so openbutler run will not start.".to_owned())
        }
        Some(choice) if choice.source == crate::default_model::DefaultSource::Usage => {
            hints.push(format!(
                "Default model comes from usage: {} / {}.",
                choice.provider_id, choice.model_id
            ))
        }
        Some(_) => {}
    }

    json!({ "ready": problems.is_empty(), "problems": problems, "hints": hints })
}
