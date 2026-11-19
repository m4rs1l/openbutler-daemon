//! Moved from settings/schema.rs by the split (see dispatch-style refactor notes).
use std::collections::BTreeMap;

use serde_json::Value;

use super::SettingsError;
use super::helpers::{opaque_bounded, string_array};
use super::keys::{KNOWN_PROVIDER_KEYS, MODEL_SPEC_KEYS, known};
use crate::shape::{as_record, js_len};
/// One configured model provider (url, default model, specs).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderState {
    pub enabled: bool,
    pub keys: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub npm: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routing: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_routing: Option<BTreeMap<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_specs: Option<BTreeMap<String, ProviderModelSpec>>,
}

/// Settings-side description of one custom model.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
/// A provider model's declared specs (context window, output cap).
pub struct ProviderModelSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_efforts: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
}

pub fn parse_provider_state(raw: &Value, id: &str) -> Result<ProviderState, SettingsError> {
    let Some(record) = as_record(raw) else {
        return Err(SettingsError(format!("providers.{id} must be a mapping")));
    };
    for key in record.keys() {
        if !KNOWN_PROVIDER_KEYS.contains(&key.as_str()) {
            return Err(SettingsError(format!(
                "providers.{id}.{key} is not a setting openbutler knows (known: {})",
                known(&KNOWN_PROVIDER_KEYS)
            )));
        }
    }
    let mut state = ProviderState {
        enabled: record
            .get("enabled")
            .is_none_or(|value| value == &Value::Bool(true)),
        keys: match record.get("keys") {
            None => Vec::new(),
            Some(value) => string_array(value, &format!("providers.{id}.keys"))?,
        },
        models: None,
        url: None,
        npm: None,
        options: None,
        routing: None,
        model_routing: None,
        payload: None,
        model_specs: None,
    };
    if let Some(value) = record.get("models") {
        let models = string_array(value, &format!("providers.{id}.models"))?;
        if models.len() > 512
            || models
                .iter()
                .any(|model| model.trim().is_empty() || js_len(model) > 512)
        {
            return Err(SettingsError(format!(
                "providers.{id}.models must contain at most 512 non-empty model IDs under 512 characters each."
            )));
        }
        let mut unique = Vec::new();
        for model in models {
            let trimmed = model.trim().to_owned();
            if !unique.contains(&trimmed) {
                unique.push(trimmed);
            }
        }
        state.models = Some(unique);
    }
    if let Some(text) = record.get("url").and_then(Value::as_str)
        && !text.is_empty()
    {
        state.url = Some(text.to_owned());
    }
    if let Some(text) = record.get("npm").and_then(Value::as_str)
        && !text.is_empty()
    {
        state.npm = Some(text.to_owned());
    }
    if let Some(value) = record.get("options") {
        let options = opaque_bounded(value, &format!("providers.{id}.options"))?;
        if as_record(&options).is_some_and(|map| !map.is_empty()) {
            state.options = Some(options);
        }
    }
    if let Some(value) = record.get("routing") {
        let routing = opaque_bounded(value, &format!("providers.{id}.routing"))?;
        if as_record(&routing).is_some_and(|map| !map.is_empty()) {
            state.routing = Some(routing);
        }
    }
    if let Some(value) = record.get("modelRouting") {
        let entries = opaque_bounded(value, &format!("providers.{id}.modelRouting"))?;
        let entries_map = as_record(&entries).ok_or_else(|| {
            SettingsError(format!(
                "providers.{id}.modelRouting must be a mapping of option names to values"
            ))
        })?;
        let mut model_routing = BTreeMap::new();
        for (model_id, routing) in entries_map {
            if model_id.trim().is_empty() || js_len(model_id) > 512 {
                return Err(SettingsError(format!(
                    "providers.{id}.modelRouting requires non-empty model IDs under 512 characters."
                )));
            }
            let nested =
                opaque_bounded(routing, &format!("providers.{id}.modelRouting.{model_id}"))?;
            model_routing.insert(model_id.clone(), nested);
        }
        state.model_routing = Some(model_routing);
    }
    if let Some(value) = record.get("payload") {
        let payload = opaque_bounded(value, &format!("providers.{id}.payload"))?;
        if as_record(&payload).is_some_and(|map| !map.is_empty()) {
            state.payload = Some(payload);
        }
    }
    if let Some(value) = record.get("modelSpecs") {
        let Some(specs) = as_record(value) else {
            return Err(SettingsError(format!(
                "providers.{id}.modelSpecs must be a mapping"
            )));
        };
        let mut parsed = BTreeMap::new();
        for (model_id, raw_spec) in specs {
            if model_id.trim().is_empty() || js_len(model_id) > 512 {
                return Err(SettingsError(format!(
                    "providers.{id}.modelSpecs requires non-empty model IDs under 512 characters."
                )));
            }
            let Some(spec_record) = as_record(raw_spec) else {
                return Err(SettingsError(format!(
                    "providers.{id}.modelSpecs.{model_id} must be a mapping"
                )));
            };
            for key in spec_record.keys() {
                if !MODEL_SPEC_KEYS.contains(&key.as_str()) {
                    return Err(SettingsError(format!(
                        "providers.{id}.modelSpecs.{model_id}.{key} is not a model spec openbutler knows (known: {})",
                        known(&MODEL_SPEC_KEYS)
                    )));
                }
            }
            let mut spec = ProviderModelSpec::default();
            if let Some(value) = spec_record.get("name") {
                let Some(name) = value.as_str() else {
                    return Err(SettingsError(format!(
                        "providers.{id}.modelSpecs.{model_id}.name must use 1-128 characters."
                    )));
                };
                if name.trim().is_empty() || js_len(name) > 128 {
                    return Err(SettingsError(format!(
                        "providers.{id}.modelSpecs.{model_id}.name must use 1-128 characters."
                    )));
                }
                spec.name = Some(name.trim().to_owned());
            }
            for field in ["context", "output"] {
                let Some(value) = spec_record.get(field) else {
                    continue;
                };
                let number = value
                    .as_u64()
                    .filter(|number| *number >= 1 && *number <= 9_007_199_254_740_991);
                let Some(number) = number else {
                    return Err(SettingsError(format!(
                        "providers.{id}.modelSpecs.{model_id}.{field} must be a positive whole number of tokens."
                    )));
                };
                if field == "context" {
                    spec.context = Some(number);
                } else {
                    spec.output = Some(number);
                }
            }
            if let Some(value) = spec_record.get("reasoning") {
                let Some(reasoning) = as_record(value) else {
                    return Err(SettingsError(format!(
                        "providers.{id}.modelSpecs.{model_id}.reasoning must be a mapping"
                    )));
                };
                let efforts_raw = reasoning
                    .get("efforts")
                    .map(|value| {
                        string_array(
                            value,
                            &format!("providers.{id}.modelSpecs.{model_id}.reasoning.efforts"),
                        )
                    })
                    .transpose()?;
                let mut efforts = Vec::new();
                for effort in efforts_raw.unwrap_or_default() {
                    let trimmed = effort.trim().to_owned();
                    if !trimmed.is_empty() && !efforts.contains(&trimmed) {
                        efforts.push(trimmed);
                    }
                }
                if !efforts.is_empty() {
                    spec.reasoning_efforts = Some(efforts);
                }
            }
            if let Some(value) = spec_record.get("payload") {
                let payload = opaque_bounded(
                    value,
                    &format!("providers.{id}.modelSpecs.{model_id}.payload"),
                )?;
                if as_record(&payload).is_some_and(|map| !map.is_empty()) {
                    spec.payload = Some(payload);
                }
            }
            parsed.insert(model_id.clone(), spec);
        }
        if !parsed.is_empty() {
            state.model_specs = Some(parsed);
        }
    }
    Ok(state)
}
