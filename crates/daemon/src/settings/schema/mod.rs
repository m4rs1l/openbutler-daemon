//! Closed schemas: what each settings document may say, and the exact
//! refusal for everything else.
//!
//! A typo that silently fell back to a default is a bug that surfaces days
//! later; here it fails at load with the known set named. Source:
//! `settings.ts` L226-L1150 (read 2026-11-16). Every message is the
//! contract — tests pin them byte for byte.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::SettingsError;
use crate::listen_target::{ListenTargetError, parse_listen_target};
use crate::shape::{as_record, is_absolute_path, js_len};

use enums::{is_shell_setting_value, is_variant_id};
use helpers::{string_array, timeout_seconds};
use keys::{KNOWN_KEYS, SHELL_PRESETS, known};

mod custodian;
mod enums;
mod helpers;
mod keys;
mod mcp;
mod provider;
mod secrets;
mod store;
mod team;
mod telegram;

pub use custodian::parse_custodian_settings;
pub use enums::{ApprovalMode, TeamRole, Theme};
pub use mcp::{McpServerState, parse_mcp_server_state};
pub use provider::{ProviderModelSpec, ProviderState, parse_provider_state};
pub use secrets::{ModalCredentials, ProviderSecrets, SecretsStore, parse_secrets};
pub use store::{DaemonSettings, SettingsStore, default_settings};
pub use team::{
    CustodianSettings, RoleBinding, TeamSettings, WorkerCaps, parse_roles, parse_team_settings,
};
pub use telegram::{TelegramSettings, parse_telegram_settings};

pub fn parse_store(doc: &Map<String, Value>) -> Result<SettingsStore, SettingsError> {
    for key in doc.keys() {
        if !KNOWN_KEYS.contains(&key.as_str()) {
            return Err(SettingsError(format!(
                "{key} is not a setting openbutler knows (known: {})",
                known(&KNOWN_KEYS)
            )));
        }
    }

    let mut settings = default_settings();

    if let Some(raw_providers) = doc.get("providers").and_then(as_record) {
        for (id, raw) in raw_providers {
            settings
                .providers
                .insert(id.clone(), parse_provider_state(raw, id)?);
        }
    }
    if let Some(raw_mcp) = doc.get("mcpServers") {
        let Some(record) = as_record(raw_mcp) else {
            return Err(SettingsError("mcpServers must be a mapping".to_owned()));
        };
        for (id, raw) in record {
            settings
                .mcp_servers
                .insert(id.clone(), parse_mcp_server_state(raw, id)?);
        }
    }

    if let Some(raw_order) = doc.get("providerOrder") {
        let order = string_array(raw_order, "providerOrder")?;
        settings.provider_order = order
            .into_iter()
            .filter(|id| settings.providers.contains_key(id))
            .collect();
    }
    for id in settings.providers.keys() {
        if !settings.provider_order.contains(id) {
            settings.provider_order.push(id.clone());
        }
    }

    if let Some(value) = doc.get("defaultProvider").and_then(Value::as_str) {
        settings.default_provider = value.to_owned();
    }
    if let Some(value) = doc.get("defaultModel").and_then(Value::as_str) {
        settings.default_model = value.to_owned();
    }
    if let Some(value) = doc.get("reasoning") {
        let Some(text) = value.as_str() else {
            return Err(SettingsError(
                "reasoning must be a valid model variant id".to_owned(),
            ));
        };
        if !is_variant_id(text) {
            return Err(SettingsError(
                "reasoning must be a valid model variant id".to_owned(),
            ));
        }
        settings.reasoning = Some(text.to_owned());
    }
    if let Some(value) = doc.get("showReasoning") {
        let Some(flag) = value.as_bool() else {
            return Err(SettingsError("showReasoning must be a boolean".to_owned()));
        };
        settings.show_reasoning = Some(flag);
    }
    if let Some(value) = doc.get("theme") {
        let Some(text) = value.as_str() else {
            return Err(SettingsError(
                "theme must be Dark, Light, or System".to_owned(),
            ));
        };
        settings.theme = Some(match text {
            "Dark" => Theme::Dark,
            "Light" => Theme::Light,
            "System" => Theme::System,
            _ => {
                return Err(SettingsError(
                    "theme must be Dark, Light, or System".to_owned(),
                ));
            }
        });
    }
    if let Some(value) = doc.get("commandShell") {
        let shell = value.as_str().map(str::trim).unwrap_or("");
        if !is_shell_setting_value(shell) {
            return Err(SettingsError(format!(
                "commandShell must be one of {} or an executable path",
                SHELL_PRESETS.join(", ")
            )));
        }
        settings.command_shell = Some(shell.to_owned());
    }
    if let Some(value) = doc.get("daemon") {
        let Some(record) = as_record(value) else {
            return Err(SettingsError("daemon must be a mapping".to_owned()));
        };
        for key in record.keys() {
            if key != "listen" {
                return Err(SettingsError(format!(
                    "{key} is not a daemon setting openbutler knows (known: listen)"
                )));
            }
        }
        if let Some(listen) = record.get("listen") {
            let Some(text) = listen.as_str() else {
                return Err(SettingsError("daemon.listen must be a string".to_owned()));
            };
            parse_listen_target(text)
                .map_err(|error: ListenTargetError| SettingsError(error.to_string()))?;
            settings.daemon = Some(DaemonSettings {
                listen: text.to_owned(),
            });
        }
    }
    if let Some(value) = doc.get("approvalMode") {
        let Some(text) = value.as_str() else {
            return Err(SettingsError(
                "approvalMode must be workspace, auto-review, or full-access".to_owned(),
            ));
        };
        settings.approval_mode = match text {
            "ask" => ApprovalMode::Workspace,
            "workspace" => ApprovalMode::Workspace,
            "auto-review" => ApprovalMode::AutoReview,
            "full-access" => ApprovalMode::FullAccess,
            _ => {
                return Err(SettingsError(
                    "approvalMode must be workspace, auto-review, or full-access".to_owned(),
                ));
            }
        };
    }
    if let Some(value) = doc.get("autoReviewTimeoutSeconds") {
        settings.auto_review_timeout_seconds =
            Some(timeout_seconds(value, "autoReviewTimeoutSeconds")?);
    }
    if doc.contains_key("autoReviewProvider") || doc.contains_key("autoReviewModel") {
        let provider = doc.get("autoReviewProvider");
        let model = doc.get("autoReviewModel");
        match (provider, model) {
            (None, None) => {}
            (Some(Value::Null), _) | (_, Some(Value::Null)) => {
                settings.auto_review_provider = None;
                settings.auto_review_model = None;
            }
            _ => {
                let valid = |value: Option<&Value>| matches!(value, Some(Value::String(text)) if !text.trim().is_empty());
                if !(valid(provider) && valid(model)) {
                    return Err(SettingsError(
                        "autoReviewProvider and autoReviewModel must both be non-empty strings, or both omitted."
                            .to_owned(),
                    ));
                }
                settings.auto_review_provider = Some(
                    provider
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_owned(),
                );
                settings.auto_review_model = Some(
                    model
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_owned(),
                );
            }
        }
    }
    if let Some(value) = doc.get("roles") {
        settings.roles = parse_roles(value)?;
    }
    if let Some(value) = doc.get("approvalTimeoutSeconds") {
        settings.approval_timeout_seconds = Some(timeout_seconds(value, "approvalTimeoutSeconds")?);
    }
    if let Some(value) = doc.get("defaultWorkspaceRoot") {
        let Some(text) = value.as_str() else {
            return Err(SettingsError(
                "defaultWorkspaceRoot must be an absolute path.".to_owned(),
            ));
        };
        if js_len(text) > 32_768 || !is_absolute_path(text) {
            return Err(SettingsError(
                "defaultWorkspaceRoot must be an absolute path.".to_owned(),
            ));
        }
        settings.default_workspace_root = Some(text.to_owned());
    }
    if let Some(value) = doc.get("telegram") {
        settings.telegram = Some(parse_telegram_settings(value)?);
    }
    if let Some(value) = doc.get("team") {
        settings.team = Some(parse_team_settings(value)?);
    }
    if let Some(value) = doc.get("custodian") {
        settings.custodian = Some(parse_custodian_settings(value)?);
    }

    // Read-time ingestion of the single-purpose fields written before roles
    // existed: from parse on there is one place every reader consults.
    if settings
        .roles
        .as_ref()
        .is_none_or(|roles| !roles.contains_key("review"))
        && settings.auto_review_provider.is_some()
        && settings.auto_review_model.is_some()
    {
        let roles = settings.roles.get_or_insert_with(BTreeMap::new);
        roles.insert(
            "review".to_owned(),
            RoleBinding {
                provider: settings.auto_review_provider.clone().unwrap_or_default(),
                model: settings.auto_review_model.clone().unwrap_or_default(),
            },
        );
    }
    if settings
        .roles
        .as_ref()
        .is_none_or(|roles| !roles.contains_key("custodian"))
        && let (Some(provider), Some(model)) = (
            settings
                .custodian
                .as_ref()
                .and_then(|c| c.provider.as_ref()),
            settings.custodian.as_ref().and_then(|c| c.model.as_ref()),
        )
    {
        let roles = settings.roles.get_or_insert_with(BTreeMap::new);
        roles.insert(
            "custodian".to_owned(),
            RoleBinding {
                provider: provider.clone(),
                model: model.clone(),
            },
        );
    }

    Ok(settings)
}
