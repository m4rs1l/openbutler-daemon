//! `settings.setup` — the first-run wizard's one atomic decision.
//!
//! New contract, deliberately not a port: the legacy wizard wrote the
//! files itself from the CLI (client-side write, pre-rule). Backend-
//! owned here — the CLI collects the answers and the daemon performs
//! the write: provider upserts (keys replace, `url` trims to absent,
//! `npm` optional, `enabled` defaults true — the source's
//! `withProviderFields` merge, `settings.ts` L298), the order push
//! (`createSession`-style: a provider the order never named is appended),
//! the defaults, then one journaled pair save and a reload.

use std::path::Path;

use openbutler_protocol::DaemonRequest;
use serde_json::{Value, json};

use super::schema::ProviderState;
use super::{
    ProviderSecrets, load_secrets, load_settings, remember_provider_model,
    save_settings_with_secrets,
};
use crate::dispatch::Host;

/// One setup payload parsed into what the wizard collected. `review` and
/// `handle` share this parse, so the screen can never show a set the save
/// would refuse (or refuse one it would accept).
struct SetupPayload {
    entries: Vec<SetupEntry>,
    default_provider: Option<String>,
    default_model: Option<String>,
}

/// The wizard's Review block: a blank line, the section bar, one bullet
/// per provider, a blank line and the default-model row. Composed here
/// because the layout is a decision the client must not make.
pub fn review(request: &DaemonRequest) -> Result<Value, String> {
    let SetupPayload {
        entries,
        default_provider,
        default_model,
    } = parse_payload(&request.payload)?;
    Ok(json!({
        "lines": review_lines(
            &entries,
            default_provider.as_deref(),
            default_model.as_deref(),
        ),
    }))
}

/// The review lines (`cli/setup.ts` render, moved to the host under the
/// backend-owned rule): `· on {id}{ *} — {n} key(s)` per provider, with
/// the empty set answered by the source's own sentence.
fn review_lines(
    entries: &[SetupEntry],
    default_provider: Option<&str>,
    default_model: Option<&str>,
) -> Vec<Value> {
    let mut lines = vec![
        crate::shape::screen_line("plain", ""),
        crate::shape::section_line("Review"),
    ];
    if entries.is_empty() {
        lines.push(crate::shape::screen_line(
            "plain",
            "  \u{00b7} no provider configured",
        ));
    } else {
        for entry in entries {
            let marker = if default_provider == Some(entry.id.as_str()) {
                " *"
            } else {
                ""
            };
            let count = if entry.keys.len() == 1 {
                "1 key".to_owned()
            } else {
                format!("{} keys", entry.keys.len())
            };
            lines.push(crate::shape::screen_line(
                "plain",
                format!("  \u{00b7} on {}{marker} \u{2014} {count}", entry.id),
            ));
        }
    }
    lines.push(crate::shape::screen_line("plain", ""));
    let pair = match (default_provider, default_model) {
        (Some(provider), Some(model)) => format!("{provider} / {model}"),
        _ => "(none)".to_owned(),
    };
    lines.push(crate::shape::kv_line("Default model", &pair));
    lines
}

/// Applies a setup payload and saves both documents atomically.
pub fn handle(host: &Host, data_dir: &Path, request: &DaemonRequest) -> Result<Value, String> {
    let SetupPayload {
        entries,
        default_provider,
        default_model,
    } = parse_payload(&request.payload)?;

    let mut loaded = load_settings(data_dir).map_err(|error| error.to_string())?;
    let mut secrets = load_secrets(data_dir).map_err(|error| error.to_string())?;

    for entry in &entries {
        // `withProviderFields` (settings.ts L298): enabled defaults true
        // and survives, keys are replaced, every other field comes from
        // the incoming patch when present and from the previous provider
        // otherwise.
        let mut provider =
            loaded
                .store
                .providers
                .get(&entry.id)
                .cloned()
                .unwrap_or(ProviderState {
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
        provider.keys.clone_from(&entry.keys);
        if let Some(url) = &entry.url {
            provider.url = Some(url.clone());
        }
        if let Some(npm) = &entry.npm {
            provider.npm = Some(npm.clone());
        }
        secrets.providers.insert(
            entry.id.clone(),
            ProviderSecrets {
                keys: entry.keys.clone(),
            },
        );
        loaded.store.providers.insert(entry.id.clone(), provider);
        if !loaded.store.provider_order.contains(&entry.id) {
            loaded.store.provider_order.push(entry.id.clone());
        }
    }
    // `setDefaultModel` (config-edit.ts L74): the pair moves together and
    // both pairs are remembered, so provider history and the defaults can
    // never disagree. One half without the other keeps the other half —
    // the action's permissive contract; the wizard always sends both.
    match (default_provider, default_model) {
        (Some(provider), Some(model)) => {
            let outgoing = (
                loaded.store.default_provider.clone(),
                loaded.store.default_model.clone(),
            );
            remember_provider_model(&mut loaded.store, &outgoing.0, &outgoing.1)?;
            remember_provider_model(&mut loaded.store, &provider, &model)?;
            loaded.store.default_provider = provider;
            loaded.store.default_model = model;
        }
        (Some(provider), None) => loaded.store.default_provider = provider,
        (None, Some(model)) => loaded.store.default_model = model,
        (None, None) => {}
    }

    save_settings_with_secrets(data_dir, &mut loaded, &secrets)
        .map_err(|error| error.to_string())?;
    host.reload_settings(data_dir)
        .map_err(|error| error.to_string())?;

    Ok(json!({
        "settingsFile": data_dir.join("settings.yaml").display().to_string(),
        "configFile": super::config_path(data_dir).display().to_string(),
    }))
}

/// The payload shape both `review` and `handle` accept: at most 32
/// providers, each with a bounded id, 1..=16 unique non-empty keys, an
/// optional URL and an optional npm package.
fn parse_payload(payload: &serde_json::Map<String, Value>) -> Result<SetupPayload, String> {
    const PROVIDERS_ERROR: &str =
        "settings.setup providers must be an array of at most 32 entries.";
    let providers = match payload.get("providers") {
        Some(Value::Array(providers)) if providers.len() <= 32 => providers,
        _ => return Err(PROVIDERS_ERROR.to_owned()),
    };
    let default_provider = optional_string(payload, "defaultProvider", 256)?;
    let default_model = optional_string(payload, "defaultModel", 256)?;

    let mut entries: Vec<SetupEntry> = Vec::with_capacity(providers.len());
    let mut seen_ids = std::collections::HashSet::new();
    for (index, raw) in providers.iter().enumerate() {
        let record = raw
            .as_object()
            .ok_or_else(|| format!("providers[{index}] must be a mapping with id and keys."))?;
        let id = record
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty() && crate::shape::js_len(id) <= 256)
            .ok_or_else(|| {
                format!("providers[{index}].id must be a non-empty string under 256 bytes.")
            })?
            .to_owned();
        // One entry per provider: the merge keeps the last value for a
        // repeated id, which would silently drop the earlier entry's keys.
        if !seen_ids.insert(id.clone()) {
            return Err("settings.setup providers must have unique ids.".to_owned());
        }
        let keys = record
            .get("keys")
            .and_then(Value::as_array)
            .filter(|keys| (1..=16).contains(&keys.len()))
            .ok_or_else(|| {
                format!(
                    "providers[{index}].keys must be a non-empty array of at most 16 strings under 4096 bytes each."
                )
            })?;
        let mut parsed_keys = Vec::with_capacity(keys.len());
        for key in keys {
            let key = key
                .as_str()
                .filter(|key| !key.trim().is_empty() && crate::shape::js_len(key) <= 4096)
                .ok_or_else(|| {
                    format!(
                        "providers[{index}].keys must be a non-empty array of at most 16 strings under 4096 bytes each."
                    )
                })?;
            parsed_keys.push(key.trim().to_owned());
        }
        // One key is one credential: a repeated entry is dropped here, the
        // same rule `keys.add` applies, so the store's shape never depends on
        // which client wrote it.
        let mut seen = std::collections::HashSet::new();
        parsed_keys.retain(|key| seen.insert(key.clone()));
        let url = match record.get("url") {
            None | Some(Value::Null) => None,
            Some(Value::String(url)) if crate::shape::js_len(url) <= 2048 => {
                let trimmed = url.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_owned())
                }
            }
            Some(_) => {
                return Err(format!(
                    "providers[{index}].url must be a string under 2048 bytes."
                ));
            }
        };
        let npm = match record.get("npm") {
            None | Some(Value::Null) => None,
            Some(Value::String(npm)) if crate::shape::js_len(npm) <= 256 => {
                let trimmed = npm.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_owned())
                }
            }
            Some(_) => {
                return Err(format!(
                    "providers[{index}].npm must be a string under 256 bytes."
                ));
            }
        };
        entries.push(SetupEntry {
            id,
            keys: parsed_keys,
            url,
            npm,
        });
    }
    Ok(SetupPayload {
        entries,
        default_provider,
        default_model,
    })
}

/// One wizard answer: the provider's whole key list replaces whatever
/// the store held (the wizard collects the complete list each run).
struct SetupEntry {
    id: String,
    keys: Vec<String>,
    url: Option<String>,
    npm: Option<String>,
}

/// An optional present field must still be a bounded non-empty string
/// (the source's input gates; absent keeps the previous value).
fn optional_string(
    payload: &serde_json::Map<String, Value>,
    name: &'static str,
    max_bytes: usize,
) -> Result<Option<String>, String> {
    match payload.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value))
            if !value.trim().is_empty() && crate::shape::js_len(value) <= max_bytes =>
        {
            Ok(Some(value.trim().to_owned()))
        }
        Some(_) => Err(format!(
            "{name} must be a non-empty string under {max_bytes} bytes."
        )),
    }
}
