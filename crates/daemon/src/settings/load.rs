//! Loading: lock, recover, parse, merge — the only reader of the two
//! settings files.
//!
//! Every load runs under the transaction boundary, so an interrupted commit
//! heals before anything reads and a reader never observes a half-written
//! pair. Source flow: `settings.ts` `loadSettings` / `loadSettingsUnlocked`
//! (read 2026-11-16).

use std::fs;
use std::path::Path;

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::schema::{SecretsStore, SettingsStore, default_settings, parse_secrets, parse_store};
use super::transaction;
use super::{LoadedSettings, SettingsError};

/// The file names inside a data directory.
const SETTINGS_FILE: &str = "settings.yaml";
const CONFIG_FILE: &str = "config.yaml";

/// Reads settings from disk, or returns the empty machine when the file is
/// absent. Any other I/O failure stays visible — only a missing file means
/// "defaults".
pub fn load_settings(data_dir: &Path) -> Result<LoadedSettings, SettingsError> {
    let _lock = transaction::acquire(data_dir, transaction::default_wait())?;
    let path = data_dir.join(SETTINGS_FILE);
    let store = match fs::read_to_string(&path) {
        Ok(text) => {
            let parsed: Value = parse_yaml(&text)?;
            // A null document is the empty machine — and deliberately not merged
            // with secrets: there is no settings half to merge them into.
            if parsed.is_null() {
                default_settings()
            } else {
                let Some(record) = parsed.as_object() else {
                    return Err(SettingsError(format!(
                        "{} must be a mapping",
                        path.display()
                    )));
                };
                let mut settings = parse_store(record)?;
                let secrets = load_secrets_unlocked(data_dir)?;
                merge_secrets(&mut settings, &secrets);
                settings
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // No settings file still leaves the secret store: keys without
            // settings are a configured provider, not an empty one.
            let mut settings = default_settings();
            let secrets = load_secrets_unlocked(data_dir)?;
            merge_secrets(&mut settings, &secrets);
            settings
        }
        Err(error) => return Err(SettingsError(error.to_string())),
    };
    // Write authority starts here: the copy records what it read, and every
    // save is checked against that reading (see `settings::save`).
    Ok(LoadedSettings {
        store,
        revision: Some(disk_settings_revision(data_dir)?),
    })
}

/// The optimistic-concurrency fingerprint of the pair on disk: the settings
/// bytes plus the provider-secret entries (sorted by id, as JSON). A
/// separate owner can rotate a team ticket or Modal token without editing
/// model settings, so only what this store rewrites participates — a
/// ticket rotation must not invalidate a loaded copy.
pub(crate) fn disk_settings_revision(data_dir: &Path) -> Result<String, SettingsError> {
    let mut hash = Sha256::new();
    match fs::read(data_dir.join(SETTINGS_FILE)) {
        Ok(bytes) => {
            hash.update(b"settings:");
            hash.update(&bytes);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            hash.update(b"settings:missing");
        }
        Err(error) => return Err(SettingsError(error.to_string())),
    }
    // An unreadable secrets store fails here rather than counting as empty:
    // the revision is only trustworthy when every input read succeeded.
    let secrets = load_secrets_unlocked(data_dir)?;
    let entries: Vec<(&String, &super::schema::ProviderSecrets)> =
        secrets.providers.iter().collect();
    hash.update(serde_json::to_vec(&entries).map_err(|error| SettingsError(error.to_string()))?);
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Reads the secret half under the same lock.
pub fn load_secrets(data_dir: &Path) -> Result<SecretsStore, SettingsError> {
    let _lock = transaction::acquire(data_dir, transaction::default_wait())?;
    load_secrets_unlocked(data_dir)
}

/// Parses one settings document into an untyped value with the contract's
/// scalar rules: `strict_booleans` keeps `y`/`n`/`no`/`on`… as strings, which
/// is exactly the js-yaml `JSON_SCHEMA` boolean set (true/false and their
/// case variants) — without it the YAML 1.1 spellings would swallow a
/// schedule named `n` or accept `enabled: no` the legacy loader refuses.
fn parse_yaml(text: &str) -> Result<Value, SettingsError> {
    serde_saphyr::from_str_with_options(text, serde_saphyr::options! { strict_booleans: true })
        .map_err(|error| SettingsError(error.to_string()))
}

/// The lock-held reader; callers already hold the transaction boundary.
pub(crate) fn load_secrets_unlocked(data_dir: &Path) -> Result<SecretsStore, SettingsError> {
    let path = data_dir.join(CONFIG_FILE);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SecretsStore::default());
        }
        Err(error) => return Err(SettingsError(error.to_string())),
    };
    let parsed: Value = parse_yaml(&text)?;
    if parsed.is_null() {
        return Ok(SecretsStore::default());
    }
    let Some(record) = parsed.as_object() else {
        return Err(SettingsError(format!(
            "{} must be a mapping",
            path.display()
        )));
    };
    parse_secrets(record)
}

/// Folds the secret store into the loaded settings: keys from both files
/// unite (a hand edit on either side never loses a key); a config-only
/// provider is created enabled and joins the provider order.
pub(crate) fn merge_secrets(settings: &mut SettingsStore, secrets: &SecretsStore) {
    for (id, secret) in &secrets.providers {
        match settings.providers.get_mut(id) {
            Some(existing) => {
                let mut merged = secret.keys.clone();
                for key in &existing.keys {
                    if !merged.contains(key) {
                        merged.push(key.clone());
                    }
                }
                existing.keys = merged;
            }
            None => {
                settings.providers.insert(
                    id.clone(),
                    super::schema::ProviderState {
                        enabled: true,
                        keys: secret.keys.clone(),
                        models: None,
                        url: None,
                        npm: None,
                        options: None,
                        routing: None,
                        model_routing: None,
                        payload: None,
                        model_specs: None,
                    },
                );
            }
        }
        if !settings.provider_order.contains(id) {
            settings.provider_order.push(id.clone());
        }
    }
}
