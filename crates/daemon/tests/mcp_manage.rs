//! `mcp.manage` pins: the screen the source prints, the schema's own
//! refusals, and the document surviving a reload.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

mod common;

use common::temp_data_dir;
use openbutler_daemon::dispatch::{self, Host};
use openbutler_daemon::settings::load_settings;
use openbutler_protocol::{DaemonRequest, DaemonResponse};
use serde_json::{Value, json};

/// One call on a fresh host: every run reloads from disk, the way two CLI
/// invocations would.
fn call(dir: &Path, id: &str, payload: Value) -> DaemonResponse {
    let host = Host::new();
    let mut state = dispatch::DispatchState::new();
    let mut request = DaemonRequest::new(id, "mcp.manage");
    if let Value::Object(fields) = payload {
        for (key, value) in fields {
            request.payload.insert(key, value);
        }
    }
    dispatch::handle(&host, dir, &mut state, &request)
}

fn ok(dir: &Path, id: &str, payload: Value) -> Value {
    match call(dir, id, payload) {
        DaemonResponse::Success { result, .. } => result,
        DaemonResponse::Failure { error, .. } => panic!("expected success, got {error}"),
    }
}

fn refusal(dir: &Path, id: &str, payload: Value) -> String {
    match call(dir, id, payload) {
        DaemonResponse::Failure { error, .. } => error,
        DaemonResponse::Success { result, .. } => panic!("expected a refusal, got {result}"),
    }
}

fn texts(result: &Value) -> Vec<String> {
    result["lines"]
        .as_array()
        .expect("lines")
        .iter()
        .map(|line| line["text"].as_str().expect("text").to_owned())
        .collect()
}

/// A stdio server definition is a command the host spawns on its own
/// machine, so installing or removing one over a remote token would grant
/// process execution the source never granted remotely. Reading stays open.
#[test]
fn a_remote_connection_cannot_install_or_remove_a_server() {
    let dir = temp_data_dir("mcp-remote");
    let host = Host::new();

    // Local: the install lands, so the remote remove has something to hit.
    let mut local = dispatch::DispatchState::new();
    let mut add = DaemonRequest::new("l1", "mcp.manage");
    add.payload.insert("command".to_owned(), json!("add"));
    add.payload.insert("id".to_owned(), json!("local"));
    add.payload.insert("server".to_owned(), stdio_server());
    let DaemonResponse::Success { .. } = dispatch::handle(&host, &dir, &mut local, &add) else {
        panic!("the local install must land");
    };

    let mut remote = dispatch::DispatchState::new();
    remote.set_remote(true);

    // Reads are still allowed to a remote caller.
    let mut list = DaemonRequest::new("r1", "mcp.manage");
    list.payload.insert("command".to_owned(), json!("list"));
    let DaemonResponse::Success { result, .. } = dispatch::handle(&host, &dir, &mut remote, &list)
    else {
        panic!("a remote caller may read the list");
    };
    assert_eq!(
        texts(&result)
            .iter()
            .filter(|line| line.contains("local"))
            .count(),
        1
    );

    // Every mutation is refused, with the one sentence.
    let mut add = DaemonRequest::new("r2", "mcp.manage");
    add.payload.insert("command".to_owned(), json!("add"));
    add.payload.insert("id".to_owned(), json!("remote"));
    add.payload.insert("server".to_owned(), stdio_server());
    let DaemonResponse::Failure { error, .. } = dispatch::handle(&host, &dir, &mut remote, &add)
    else {
        panic!("a remote install must be refused");
    };
    assert_eq!(error, "MCP servers are managed on the host machine only.");

    let mut remove = DaemonRequest::new("r3", "mcp.manage");
    remove.payload.insert("command".to_owned(), json!("remove"));
    remove.payload.insert("id".to_owned(), json!("local"));
    let DaemonResponse::Failure { error, .. } = dispatch::handle(&host, &dir, &mut remote, &remove)
    else {
        panic!("a remote removal must be refused");
    };
    assert_eq!(error, "MCP servers are managed on the host machine only.");

    // Neither refusal touched the document.
    let loaded = load_settings(&dir).expect("settings");
    assert!(loaded.store.mcp_servers.contains_key("local"));
    assert!(!loaded.store.mcp_servers.contains_key("remote"));
}

fn stdio_server() -> Value {
    json!({
        "transport": "stdio",
        "command": "npx",
        "args": ["-y", "server"],
        "env": { "TOKEN": "HOST_TOKEN" },
    })
}

#[test]
fn list_answers_the_source_screen() {
    let dir = temp_data_dir("mcp-list-empty");
    let rendered = texts(&ok(&dir, "m1", json!({ "command": "list" })));
    let bar = format!("\u{2500}\u{2500} MCP servers {}", "\u{2500}".repeat(42));
    assert_eq!(rendered[0], bar);
    assert_eq!(rendered[1], "No MCP servers configured.");
    assert_eq!(
        rendered[2],
        "Manage global servers with openbutler mcp. Credentials are environment-variable references, not saved values."
    );
    assert_eq!(rendered[3], "");
    assert_eq!(rendered.len(), 4);
}

#[test]
fn add_persists_answers_and_lists() {
    let dir = temp_data_dir("mcp-add");
    let saved = ok(
        &dir,
        "m2",
        json!({ "command": "add", "id": "acme", "server": stdio_server() }),
    );
    assert_eq!(
        texts(&saved),
        vec![
            "Saved global MCP server acme. OpenButler will ask before connecting at session start."
                .to_owned()
        ]
    );
    // The document really moved: a fresh read sees the server.
    let loaded = load_settings(&dir).expect("load");
    assert!(loaded.store.mcp_servers.contains_key("acme"));

    let listed = ok(&dir, "m3", json!({ "command": "list" }));
    assert_eq!(listed["servers"][0]["id"], "acme");
    assert_eq!(listed["servers"][0]["transport"], "stdio");
    assert_eq!(listed["servers"][0]["endpoint"], "npx -y server");
    let rendered = texts(&listed);
    let row = rendered
        .iter()
        .find(|line| line.starts_with("  acme"))
        .expect("the server row");
    assert!(row.contains("stdio"), "row: {row}");
    assert!(row.contains("npx -y server"), "row: {row}");
}

#[test]
fn add_refuses_a_duplicate_id() {
    let dir = temp_data_dir("mcp-dup");
    ok(
        &dir,
        "m4",
        json!({ "command": "add", "id": "acme", "server": stdio_server() }),
    );
    assert_eq!(
        refusal(
            &dir,
            "m5",
            json!({ "command": "add", "id": "acme", "server": stdio_server() })
        ),
        "Use a unique id with 1-59 letters, digits, underscores, or dashes."
    );
}

#[test]
fn add_refuses_a_body_the_loader_would() {
    let dir = temp_data_dir("mcp-shape");
    assert_eq!(
        refusal(
            &dir,
            "m6",
            json!({ "command": "add", "id": "acme", "server": { "transport": "stdio", "command": "  " } })
        ),
        "mcpServers.acme.command must be a non-empty executable name"
    );
    assert_eq!(
        refusal(
            &dir,
            "m7",
            json!({ "command": "add", "id": "bad id", "server": stdio_server() })
        ),
        "mcpServers.bad id has an invalid server id"
    );
    // Nothing reached the document.
    assert!(
        load_settings(&dir)
            .expect("load")
            .store
            .mcp_servers
            .is_empty()
    );
}

#[test]
fn remove_answers_and_refuses_an_unknown_id() {
    let dir = temp_data_dir("mcp-remove");
    ok(
        &dir,
        "m8",
        json!({ "command": "add", "id": "acme", "server": stdio_server() }),
    );
    let removed = ok(&dir, "m9", json!({ "command": "remove", "id": "acme" }));
    assert_eq!(texts(&removed), vec!["Removed MCP server acme.".to_owned()]);
    assert!(
        load_settings(&dir)
            .expect("load")
            .store
            .mcp_servers
            .is_empty()
    );
    assert_eq!(
        refusal(&dir, "m10", json!({ "command": "remove", "id": "acme" })),
        "mcpServers.acme is not configured."
    );
}

#[test]
fn an_unknown_action_is_refused() {
    let dir = temp_data_dir("mcp-action");
    assert_eq!(
        refusal(&dir, "m11", json!({ "command": "frobnicate" })),
        "mcp.manage action must be list, add, or remove."
    );
}
