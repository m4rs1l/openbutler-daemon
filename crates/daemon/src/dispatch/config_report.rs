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
use crate::shape::{kv_line, screen_line, section_line, table_lines};

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

    let mut report = json!({
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
    });
    // The screen, composed here (the source's `runConfig` layout, L169-213):
    // the client prints these lines and makes no decision.
    report["lines"] = Value::Array(render(&report));
    Ok(report)
}

/// The manager screen as finished lines: the MCP block, the two posture rows,
/// the provider table with its default block, then readiness.
fn render(report: &Value) -> Vec<Value> {
    let mut lines: Vec<Value> = Vec::new();

    lines.push(screen_line("plain", ""));
    lines.push(section_line("MCP servers"));
    let mcp = report["mcpServers"].as_array().cloned().unwrap_or_default();
    if mcp.is_empty() {
        lines.push(screen_line("info", "No MCP servers configured."));
    } else {
        let rows: Vec<Vec<String>> = mcp
            .iter()
            .map(|row| {
                vec![
                    row["id"].as_str().unwrap_or("").to_owned(),
                    row["transport"].as_str().unwrap_or("").to_owned(),
                    row["endpoint"].as_str().unwrap_or("").to_owned(),
                ]
            })
            .collect();
        lines.extend(table_lines(&["SERVER", "TRANSPORT", "ENDPOINT"], &rows));
    }
    lines.push(screen_line(
        "info",
        "Manage global servers with openbutler mcp. Credentials are environment-variable references, not saved values.",
    ));
    lines.push(screen_line("plain", ""));

    let workspace = report["defaultWorkspace"]
        .as_str()
        .unwrap_or("(choose when opening OpenButler)");
    lines.push(kv_line("Default workspace", workspace));
    lines.push(kv_line(
        "Tool access",
        report["toolAccess"].as_str().unwrap_or(""),
    ));

    if !report["hasProviders"].as_bool().unwrap_or(false) {
        lines.push(screen_line("plain", ""));
        lines.push(section_line("Providers"));
        lines.push(screen_line("plain", ""));
        lines.push(screen_line("warn", "No provider is configured."));
        lines.push(screen_line("info", "Set one up: openbutler setup"));
        return lines;
    }

    lines.push(screen_line("plain", ""));
    lines.push(section_line("Providers"));
    let providers = report["providers"].as_array().cloned().unwrap_or_default();
    let rows: Vec<Vec<String>> = providers
        .iter()
        .map(|row| {
            let id = row["id"].as_str().unwrap_or("");
            let tools = match row["toolCapableCount"].as_u64() {
                Some(count) => count.to_string(),
                None => "unknown".to_owned(),
            };
            vec![
                if row["isDefault"].as_bool().unwrap_or(false) {
                    format!("{id} *")
                } else {
                    id.to_owned()
                },
                if row["enabled"].as_bool().unwrap_or(false) {
                    "on"
                } else {
                    "off"
                }
                .to_owned(),
                row["keyCount"].as_u64().unwrap_or(0).to_string(),
                tools,
                row["defaultModel"].as_str().unwrap_or("").to_owned(),
                if row["custom"].as_bool().unwrap_or(false) {
                    "custom"
                } else {
                    "catalog"
                }
                .to_owned(),
            ]
        })
        .collect();
    lines.extend(table_lines(
        &["PROVIDER", "STATE", "KEYS", "TOOLS", "DEFAULT", "KIND"],
        &rows,
    ));
    lines.push(screen_line("plain", ""));

    let default_line = match report["default"].as_object() {
        Some(choice) => format!(
            "{} / {}{}",
            choice["providerId"].as_str().unwrap_or(""),
            choice["modelId"].as_str().unwrap_or(""),
            if choice["source"].as_str() == Some("usage") {
                " (from usage)"
            } else {
                ""
            }
        ),
        None => "(none)".to_owned(),
    };
    lines.push(kv_line("Default", &default_line));
    lines.push(kv_line(
        "Reasoning",
        report["reasoning"].as_str().unwrap_or(""),
    ));
    lines.push(kv_line(
        "Show reasoning",
        if report["showReasoning"].as_bool().unwrap_or(false) {
            "on"
        } else {
            "off"
        },
    ));
    lines.push(screen_line("plain", ""));

    if report["readiness"]["ready"].as_bool().unwrap_or(false) {
        lines.push(screen_line("success", "Ready to run."));
    } else if let Some(problems) = report["readiness"]["problems"].as_array() {
        for problem in problems {
            lines.push(screen_line("failure", problem.as_str().unwrap_or("")));
        }
    }
    if let Some(hints) = report["readiness"]["hints"].as_array() {
        for hint in hints {
            lines.push(screen_line("info", hint.as_str().unwrap_or("")));
        }
    }
    if report["catalogLoading"].as_bool().unwrap_or(false)
        && !report["catalogLoaded"].as_bool().unwrap_or(false)
    {
        lines.push(screen_line(
            "info",
            "The catalog is downloading in the background; run this again when it finishes.",
        ));
    }
    if report["default"].is_null() {
        lines.push(screen_line(
            "info",
            "Change the default model: openbutler setup",
        ));
    }
    lines
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn texts(lines: &[Value]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line["text"].as_str().unwrap().to_owned())
            .collect()
    }

    /// The manager screen is composed here, so its layout is pinned here: the
    /// section bars, the two posture rows, the provider table and the default
    /// block, in the source's order.
    #[test]
    fn the_screen_lays_out_every_line_like_the_source() {
        let report = json!({
            "mcpServers": [
                { "id": "local", "transport": "stdio", "endpoint": "npx -y server-files" }
            ],
            "defaultWorkspace": "C:\\work\\proj",
            "toolAccess": "Workspace (default)",
            "hasProviders": true,
            "catalogLoaded": true,
            "catalogLoading": false,
            "providers": [
                {
                    "id": "mock", "enabled": true, "keyCount": 2,
                    "toolCapableCount": 3, "isDefault": true, "custom": true,
                    "url": "http://127.0.0.1:9/v1", "defaultModel": "test-model"
                },
                {
                    "id": "other", "enabled": false, "keyCount": 0,
                    "toolCapableCount": null, "isDefault": false, "custom": false,
                    "url": "", "defaultModel": ""
                }
            ],
            "default": { "providerId": "mock", "modelId": "test-model", "source": "settings" },
            "reasoning": "default",
            "showReasoning": true,
            "readiness": { "ready": true, "problems": [], "hints": [] }
        });
        let lines = render(&report);
        let got = texts(&lines);
        let tones: Vec<&str> = lines
            .iter()
            .map(|line| line["tone"].as_str().unwrap())
            .collect();

        assert_eq!(got[0], "");
        assert!(got[1].starts_with("\u{2500}\u{2500} MCP servers "));
        assert_eq!(got[1].chars().count(), 57);
        assert_eq!(got[2].trim_end(), "  SERVER  TRANSPORT  ENDPOINT");
        assert!(got[3].starts_with("  \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}  "));
        assert_eq!(
            got[4].trim_end(),
            "  local   stdio      npx -y server-files"
        );
        assert_eq!(
            got[5],
            "Manage global servers with openbutler mcp. Credentials are environment-variable references, not saved values."
        );
        assert_eq!(lines[5]["tone"], json!("info"));
        assert_eq!(got[6], "");
        assert_eq!(got[7], "  Default workspace      C:\\work\\proj");
        assert_eq!(got[8], "  Tool access            Workspace (default)");
        assert_eq!(got[9], "");
        assert!(got[10].starts_with("\u{2500}\u{2500} Providers "));
        assert_eq!(
            got[11].trim_end(),
            "  PROVIDER  STATE  KEYS  TOOLS    DEFAULT     KIND"
        );
        assert_eq!(
            got[13].trim_end(),
            "  mock *    on     2     3        test-model  custom"
        );
        assert_eq!(
            got[14].trim_end(),
            "  other     off    0     unknown              catalog"
        );
        assert_eq!(got[16], "  Default                mock / test-model");
        assert_eq!(got[17], "  Reasoning              default");
        assert_eq!(got[18], "  Show reasoning         on");
        assert_eq!(got[20], "Ready to run.");
        assert_eq!(*tones.last().unwrap(), "success");
    }

    /// An empty machine stops at the setup hint, exactly like the source.
    #[test]
    fn an_empty_machine_stops_at_the_setup_hint() {
        let report = json!({
            "mcpServers": [],
            "defaultWorkspace": null,
            "toolAccess": "Full access (no permission prompts)",
            "hasProviders": false,
            "catalogLoaded": false,
            "catalogLoading": false,
            "providers": [],
            "default": null,
            "reasoning": "default",
            "showReasoning": true,
            "readiness": { "ready": false, "problems": [], "hints": [] }
        });
        let lines = render(&report);
        let got = texts(&lines);
        assert_eq!(got[2], "No MCP servers configured.");
        assert_eq!(
            got[5],
            "  Default workspace      (choose when opening OpenButler)"
        );
        assert_eq!(got[10], "No provider is configured.");
        assert_eq!(got[11], "Set one up: openbutler setup");
        assert_eq!(lines[10]["tone"], json!("warn"));
        assert!(!got.iter().any(|line| line.contains("Reasoning")));
    }
}
