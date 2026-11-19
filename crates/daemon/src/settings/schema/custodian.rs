//! Moved from settings/schema.rs by the split (see dispatch-style refactor notes).
use serde_json::Value;

use super::helpers::whole_number_in_range;
use super::keys::{
    CUSTODIAN_CONTEXT_LIMIT_RANGE, CUSTODIAN_KEYS, CUSTODIAN_REPORT_GAP_RANGE,
    CUSTODIAN_WORKER_CAP_KEYS, known,
};
use super::{CustodianSettings, SettingsError, WorkerCaps};
use crate::shape::as_record;
pub fn parse_custodian_settings(raw: &Value) -> Result<CustodianSettings, SettingsError> {
    let Some(record) = as_record(raw) else {
        return Err(SettingsError("custodian must be a mapping".to_owned()));
    };
    for key in record.keys() {
        if !CUSTODIAN_KEYS.contains(&key.as_str()) {
            return Err(SettingsError(format!(
                "custodian.{key} is not a setting openbutler knows (known: {})",
                known(&CUSTODIAN_KEYS)
            )));
        }
    }
    let mut settings = CustodianSettings::default();
    let provider = record.get("provider");
    let model = record.get("model");
    if provider.is_some() || model.is_some() {
        let (Some(Value::String(provider)), Some(Value::String(model))) = (provider, model) else {
            return Err(SettingsError(
                "custodian.provider and custodian.model must both be non-empty strings, or both omitted."
                    .to_owned(),
            ));
        };
        if provider.trim().is_empty() || model.trim().is_empty() {
            return Err(SettingsError(
                "custodian.provider and custodian.model must both be non-empty strings, or both omitted."
                    .to_owned(),
            ));
        }
        settings.provider = Some(provider.trim().to_owned());
        settings.model = Some(model.trim().to_owned());
    }
    if let Some(value) = record.get("contextLimitTokens") {
        settings.context_limit_tokens = Some(whole_number_in_range(
            value,
            "custodian.contextLimitTokens",
            CUSTODIAN_CONTEXT_LIMIT_RANGE.0,
            CUSTODIAN_CONTEXT_LIMIT_RANGE.1,
        )?);
    }
    if let Some(value) = record.get("reportLine") {
        let Some(flag) = value.as_bool() else {
            return Err(SettingsError(
                "custodian.reportLine must be true or false.".to_owned(),
            ));
        };
        settings.report_line = Some(flag);
    }
    if let Some(value) = record.get("reportTurnGapSeconds") {
        settings.report_turn_gap_seconds = Some(whole_number_in_range(
            value,
            "custodian.reportTurnGapSeconds",
            CUSTODIAN_REPORT_GAP_RANGE.0,
            CUSTODIAN_REPORT_GAP_RANGE.1,
        )?);
    }
    if let Some(value) = record.get("workerCaps") {
        let Some(caps) = as_record(value) else {
            return Err(SettingsError(
                "custodian.workerCaps must be a mapping".to_owned(),
            ));
        };
        for key in caps.keys() {
            if !CUSTODIAN_WORKER_CAP_KEYS.contains(&key.as_str()) {
                return Err(SettingsError(format!(
                    "custodian.workerCaps.{key} is not a setting openbutler knows (known: {})",
                    known(&CUSTODIAN_WORKER_CAP_KEYS)
                )));
            }
        }
        let cap = |field: &str, min: u64, max: u64| -> Result<u64, SettingsError> {
            let default = match field {
                "perSession" => 8,
                "perWorkerParent" => 2,
                "perMachine" => 10,
                _ => 2,
            };
            match caps.get(field) {
                None | Some(Value::Null) => Ok(default),
                Some(value) => {
                    whole_number_in_range(value, &format!("custodian.workerCaps.{field}"), min, max)
                }
            }
        };
        settings.worker_caps = Some(WorkerCaps {
            per_session: cap("perSession", 1, 32)?,
            per_worker_parent: cap("perWorkerParent", 0, 8)?,
            per_machine: cap("perMachine", 1, 64)?,
            depth: cap("depth", 1, 3)?,
        });
    }
    Ok(settings)
}

// --- Secrets -----------------------------------------------------------------
