//! Moved from settings/schema.rs by the split (see dispatch-style refactor notes).
use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::SettingsError;
use super::helpers::string_array;
use super::keys::{KNOWN_PROVIDER_SECRET_KEYS, KNOWN_SECRET_KEYS, known};
use crate::shape::{as_record, js_len};

/// The `config.yaml` secrets document (provider keys and tokens).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SecretsStore {
    pub providers: BTreeMap<String, ProviderSecrets>,
    pub team_ticket: Option<String>,
    pub modal: Option<ModalCredentials>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
/// One provider's secret keys (first key is the live one).
pub struct ProviderSecrets {
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
/// Modal credentials for the voice channel (id/secret pair).
pub struct ModalCredentials {
    pub token_id: String,
    pub token_secret: String,
}

// --- Section parsers ---------------------------------------------------------

/// Parses a settings document that is already known to be a mapping.
fn parse_provider_secrets(raw: &Value, id: &str) -> Result<ProviderSecrets, SettingsError> {
    let Some(record) = as_record(raw) else {
        return Err(SettingsError(format!(
            "config providers.{id} must be a mapping"
        )));
    };
    for key in record.keys() {
        if !KNOWN_PROVIDER_SECRET_KEYS.contains(&key.as_str()) {
            return Err(SettingsError(format!(
                "config providers.{id}.{key} is not a secret openbutler knows (known: {})",
                known(&KNOWN_PROVIDER_SECRET_KEYS)
            )));
        }
    }
    Ok(ProviderSecrets {
        keys: match record.get("keys") {
            None => Vec::new(),
            Some(value) => string_array(value, &format!("config providers.{id}.keys"))?,
        },
    })
}

fn parse_modal_secrets(raw: &Value) -> Result<ModalCredentials, SettingsError> {
    let Some(record) = as_record(raw) else {
        return Err(SettingsError("config modal must be a mapping".to_owned()));
    };
    for key in record.keys() {
        if key != "tokenId" && key != "tokenSecret" {
            return Err(SettingsError(format!(
                "config modal.{key} is not a secret openbutler knows"
            )));
        }
    }
    let Some(token_id) = record.get("tokenId").and_then(Value::as_str) else {
        return Err(SettingsError(
            "config modal.tokenId must be a non-empty string.".to_owned(),
        ));
    };
    if token_id.is_empty() || js_len(token_id) > 256 {
        return Err(SettingsError(
            "config modal.tokenId must be a non-empty string.".to_owned(),
        ));
    }
    let Some(token_secret) = record.get("tokenSecret").and_then(Value::as_str) else {
        return Err(SettingsError(
            "config modal.tokenSecret must be a non-empty string.".to_owned(),
        ));
    };
    if token_secret.is_empty() || js_len(token_secret) > 512 {
        return Err(SettingsError(
            "config modal.tokenSecret must be a non-empty string.".to_owned(),
        ));
    }
    Ok(ModalCredentials {
        token_id: token_id.to_owned(),
        token_secret: token_secret.to_owned(),
    })
}

pub fn parse_secrets(doc: &Map<String, Value>) -> Result<SecretsStore, SettingsError> {
    for key in doc.keys() {
        if !KNOWN_SECRET_KEYS.contains(&key.as_str()) {
            return Err(SettingsError(format!(
                "{key} is not a secret openbutler knows (known: {})",
                known(&KNOWN_SECRET_KEYS)
            )));
        }
    }
    let mut secrets = SecretsStore::default();
    match doc.get("providers") {
        None => {}
        Some(value) => {
            // Unlike the public half (which skips a non-record providers
            // block), the secret half reads it strictly: `Object.entries`
            // over a non-record either throws or walks the wrong shape, and
            // an unreadable store must never count as an empty one.
            let Some(raw_providers) = as_record(value) else {
                return Err(SettingsError(
                    "config providers must be a mapping".to_owned(),
                ));
            };
            for (id, raw) in raw_providers {
                secrets
                    .providers
                    .insert(id.clone(), parse_provider_secrets(raw, id)?);
            }
        }
    }
    if let Some(value) = doc.get("teamTicket") {
        let Some(text) = value.as_str() else {
            return Err(SettingsError(
                "config teamTicket must be a non-empty string under 512 characters.".to_owned(),
            ));
        };
        if text.is_empty() || js_len(text) > 512 {
            return Err(SettingsError(
                "config teamTicket must be a non-empty string under 512 characters.".to_owned(),
            ));
        }
        secrets.team_ticket = Some(text.to_owned());
    }
    if let Some(value) = doc.get("modal") {
        secrets.modal = Some(parse_modal_secrets(value)?);
    }
    Ok(secrets)
}

#[cfg(test)]
mod tests {
    use crate::settings::schema::helpers::is_environment_name;
    use crate::shape::{is_absolute_path, is_id_token};

    #[test]
    fn environment_names_follow_the_regex_shape() {
        assert!(is_environment_name("OPENBUTLER_TOKEN"));
        assert!(is_environment_name("_x9"));
        assert!(!is_environment_name("9x"));
        assert!(!is_environment_name("A-B"));
        assert!(!is_environment_name(""));
    }

    #[test]
    fn id_tokens_follow_the_schedule_shape() {
        assert!(is_id_token("morning-brief_1", 64));
        assert!(!is_id_token("_leading", 64));
        assert!(!is_id_token("has space", 64));
        assert!(!is_id_token(&"x".repeat(65), 64));
    }

    #[test]
    fn absolute_paths_follow_the_host_rule() {
        #[cfg(windows)]
        {
            assert!(is_absolute_path(r"C:\work"));
            assert!(is_absolute_path(r"\\server\share"));
            assert!(!is_absolute_path("work/dir"));
        }
        #[cfg(not(windows))]
        {
            assert!(is_absolute_path("/home/user"));
            assert!(!is_absolute_path("home/user"));
        }
    }
}
