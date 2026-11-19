//! Moved from settings/schema.rs by the split (see dispatch-style refactor notes).
use std::collections::BTreeMap;

use serde_json::Value;

use super::SettingsError;
use super::helpers::{is_environment_name, is_header_name, is_mcp_id, string_array};
use super::telegram::environment_references;
use crate::shape::as_record;
/// `mcpServers.*.state`: enabled or disabled without deleting the config.
#[derive(Debug, Clone, PartialEq)]
pub enum McpServerState {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: Option<String>,
    },
    Http {
        transport: &'static str,
        url: String,
        headers: BTreeMap<String, String>,
    },
}

/// The wire form is one flat object with a `transport` discriminator; the
/// `http` variant carries its own `http`/`sse` value, so the tag cannot be
/// derived and the encoder is written out.
impl serde::Serialize for McpServerState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            McpServerState::Stdio {
                command,
                args,
                env,
                cwd,
            } => {
                let mut map = serializer.serialize_map(Some(4 + usize::from(cwd.is_some())))?;
                map.serialize_entry("transport", "stdio")?;
                map.serialize_entry("command", command)?;
                map.serialize_entry("args", args)?;
                map.serialize_entry("env", env)?;
                if let Some(cwd) = cwd {
                    map.serialize_entry("cwd", cwd)?;
                }
                map.end()
            }
            McpServerState::Http {
                transport,
                url,
                headers,
            } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("transport", transport)?;
                map.serialize_entry("url", url)?;
                map.serialize_entry("headers", headers)?;
                map.end()
            }
        }
    }
}

pub fn parse_mcp_server_state(raw: &Value, id: &str) -> Result<McpServerState, SettingsError> {
    if !is_mcp_id(id) {
        return Err(SettingsError(format!(
            "mcpServers.{id} has an invalid server id"
        )));
    }
    let Some(record) = as_record(raw) else {
        return Err(SettingsError(format!("mcpServers.{id} must be a mapping")));
    };
    match record.get("transport").and_then(Value::as_str) {
        Some("stdio") => {
            let allowed = ["transport", "command", "args", "env", "cwd"];
            for key in record.keys() {
                if !allowed.contains(&key.as_str()) {
                    return Err(SettingsError(format!(
                        "mcpServers.{id}.{key} is not a stdio setting"
                    )));
                }
            }
            let command = record.get("command").and_then(Value::as_str).unwrap_or("");
            if command.trim().is_empty() {
                return Err(SettingsError(format!(
                    "mcpServers.{id}.command must be a non-empty executable name"
                )));
            }
            let args = record
                .get("args")
                .map(|value| string_array(value, &format!("mcpServers.{id}.args")))
                .transpose()?
                .unwrap_or_default();
            let env = environment_references(
                record.get("env"),
                &format!("mcpServers.{id}.env"),
                is_environment_name,
            )?;
            let cwd = match record.get("cwd") {
                None | Some(Value::Null) => None,
                Some(Value::String(text)) if text.is_empty() => {
                    return Err(SettingsError(format!(
                        "mcpServers.{id}.cwd must be a non-empty path"
                    )));
                }
                Some(Value::String(text)) => Some(text.clone()),
                Some(_) => {
                    return Err(SettingsError(format!(
                        "mcpServers.{id}.cwd must be a non-empty path"
                    )));
                }
            };
            Ok(McpServerState::Stdio {
                command: command.to_owned(),
                args,
                env,
                cwd,
            })
        }
        Some("http") | Some("sse") => {
            let allowed = ["transport", "url", "headers"];
            for key in record.keys() {
                if !allowed.contains(&key.as_str()) {
                    return Err(SettingsError(format!(
                        "mcpServers.{id}.{key} is not an HTTP setting"
                    )));
                }
            }
            let Some(url_text) = record.get("url").and_then(Value::as_str) else {
                return Err(SettingsError(format!("mcpServers.{id}.url must be a URL")));
            };
            let Ok(url) = url::Url::parse(url_text) else {
                return Err(SettingsError(format!(
                    "mcpServers.{id}.url must be a valid HTTP or HTTPS URL"
                )));
            };
            if !matches!(url.scheme(), "http" | "https")
                || !url.username().is_empty()
                || url.password().is_some()
            {
                return Err(SettingsError(format!(
                    "mcpServers.{id}.url must use HTTP(S) and must not contain credentials"
                )));
            }
            let headers = environment_references(
                record.get("headers"),
                &format!("mcpServers.{id}.headers"),
                is_header_name,
            )?;
            Ok(McpServerState::Http {
                transport: if record.get("transport").and_then(Value::as_str) == Some("sse") {
                    "sse"
                } else {
                    "http"
                },
                url: url.to_string(),
                headers,
            })
        }
        _ => Err(SettingsError(format!(
            "mcpServers.{id}.transport must be stdio, http, or sse"
        ))),
    }
}
