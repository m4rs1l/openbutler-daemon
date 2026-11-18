//! Saving: the only writer of the two settings files.
//!
//! Write authority comes from loading first. A copy that was loaded
//! carries the revision it read; a copy built from defaults carries none
//! and may only create an empty machine. Source flow: `settings.ts`
//! `saveSettings` / `saveSettingsUnlocked` / `diskSettingsRevision`
//! (read 2026-11-16).

use std::path::Path;

use serde_json::{Map, Value, json};

use super::load::{disk_settings_revision, load_secrets_unlocked};
use super::schema::{SecretsStore, SettingsStore};
use super::transaction;
use super::{LoadedSettings, SettingsError};

const STALE: &str = "Settings changed after this copy was loaded. Reload settings before saving to avoid overwriting another writer.";
const NEVER_LOADED: &str =
    "Refusing to overwrite existing configuration: this store was never loaded from";
const NEVER_LOADED_SUFFIX: &str = ". Load the settings first, change the loaded store, and save that; a store that was not loaded may only create the files, never replace them.";

/// Saves the pair under the transaction boundary after checking this
/// copy's right to write. The document builder runs inside the same lock
/// and re-reads the secrets file first: an unreadable store is an error,
/// never an empty one.
pub fn save_settings(data_dir: &Path, loaded: &mut LoadedSettings) -> Result<(), SettingsError> {
    let lock = transaction::acquire(data_dir, transaction::default_wait())?;
    let disk = disk_settings_revision(data_dir)?;
    match loaded.revision.as_deref() {
        Some(expected) if expected != disk => {
            return Err(SettingsError(STALE.to_owned()));
        }
        Some(_) => {}
        None => {
            // Write authority comes from reading the file first. A store
            // that was never loaded has nothing to reconcile against, so it
            // may create an empty machine but must never replace existing
            // configuration — that is how a defaults object wiped
            // settings.yaml and every provider key.
            let settings_path = data_dir.join("settings.yaml");
            let config_path = data_dir.join("config.yaml");
            if settings_path.exists() || config_path.exists() {
                return Err(SettingsError(format!(
                    "{NEVER_LOADED} {}{NEVER_LOADED_SUFFIX}",
                    settings_path.display()
                )));
            }
        }
    }

    let secrets = load_secrets_unlocked(data_dir)?;
    let settings_yaml = serialize(&build_settings_document(&loaded.store)?)?;
    let config_yaml = serialize(&build_secrets_document(&secrets)?)?;
    lock.commit_pair(data_dir, &settings_yaml, &config_yaml)?;
    loaded.revision = Some(disk_settings_revision(data_dir)?);
    Ok(())
}

/// Stable YAML for the journaled pair. The emitter's wrap width mirrors
/// the established 120-column line length; byte-for-byte equality with
/// any other emitter is not the goal — stability across our own writes
/// is, and the revision hash reads exactly what we wrote.
fn serialize(document: &Value) -> Result<String, SettingsError> {
    serde_saphyr::to_string_with_options(
        document,
        serde_saphyr::ser_options! {
            indent_step: 2,
            folded_wrap_chars: 120,
        },
    )
    .map_err(|error| SettingsError(error.to_string()))
}

/// The public half, in the established field order. Top-level keys are
/// inserted by hand because each conditional mirrors the source rule:
/// truthy strings stay absent when empty, `undefined` options stay
/// absent, and the always-present fields always land.
fn build_settings_document(store: &SettingsStore) -> Result<Value, SettingsError> {
    let mut document = Map::new();

    // Providers, ordered: the declared order first, then any provider the
    // store holds but the order list never named.
    let mut ordered_ids: Vec<&String> = store
        .provider_order
        .iter()
        .filter(|id| store.providers.contains_key(*id))
        .collect();
    for id in store.providers.keys() {
        if !ordered_ids.contains(&id) {
            ordered_ids.push(id);
        }
    }
    let mut providers = Map::new();
    for &id in &ordered_ids {
        let Some(provider) = store.providers.get(id) else {
            continue;
        };
        let mut encoded =
            serde_json::to_value(provider).map_err(|error| SettingsError(error.to_string()))?;
        // Keys live in config.yaml only; settings.yaml never sees them.
        if let Value::Object(record) = &mut encoded {
            record.remove("keys");
        }
        providers.insert(id.to_owned(), encoded);
    }
    document.insert("providers".to_owned(), Value::Object(providers));
    document.insert(
        "mcpServers".to_owned(),
        serde_json::to_value(&store.mcp_servers)
            .map_err(|error| SettingsError(error.to_string()))?,
    );
    document.insert(
        "providerOrder".to_owned(),
        Value::Array(
            ordered_ids
                .into_iter()
                .cloned()
                .map(Value::String)
                .collect(),
        ),
    );
    document.insert("defaultProvider".to_owned(), json!(store.default_provider));
    document.insert("defaultModel".to_owned(), json!(store.default_model));

    if let Some(reasoning) = &store.reasoning
        && !reasoning.is_empty()
    {
        document.insert("reasoning".to_owned(), json!(reasoning));
    }
    if let Some(show) = store.show_reasoning {
        document.insert("showReasoning".to_owned(), json!(show));
    }
    if let Some(theme) = &store.theme {
        document.insert("theme".to_owned(), json!(theme.as_str()));
    }
    document.insert(
        "approvalMode".to_owned(),
        json!(store.approval_mode.as_str()),
    );
    if let Some(seconds) = store.auto_review_timeout_seconds {
        document.insert("autoReviewTimeoutSeconds".to_owned(), json!(seconds));
    }
    if let Some(provider) = &store.auto_review_provider {
        document.insert("autoReviewProvider".to_owned(), json!(provider));
    }
    if let Some(model) = &store.auto_review_model {
        document.insert("autoReviewModel".to_owned(), json!(model));
    }
    if let Some(roles) = &store.roles {
        document.insert(
            "roles".to_owned(),
            serde_json::to_value(roles).map_err(|error| SettingsError(error.to_string()))?,
        );
    }
    if let Some(seconds) = store.approval_timeout_seconds {
        document.insert("approvalTimeoutSeconds".to_owned(), json!(seconds));
    }
    // Truthy like the source rule: an empty shell is not a configured
    // shell, and the parser would refuse it on the way back in.
    if let Some(shell) = &store.command_shell
        && !shell.is_empty()
    {
        document.insert("commandShell".to_owned(), json!(shell));
    }
    if let Some(root) = &store.default_workspace_root
        && !root.is_empty()
    {
        document.insert("defaultWorkspaceRoot".to_owned(), json!(root));
    }
    if let Some(daemon) = &store.daemon {
        document.insert(
            "daemon".to_owned(),
            serde_json::to_value(daemon).map_err(|error| SettingsError(error.to_string()))?,
        );
    }
    if let Some(telegram) = &store.telegram {
        document.insert(
            "telegram".to_owned(),
            serde_json::to_value(telegram).map_err(|error| SettingsError(error.to_string()))?,
        );
    }
    if let Some(team) = &store.team {
        document.insert(
            "team".to_owned(),
            serde_json::to_value(team).map_err(|error| SettingsError(error.to_string()))?,
        );
    }
    if let Some(custodian) = &store.custodian {
        document.insert(
            "custodian".to_owned(),
            serde_json::to_value(custodian).map_err(|error| SettingsError(error.to_string()))?,
        );
    }

    Ok(Value::Object(document))
}

/// The secret half: provider keys this store rewrites, plus the ticket
/// and Modal credentials exactly as they were read — another owner's
/// rotation survives this save.
fn build_secrets_document(secrets: &SecretsStore) -> Result<Value, SettingsError> {
    let mut providers = Map::new();
    for (id, secret) in &secrets.providers {
        providers.insert(id.clone(), json!({ "keys": secret.keys }));
    }
    let mut document = Map::new();
    document.insert("providers".to_owned(), Value::Object(providers));
    if let Some(ticket) = &secrets.team_ticket
        && !ticket.is_empty()
    {
        document.insert("teamTicket".to_owned(), json!(ticket));
    }
    if let Some(modal) = &secrets.modal {
        document.insert(
            "modal".to_owned(),
            serde_json::to_value(modal).map_err(|error| SettingsError(error.to_string()))?,
        );
    }
    Ok(Value::Object(document))
}
