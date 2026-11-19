//! Moved from settings/schema.rs by the split (see dispatch-style refactor notes).
use std::collections::BTreeMap;

use serde_json::Value;

use super::helpers::integer_in_range;
use super::keys::TEAM_KEYS;
use super::{SettingsError, TeamRole};
use crate::shape::{as_record, js_len};
/// `team`: name, member, and role (the connection itself lives in Rust).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamSettings {
    pub role: TeamRole,
    pub team_name: String,
    pub member_name: String,
    pub hub_host: String,
    pub hub_port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hub_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
}

/// Worker caps of the custodian, filled per field when the block is present.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
/// `workerCaps`: per-worker tool grants (what a spawn may use).
pub struct WorkerCaps {
    pub per_session: u64,
    pub per_worker_parent: u64,
    pub per_machine: u64,
    pub depth: u64,
}

/// The head's settings, validated where they are read: a wrong value is
/// refused by name and range instead of silently becoming the default.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
/// `custodian`: the machine custodian's model and identity settings.
pub struct CustodianSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_limit_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_line: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_turn_gap_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_caps: Option<WorkerCaps>,
}

/// A role → provider/model binding; whole pairs only.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
/// `roleBindings.*`: a named model role bound to provider/model.
pub struct RoleBinding {
    pub provider: String,
    pub model: String,
}

pub fn parse_roles(value: &Value) -> Result<Option<BTreeMap<String, RoleBinding>>, SettingsError> {
    if value.is_null() {
        return Ok(None);
    }
    let Some(record) = as_record(value) else {
        return Err(SettingsError(
            "roles must be a mapping of role name to a { provider, model } pair.".to_owned(),
        ));
    };
    let mut roles = BTreeMap::new();
    for (role, raw) in record {
        if raw.is_null() {
            continue;
        }
        let Some(pair) = as_record(raw) else {
            return Err(SettingsError(format!(
                "roles.{role} must be a {{ provider, model }} pair."
            )));
        };
        let provider = pair.get("provider");
        let model = pair.get("model");
        if provider.is_none_or(Value::is_null) && model.is_none_or(Value::is_null) {
            continue;
        }
        let (Some(Value::String(provider)), Some(Value::String(model))) = (provider, model) else {
            return Err(SettingsError(format!(
                "roles.{role}.provider and roles.{role}.model must both be non-empty strings, or the role must be omitted."
            )));
        };
        if provider.trim().is_empty() || model.trim().is_empty() {
            return Err(SettingsError(format!(
                "roles.{role}.provider and roles.{role}.model must both be non-empty strings, or the role must be omitted."
            )));
        }
        roles.insert(
            role.clone(),
            RoleBinding {
                provider: provider.trim().to_owned(),
                model: model.trim().to_owned(),
            },
        );
    }
    Ok((!roles.is_empty()).then_some(roles))
}

pub fn parse_team_settings(raw: &Value) -> Result<TeamSettings, SettingsError> {
    let Some(record) = as_record(raw) else {
        return Err(SettingsError("team must be a mapping.".to_owned()));
    };
    for key in record.keys() {
        if !TEAM_KEYS.contains(&key.as_str()) {
            return Err(SettingsError(format!(
                "team.{key} is not a supported setting."
            )));
        }
    }
    let role = match record.get("role").and_then(Value::as_str) {
        Some("hub") => TeamRole::Hub,
        Some("member") => TeamRole::Member,
        _ => return Err(SettingsError("team.role must be hub or member.".to_owned())),
    };
    let bounded = |field: &str, max: usize, message: &str| -> Result<String, SettingsError> {
        let Some(text) = record.get(field).and_then(Value::as_str) else {
            return Err(SettingsError(message.to_owned()));
        };
        if text.trim().is_empty() || js_len(text) > max {
            return Err(SettingsError(message.to_owned()));
        }
        Ok(text.to_owned())
    };
    let team_name = bounded("teamName", 64, "team.teamName must be 1-64 characters.")?;
    let member_name = bounded("memberName", 64, "team.memberName must be 1-64 characters.")?;
    let hub_host = bounded(
        "hubHost",
        512,
        "team.hubHost must be a non-empty host under 512 characters.",
    )?;
    let hub_port = record
        .get("hubPort")
        .ok_or_else(|| SettingsError("team.hubPort must be a port 1-65535.".to_owned()))?;
    // TS wraps every hubPort failure (type, integer, range) in the one port message.
    let hub_port = integer_in_range(hub_port, "team.hubPort", 1, 65_535)
        .map_err(|_| SettingsError("team.hubPort must be a port 1-65535.".to_owned()))?;
    let hub_fingerprint = match record.get("hubFingerprint") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if js_len(text) <= 64 => Some(text.clone()),
        Some(_) => {
            return Err(SettingsError(
                "team.hubFingerprint must be a short string.".to_owned(),
            ));
        }
    };
    let team_id = match record.get("teamId") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if js_len(text) <= 64 => Some(text.clone()),
        Some(_) => {
            return Err(SettingsError(
                "team.teamId must be a short string.".to_owned(),
            ));
        }
    };
    Ok(TeamSettings {
        role,
        team_name,
        member_name,
        hub_host,
        hub_port: hub_port as u16,
        hub_fingerprint,
        team_id,
    })
}
