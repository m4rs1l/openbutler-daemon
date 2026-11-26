//! `keys.list` / `keys.add` / `keys.remove`: the provider key store.
//!
//! Source: `cli/keys.ts` (read 2026-11-26) — `describeProviderKeys`,
//! `classifyKeysTarget`, `addProviderKeys`, `removeProviderKeys`. The
//! source read and wrote `config.yaml` from the CLI process; the store is
//! the daemon's here, so `keys list` answers display fragments (…tails,
//! never a key) and `keys add`/`keys remove` are the only writers — and
//! they are **local-only**, the same credential contract as `token.create`
//! and `pair.create` (a new gate: the source had none).
//!
//! Keys arrive in the payload because the masked prompt runs where the
//! terminal is; they are never echoed back — the answer carries a count.
//! The write goes through the secrets document (`config.yaml` is built
//! from it, not re-read), the same path `settings.setup` uses.

use std::collections::BTreeSet;
use std::path::Path;

use openbutler_protocol::DaemonRequest;
use serde_json::{Value, json};

use super::Host;
use crate::settings::schema::{ProviderSecrets, ProviderState};
use crate::shape::js_len;

/// One key's size gate — anything longer is not a credential.
const MAX_KEY_BYTES: usize = 8 * 1024;
/// One call may carry this many keys (`askKeys` builds them one by one).
const MAX_KEYS_PER_CALL: usize = 32;
/// Provider ids are short slugs; the settings schema caps them at 128.
const MAX_PROVIDER_BYTES: usize = 128;
/// The store is managed on the host machine only (new contract).
pub const REMOTE_REFUSAL: &str = "API keys are managed on the host machine only.";

/// One row per configured provider, in `providerOrder` then id order —
/// `describeProviderKeys`, with `…tail` display fragments.
pub fn list(host: &Host) -> Result<Value, String> {
    let settings = host.settings();
    let Some(loaded) = settings.as_ref() else {
        return Err("settings are not loaded".to_owned());
    };
    let store = &loaded.store;
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut rows: Vec<Value> = Vec::new();
    for id in store.provider_order.iter().chain(store.providers.keys()) {
        if !seen.insert(id.clone()) {
            continue;
        }
        let Some(state) = store.providers.get(id) else {
            continue;
        };
        rows.push(json!({
            "id": id,
            "enabled": state.enabled,
            "keyCount": state.keys.len(),
            "tails": state.keys.iter().map(|key| format!("…{}", tail(key))).collect::<Vec<_>>(),
        }));
    }
    Ok(json!({ "providers": rows }))
}

/// Appends keys to a provider (`addProviderKeys`): trimmed, empties
/// dropped, duplicates skipped, the entry created enabled when missing.
pub fn add(host: &Host, data_dir: &Path, request: &DaemonRequest) -> Result<Value, String> {
    let provider = super::require(request.get("provider"), "provider", MAX_PROVIDER_BYTES)?;
    let keys = payload_keys(request)?;
    classify(host, &provider)?;
    let normalized: Vec<String> = keys
        .into_iter()
        .map(|key| key.trim().to_owned())
        .filter(|key| !key.is_empty())
        .collect();
    if normalized.is_empty() {
        return Ok(json!({ "provider": provider, "added": 0 }));
    }
    let mut loaded = crate::settings::load_settings(data_dir).map_err(|error| error.to_string())?;
    let mut secrets = crate::settings::load_secrets(data_dir).map_err(|error| error.to_string())?;
    let entry = secrets
        .providers
        .entry(provider.clone())
        .or_insert(ProviderSecrets { keys: Vec::new() });
    let mut merged = entry.keys.clone();
    for key in normalized {
        if !merged.contains(&key) {
            merged.push(key);
        }
    }
    let added = merged.len() - entry.keys.len();
    entry.keys = merged.clone();
    // The store mirrors the keys in memory (the write strips them from
    // settings.yaml); a fresh provider entry starts enabled.
    let existing = loaded.store.providers.get(&provider).cloned();
    let mut state = existing.unwrap_or(ProviderState {
        enabled: true,
        keys: Vec::new(),
        models: None,
        url: None,
        npm: None,
        options: None,
        routing: None,
        model_routing: None,
        payload: None,
        model_specs: None,
    });
    state.keys = merged;
    loaded.store.providers.insert(provider.clone(), state);
    if !loaded.store.provider_order.contains(&provider) {
        loaded.store.provider_order.push(provider.clone());
    }
    crate::settings::save_settings_with_secrets(data_dir, &mut loaded, &secrets)
        .map_err(|error| error.to_string())?;
    host.reload_settings(data_dir)
        .map_err(|error| error.to_string())?;
    Ok(json!({ "provider": provider, "added": added }))
}

/// Drops every key of a provider, keeping the entry (`removeProviderKeys`).
pub fn remove(host: &Host, data_dir: &Path, request: &DaemonRequest) -> Result<Value, String> {
    let provider = super::require(request.get("provider"), "provider", MAX_PROVIDER_BYTES)?;
    classify(host, &provider)?;
    let mut loaded = crate::settings::load_settings(data_dir).map_err(|error| error.to_string())?;
    let mut secrets = crate::settings::load_secrets(data_dir).map_err(|error| error.to_string())?;
    let removed = secrets
        .providers
        .get(&provider)
        .map(|entry| entry.keys.len())
        .unwrap_or(0);
    if removed == 0 {
        return Ok(json!({ "provider": provider, "removed": 0 }));
    }
    secrets
        .providers
        .entry(provider.clone())
        .or_insert(ProviderSecrets { keys: Vec::new() })
        .keys
        .clear();
    if let Some(state) = loaded.store.providers.get_mut(&provider) {
        state.keys.clear();
    }
    crate::settings::save_settings_with_secrets(data_dir, &mut loaded, &secrets)
        .map_err(|error| error.to_string())?;
    host.reload_settings(data_dir)
        .map_err(|error| error.to_string())?;
    Ok(json!({ "provider": provider, "removed": removed }))
}

/// `classifyKeysTarget`: a provider is anything configured or known to the
/// catalog; an MCP server id is a different credential home (refused with
/// the source's sentence); anything else is unknown.
fn classify(host: &Host, provider: &str) -> Result<(), String> {
    let settings = host.settings();
    let Some(loaded) = settings.as_ref() else {
        return Err("settings are not loaded".to_owned());
    };
    let store = &loaded.store;
    if store.providers.contains_key(provider) || catalog_ids(host).contains(provider) {
        return Ok(());
    }
    if store.mcp_servers.contains_key(provider) {
        return Err(format!(
            "\"{provider}\" is an MCP server, not a model provider. MCP credentials live in host environment variables referenced by name in the server definition; manage them with openbutler mcp. This store holds model provider API keys only."
        ));
    }
    Err(format!(
        "Unknown provider \"{provider}\". Configure it first with openbutler setup."
    ))
}

/// The catalog's provider ids; an unloaded or unreadable catalog degrades
/// to "configured providers only", never to a failure.
fn catalog_ids(host: &Host) -> BTreeSet<String> {
    match host.catalog() {
        Some(catalog) if catalog.is_loaded() => catalog.list_provider_ids().into_iter().collect(),
        _ => BTreeSet::new(),
    }
}

/// The payload's `keys`: an array of strings, each under the key cap.
fn payload_keys(request: &DaemonRequest) -> Result<Vec<String>, String> {
    let Some(value) = request.get("keys") else {
        return Err("keys must be an array of strings.".to_owned());
    };
    let Some(items) = value.as_array() else {
        return Err("keys must be an array of strings.".to_owned());
    };
    if items.len() > MAX_KEYS_PER_CALL {
        return Err(format!(
            "keys must carry at most {MAX_KEYS_PER_CALL} entries."
        ));
    }
    let mut keys = Vec::with_capacity(items.len());
    for item in items {
        let Some(text) = item.as_str() else {
            return Err("keys must be an array of strings.".to_owned());
        };
        if js_len(text) > MAX_KEY_BYTES {
            return Err("a key is longer than a credential can be.".to_owned());
        }
        keys.push(text.to_owned());
    }
    Ok(keys)
}

/// The last four characters — the source's `key.slice(-4)`.
fn tail(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    chars[chars.len().saturating_sub(4)..].iter().collect()
}
