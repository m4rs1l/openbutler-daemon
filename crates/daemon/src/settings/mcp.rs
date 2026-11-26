//! `openbutler mcp`: the global MCP server list, composed on the host.
//!
//! Source: `cli-main.ts` (`showMcpServers` L380, `runMcpManager` L398) for
//! the screen, the sentences and the id rule. The shape validation is the
//! settings schema's own parser (`parse_mcp_server_state`), so a server the
//! loader would refuse can never be written here — one authority for the
//! document's shape.

use std::path::Path;

use serde_json::{Map, Value, json};

use super::schema::{McpServerState, parse_mcp_server_state};
use super::{load_settings, save_settings};
use crate::dispatch::Host;

/// The source's id prompt validator: unique, 1-59 letters, digits,
/// underscores or dashes (the pattern is the schema's `is_mcp_id`).
const UNIQUE_ID: &str = "Use a unique id with 1-59 letters, digits, underscores, or dashes.";

/// Writes are local-only: a server definition is a command the host
/// spawns on its own machine, so installing or removing one over a remote
/// token would grant process execution the source never granted remotely.
/// Reading stays open to a remote caller.
pub const REMOTE_REFUSAL: &str = "MCP servers are managed on the host machine only.";

/// `mcp.manage`: `{command: "list" | "add" | "remove"}` (the envelope
/// already owns `action`, so the payload field carries the other name).
pub fn manage(
    host: &Host,
    data_dir: &Path,
    request: &openbutler_protocol::DaemonRequest,
    remote: bool,
) -> Result<Value, String> {
    match request.payload.get("command").and_then(Value::as_str) {
        None | Some("list") => list(data_dir),
        Some("add") | Some("remove") if remote => Err(REMOTE_REFUSAL.to_owned()),
        Some("add") => add(host, data_dir, &request.payload),
        Some("remove") => remove(host, data_dir, &request.payload),
        Some(_) => Err("mcp.manage action must be list, add, or remove.".to_owned()),
    }
}

/// The list screen: the section bar, one row per server (stdio shows the
/// command line it runs, HTTP shows its URL), the management hint and a
/// closing blank — every word the source's `showMcpServers` prints.
fn list(data_dir: &Path) -> Result<Value, String> {
    let loaded = load_settings(data_dir).map_err(|error| error.to_string())?;
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut servers: Vec<Value> = Vec::new();
    for (id, server) in &loaded.store.mcp_servers {
        let (transport, endpoint) = match server {
            McpServerState::Stdio { command, args, .. } => {
                let mut parts = vec![command.clone()];
                parts.extend(args.iter().cloned());
                ("stdio".to_owned(), parts.join(" "))
            }
            McpServerState::Http { transport, url, .. } => ((*transport).to_owned(), url.clone()),
        };
        rows.push(vec![id.clone(), transport.clone(), endpoint.clone()]);
        servers.push(json!({
            "id": id,
            "transport": transport,
            "endpoint": endpoint,
        }));
    }
    let mut lines = vec![crate::shape::section_line("MCP servers")];
    if rows.is_empty() {
        lines.push(crate::shape::screen_line(
            "info",
            "No MCP servers configured.",
        ));
    } else {
        lines.extend(crate::shape::table_lines(
            &["SERVER", "TRANSPORT", "ENDPOINT"],
            &rows,
        ));
    }
    lines.push(crate::shape::screen_line(
        "info",
        "Manage global servers with openbutler mcp. Credentials are environment-variable references, not saved values.",
    ));
    lines.push(crate::shape::screen_line("plain", ""));
    Ok(json!({ "servers": servers, "lines": lines }))
}

/// Adds one server: the id must be unseen, the body must survive the
/// schema's own parse, then the settings pair is saved and the live copy
/// reloaded (the source saved from the CLI process).
fn add(host: &Host, data_dir: &Path, payload: &Map<String, Value>) -> Result<Value, String> {
    let id = payload
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "id must be a non-empty string.".to_owned())?;
    let server = payload
        .get("server")
        .ok_or_else(|| "server must be a mapping.".to_owned())?;
    let mut loaded = load_settings(data_dir).map_err(|error| error.to_string())?;
    if loaded.store.mcp_servers.contains_key(id) {
        return Err(UNIQUE_ID.to_owned());
    }
    let parsed = parse_mcp_server_state(server, id).map_err(|error| error.0.clone())?;
    loaded.store.mcp_servers.insert(id.to_owned(), parsed);
    save_settings(data_dir, &mut loaded).map_err(|error| error.to_string())?;
    host.reload_settings(data_dir)
        .map_err(|error| error.to_string())?;
    Ok(json!({
        "id": id,
        "lines": [crate::shape::screen_line(
            "success",
            format!("Saved global MCP server {id}. OpenButler will ask before connecting at session start."),
        )],
    }))
}

/// Removes one server; an unknown id is a refusal, not a silent success.
fn remove(host: &Host, data_dir: &Path, payload: &Map<String, Value>) -> Result<Value, String> {
    let id = payload
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "id must be a non-empty string.".to_owned())?;
    let mut loaded = load_settings(data_dir).map_err(|error| error.to_string())?;
    if loaded.store.mcp_servers.remove(id).is_none() {
        return Err(format!("mcpServers.{id} is not configured."));
    }
    save_settings(data_dir, &mut loaded).map_err(|error| error.to_string())?;
    host.reload_settings(data_dir)
        .map_err(|error| error.to_string())?;
    Ok(json!({
        "id": id,
        "removed": true,
        "lines": [crate::shape::screen_line("success", format!("Removed MCP server {id}."))],
    }))
}
