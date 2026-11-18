//! The schedule store: `schedules.json` — schedules are user records, not
//! daemon configuration (owner ruling 2026-11-18: the settings file never
//! carries them; a settings document that still does is refused by name).
//!
//! Sources: the schedule half of `settings.ts` moved here byte-preserved —
//! the per-schedule parser messages, the list shape and size refusal, the
//! duplicate-id refusal (all read 2026-11-18). Writes take the host's
//! store lock and go through the one atomic writer primitive.
//!
//! This is the store only: trigger *semantics* (adapter rules, ranges,
//! timestamps) belong to `dispatch::schedule`.

use std::collections::BTreeSet;
use std::path::Path;

use crate::settings::transaction;
use crate::shape::{as_record, is_id_token, js_len, json_size};

/// One scheduled instruction (`{adapter, options}` trigger).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
/// One stored schedule (id, workspace, trigger, instructions).
pub struct ScheduleState {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub workspace_root: String,
    pub origin_session_id: String,
    pub instruction: String,
    pub trigger: ScheduleTrigger,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
/// The trigger: adapter name plus its options object.
pub struct ScheduleTrigger {
    pub adapter: String,
    pub options: serde_json::Value,
}

/// A store refusal. The message *is* the contract.
#[derive(Debug, thiserror::Error)]
/// Why a schedules document was refused.
pub enum ScheduleError {
    #[error("{0}")]
    Refusal(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// The schedule record's closed key set (`parseScheduleState`).
const SCHEDULE_KEYS: [&str; 7] = [
    "id",
    "name",
    "enabled",
    "workspaceRoot",
    "originSessionId",
    "instruction",
    "trigger",
];

/// The list limits the settings half enforced — same messages.
const MAX_SCHEDULES: usize = 256;

/// `schedules.json`: absent is an empty store (reading creates nothing);
/// any other failure is refused by message.
pub fn load(data_dir: &Path) -> Result<Vec<ScheduleState>, ScheduleError> {
    let path = data_dir.join("schedules.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(ScheduleError::Io(error)),
    };
    let document: serde_json::Value =
        serde_json::from_str(&text).map_err(|error| ScheduleError::Refusal(error.to_string()))?;
    let Some(items) = document.as_array() else {
        return Err(ScheduleError::Refusal(list_refusal()));
    };
    if items.len() > MAX_SCHEDULES {
        return Err(ScheduleError::Refusal(list_refusal()));
    }
    let mut schedules = Vec::with_capacity(items.len());
    for item in items {
        schedules.push(parse_schedule_state(item)?);
    }
    let mut seen = BTreeSet::new();
    for schedule in &schedules {
        if !seen.insert(schedule.id.as_str()) {
            return Err(ScheduleError::Refusal(
                "schedules contains duplicate schedule ids.".to_owned(),
            ));
        }
    }
    Ok(schedules)
}

/// Write the whole list: the host's store lock, then the one atomic
/// writer — a partial document never exists.
pub fn save(data_dir: &Path, schedules: &[ScheduleState]) -> Result<(), ScheduleError> {
    let _lock = transaction::acquire(data_dir, transaction::default_wait())
        .map_err(|error| ScheduleError::Refusal(error.to_string()))?;
    let mut document = serde_json::to_string_pretty(schedules)
        .map_err(|error| ScheduleError::Refusal(error.to_string()))?;
    document.push('\n');
    transaction::atomic_write(&data_dir.join("schedules.json"), document.as_bytes())?;
    Ok(())
}

fn list_refusal() -> String {
    "schedules must be a list of up to 256 schedules.".to_owned()
}

/// `parseScheduleState` — the shape rules with the source's exact messages
/// (trigger *semantics* validate elsewhere, on add/update).
pub fn parse_schedule_state(raw: &serde_json::Value) -> Result<ScheduleState, ScheduleError> {
    let refused = |message: &str| ScheduleError::Refusal(message.to_owned());
    let Some(record) = as_record(raw) else {
        return Err(refused("A Schedule must be a mapping."));
    };
    for key in record.keys() {
        if !SCHEDULE_KEYS.contains(&key.as_str()) {
            return Err(ScheduleError::Refusal(format!(
                "Schedule {key} is not a supported setting."
            )));
        }
    }
    let id = record
        .get("id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if !is_id_token(id, 64) {
        return Err(refused(
            "Schedule id must use 1-64 letters, digits, underscores, or dashes.",
        ));
    }
    let name = record
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if name.trim().is_empty() || js_len(name) > 128 {
        return Err(refused("Schedule name must use 1-128 characters."));
    }
    let enabled = match record.get("enabled") {
        None | Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::Bool(flag)) => *flag,
        Some(_) => return Err(refused("Schedule enabled must be a boolean.")),
    };
    let workspace_root = record
        .get("workspaceRoot")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if js_len(workspace_root) > 32_768 || !crate::shape::is_absolute_path(workspace_root) {
        return Err(refused("Schedule workspaceRoot must be an absolute path."));
    }
    let origin_session_id = match record.get("originSessionId") {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(text)) if js_len(text) <= 256 => text.clone(),
        Some(_) => {
            return Err(refused(
                "Schedule originSessionId must be a session id under 256 characters.",
            ));
        }
    };
    let instruction = record
        .get("instruction")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if instruction.trim().is_empty() || js_len(instruction) > 16_000 {
        return Err(refused("Schedule instruction must use 1-16000 characters."));
    }
    let Some(trigger) = record.get("trigger").and_then(as_record) else {
        return Err(refused(
            "Schedule trigger must contain adapter and options.",
        ));
    };
    for key in trigger.keys() {
        if key != "adapter" && key != "options" {
            return Err(refused(
                "Schedule trigger must contain adapter and options.",
            ));
        }
    }
    let adapter = trigger
        .get("adapter")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if !is_id_token(adapter, 64) {
        return Err(refused("Schedule trigger adapter is invalid."));
    }
    let Some(options) = trigger.get("options").and_then(as_record) else {
        return Err(refused(
            "Schedule trigger options must be a mapping under 16000 characters.",
        ));
    };
    let size =
        json_size(&serde_json::Value::Object(options.clone())).map_err(ScheduleError::Refusal)?;
    if size > 16_000 {
        return Err(refused(
            "Schedule trigger options must be a mapping under 16000 characters.",
        ));
    }
    Ok(ScheduleState {
        id: id.to_owned(),
        name: name.to_owned(),
        enabled,
        workspace_root: workspace_root.to_owned(),
        origin_session_id,
        instruction: instruction.to_owned(),
        trigger: ScheduleTrigger {
            adapter: adapter.to_owned(),
            options: serde_json::Value::Object(options.clone()),
        },
    })
}
