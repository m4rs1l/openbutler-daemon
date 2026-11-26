//! `logs.tail` over the host's own structured log: the default path, the
//! level/event filters, the JSON mode and the field refusals.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;

use common::temp_data_dir;
use openbutler_daemon::dispatch::{self, Host};
use openbutler_protocol::{DaemonRequest, DaemonResponse};
use serde_json::{Value, json};

const LOG: &str = concat!(
    r#"{"ts":"2026-11-26T09:00:00.000Z","level":"info","event":"host.started","pid":7}"#,
    "\n",
    r#"{"ts":"2026-11-26T09:00:01.000Z","level":"warn","event":"host.log","message":"slow"}"#,
    "\n",
    r#"{"ts":"2026-11-26T09:00:02.000Z","level":"error","event":"ipc.failed","message":"boom"}"#,
    "\n",
    "a plain line the parser ignores\n",
);

fn seed(dir: &Path) {
    let logs = dir.join("logs");
    std::fs::create_dir_all(&logs).expect("logs dir");
    std::fs::write(logs.join("openbutler.log"), LOG).expect("write");
}

fn call(dir: &Path, id: &str, payload: Value) -> DaemonResponse {
    let host = Host::new();
    let mut state = dispatch::DispatchState::new();
    let mut request = DaemonRequest::new(id, "logs.tail");
    if let Value::Object(fields) = payload {
        for (key, value) in fields {
            request.payload.insert(key, value);
        }
    }
    dispatch::handle(&host, dir, &mut state, &request)
}

fn texts(response: DaemonResponse) -> Vec<String> {
    match response {
        DaemonResponse::Success { result, .. } => result["lines"]
            .as_array()
            .expect("lines")
            .iter()
            .map(|line| line["text"].as_str().expect("text").to_owned())
            .collect(),
        DaemonResponse::Failure { error, .. } => panic!("expected lines, got {error}"),
    }
}

#[test]
fn the_default_subject_is_the_hosts_own_log() {
    let dir = temp_data_dir("logs-default");
    seed(&dir);
    let rendered = texts(call(&dir, "g1", json!({ "lines": 10 })));
    // Formatted records and the untouched plain line, oldest first.
    assert!(
        rendered[0].contains("INFO  host.started pid=7"),
        "{rendered:?}"
    );
    assert!(
        rendered[1].contains("WARN  host.log message=slow"),
        "{rendered:?}"
    );
    assert_eq!(rendered[3], "a plain line the parser ignores");
}

#[test]
fn the_level_filter_keeps_only_that_level() {
    let dir = temp_data_dir("logs-level");
    seed(&dir);
    let rendered = texts(call(&dir, "g2", json!({ "lines": 10, "level": "warn" })));
    assert_eq!(rendered.len(), 1, "{rendered:?}");
    assert!(rendered[0].contains("WARN  host.log"), "{rendered:?}");
}

#[test]
fn the_event_filter_matches_the_name_exactly() {
    let dir = temp_data_dir("logs-event");
    seed(&dir);
    let rendered = texts(call(
        &dir,
        "g3",
        json!({ "lines": 10, "event": "ipc.failed" }),
    ));
    assert_eq!(rendered.len(), 1, "{rendered:?}");
    assert!(rendered[0].contains("ERROR ipc.failed"), "{rendered:?}");
}

#[test]
fn the_json_mode_keeps_the_records_raw() {
    let dir = temp_data_dir("logs-json");
    seed(&dir);
    let rendered = texts(call(&dir, "g4", json!({ "lines": 10, "format": "json" })));
    assert!(
        rendered[0].starts_with("{\"ts\":\"2026-11-26T09:00:00.000Z\""),
        "{rendered:?}"
    );
}

#[test]
fn a_bad_level_is_refused_by_name() {
    let dir = temp_data_dir("logs-bad-level");
    seed(&dir);
    match call(&dir, "g5", json!({ "lines": 10, "level": "loud" })) {
        DaemonResponse::Failure { error, .. } => {
            assert_eq!(error, "level must be error, warn, info, or debug.")
        }
        DaemonResponse::Success { result, .. } => panic!("expected a refusal, got {result}"),
    }
}
