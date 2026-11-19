//! Moved from settings/schema.rs by the split (see dispatch-style refactor notes).
use std::collections::BTreeMap;

use serde_json::Value;

use super::SettingsError;
use super::helpers::is_environment_name;
use super::keys::TELEGRAM_KEYS;
use crate::shape::{as_record, is_absolute_path};
/// `telegram`: channel identity and workspace binding.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TelegramSettings {
    pub enabled: bool,
    pub token_env: String,
    pub workspace_root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

/// Maps names exposed to a server to host environment-variable names. A
/// literal secret in either half is rejected at parse.
pub(super) fn environment_references(
    value: Option<&Value>,
    where_: &str,
    is_target_name: fn(&str) -> bool,
) -> Result<BTreeMap<String, String>, SettingsError> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let Some(record) = as_record(value) else {
        return Err(SettingsError(format!(
            "{where_} must map names to environment-variable names"
        )));
    };
    let mut out = BTreeMap::new();
    for (key, reference) in record {
        let reference_text = reference.as_str().unwrap_or("");
        if matches!(key.as_str(), "__proto__" | "prototype" | "constructor")
            || !is_target_name(key)
            || !reference.is_string()
            || !is_environment_name(reference_text)
        {
            return Err(SettingsError(format!(
                "{where_}.{key} must name a host environment variable"
            )));
        }
        out.insert(key.clone(), reference_text.to_owned());
    }
    Ok(out)
}

pub fn parse_telegram_settings(raw: &Value) -> Result<TelegramSettings, SettingsError> {
    let Some(record) = as_record(raw) else {
        return Err(SettingsError(
            "telegram contains an unsupported setting.".to_owned(),
        ));
    };
    for key in record.keys() {
        if !TELEGRAM_KEYS.contains(&key.as_str()) {
            return Err(SettingsError(
                "telegram contains an unsupported setting.".to_owned(),
            ));
        }
    }
    let enabled = match record.get("enabled") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => {
            return Err(SettingsError(
                "telegram.enabled must be a boolean.".to_owned(),
            ));
        }
    };
    let Some(token_env) = record.get("tokenEnv").and_then(Value::as_str) else {
        return Err(SettingsError(
            "telegram.tokenEnv must name an environment variable, not contain a token.".to_owned(),
        ));
    };
    if !is_environment_name(token_env) {
        return Err(SettingsError(
            "telegram.tokenEnv must name an environment variable, not contain a token.".to_owned(),
        ));
    }
    let Some(workspace_root) = record.get("workspaceRoot").and_then(Value::as_str) else {
        return Err(SettingsError(
            "telegram.workspaceRoot must be an absolute path.".to_owned(),
        ));
    };
    if !is_absolute_path(workspace_root) {
        return Err(SettingsError(
            "telegram.workspaceRoot must be an absolute path.".to_owned(),
        ));
    }
    if record.contains_key("chatId") != record.contains_key("userId") {
        return Err(SettingsError(
            "telegram chatId and userId must be paired together.".to_owned(),
        ));
    }
    let (chat_id, user_id) = match (record.get("chatId"), record.get("userId")) {
        (None, None) => (None, None),
        (Some(Value::String(chat)), Some(Value::String(user))) => {
            if !is_telegram_id(chat) {
                return Err(SettingsError(
                    "telegram.chatId must be a private chat id.".to_owned(),
                ));
            }
            if !is_telegram_id(user) {
                return Err(SettingsError("telegram.userId is invalid.".to_owned()));
            }
            (Some(chat.clone()), Some(user.clone()))
        }
        _ => {
            if record.contains_key("chatId") {
                return Err(SettingsError(
                    "telegram.chatId must be a private chat id.".to_owned(),
                ));
            }
            return Err(SettingsError("telegram.userId is invalid.".to_owned()));
        }
    };
    Ok(TelegramSettings {
        enabled,
        token_env: token_env.to_owned(),
        workspace_root: workspace_root.to_owned(),
        chat_id,
        user_id,
    })
}

/// `/^\d{1,20}$/` — a Telegram chat/user id.
pub(super) fn is_telegram_id(value: &str) -> bool {
    (1..=20).contains(&value.chars().count()) && value.bytes().all(|byte| byte.is_ascii_digit())
}
