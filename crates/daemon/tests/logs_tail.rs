//! `logs.tail` pins: the tail the CLI used to compute (blocks read backwards,
//! CRLF stripped, an unreadable file answers like an empty one), the absolute
//! path rule, the window guard and the local-only gate.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;

use common::temp_data_dir;
use openbutler_daemon::dispatch::{DispatchState, Host, handle};
use openbutler_protocol::{DaemonRequest, DaemonResponse};
use serde_json::{Value, json};

fn call(host: &Host, dir: &Path, payload: Value) -> DaemonResponse {
    let mut request = DaemonRequest::new("r1", "logs.tail");
    if let Value::Object(fields) = payload {
        request.payload = fields;
    }
    handle(host, dir, &mut DispatchState::new(), &request)
}

fn error_of(response: DaemonResponse) -> String {
    let DaemonResponse::Failure { error, .. } = response else {
        panic!("expected a refusal, got {response:?}");
    };
    error
}

fn texts(response: DaemonResponse) -> Vec<String> {
    let DaemonResponse::Success { result, .. } = response else {
        panic!("expected success, got {response:?}");
    };
    result["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line["text"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn the_tail_keeps_the_last_lines_with_their_tone() {
    let dir = temp_data_dir("logs-tail");
    std::fs::create_dir_all(dir.join("logs")).unwrap();
    std::fs::write(
        dir.join("logs").join("openbutler.log"),
        "one\r\ntwo\r\nthree\r\nfour\r\nfive\r\n",
    )
    .unwrap();
    let host = Host::new();

    let DaemonResponse::Success { result, .. } = call(&host, &dir, json!({ "lines": 3 })) else {
        panic!("expected success");
    };
    let lines = result["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["tone"], json!("plain"));
    assert_eq!(lines[0]["text"], json!("three"));
    assert_eq!(lines[2]["text"], json!("five"));
}

#[test]
fn a_missing_file_answers_the_empty_sentence_with_its_path() {
    let dir = temp_data_dir("logs-missing");
    let host = Host::new();
    let path = dir.join("logs").join("openbutler.log");
    assert_eq!(
        texts(call(&host, &dir, json!({}))),
        vec![format!("No log lines at {}.", path.display())]
    );
}

#[test]
fn an_absolute_file_wins() {
    let dir = temp_data_dir("logs-file");
    let other = dir.join("elsewhere.log");
    std::fs::write(&other, "alpha\nbeta\n").unwrap();
    let host = Host::new();

    let payload = json!({ "lines": 10, "file": other.to_string_lossy() });
    assert_eq!(texts(call(&host, &dir, payload)), vec!["alpha", "beta"]);
}

#[test]
fn a_relative_file_is_refused() {
    let dir = temp_data_dir("logs-relative");
    let host = Host::new();
    assert_eq!(
        error_of(call(&host, &dir, json!({ "file": "daemon.log" }))),
        "file must be an absolute path."
    );
}

#[test]
fn the_window_is_guarded() {
    let dir = temp_data_dir("logs-window");
    let host = Host::new();
    for payload in [
        json!({ "lines": 0 }),
        json!({ "lines": 100_001 }),
        json!({ "lines": "10" }),
    ] {
        assert_eq!(
            error_of(call(&host, &dir, payload)),
            "lines must be a whole number between 1 and 100000."
        );
    }
}

#[test]
fn a_remote_connection_cannot_read_host_files() {
    let dir = temp_data_dir("logs-remote");
    let host = Host::new();
    let mut request = DaemonRequest::new("r1", "logs.tail");
    if let Value::Object(fields) = json!({}) {
        request.payload = fields;
    }
    let mut state = DispatchState::new();
    state.set_remote(true);
    assert_eq!(
        error_of(handle(&host, &dir, &mut state, &request)),
        "Log files are read on the host machine only."
    );
}
