//! The schedule screens: the empty sentences and the table shape.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;

use common::temp_data_dir;
use openbutler_daemon::dispatch::{self, Host};
use openbutler_protocol::{DaemonRequest, DaemonResponse};
use serde_json::{Value, json};

fn call(dir: &Path, id: &str, payload: Value) -> DaemonResponse {
    let host = Host::new();
    let mut state = dispatch::DispatchState::new();
    let mut request = DaemonRequest::new(id, "schedule.manage");
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
fn an_empty_list_answers_the_source_sentence() {
    let dir = temp_data_dir("schedule-lines-empty");
    let rendered = texts(call(&dir, "s1", json!({ "input": { "action": "list" } })));
    assert_eq!(
        rendered,
        vec![
            "No schedules are configured. Ask OpenButler to create one, or use `openbutler schedule add <schedule.json>`."
                .to_owned()
        ]
    );
}

#[test]
fn enable_disable_and_remove_answer_the_source_sentences() {
    let dir = temp_data_dir("schedule-sentences");
    let mut add = json!({ "input": {
        "action": "add",
        "schedule": {
            "name": "morning",
            "trigger": { "adapter": "daily", "options": { "at": "09:00" } },
            "instruction": "say hi"
        }
    }});
    add["input"]["schedule"]["workspaceRoot"] = json!(dir.display().to_string());
    let id = match call(&dir, "s2", add) {
        DaemonResponse::Success { result, .. } => {
            result["schedule"]["id"].as_str().expect("an id").to_owned()
        }
        DaemonResponse::Failure { error, .. } => panic!("add refused: {error}"),
    };
    for (index, action) in ["disable", "enable", "remove"].iter().enumerate() {
        let rendered = texts(call(
            &dir,
            &format!("s{}", index + 3),
            json!({ "input": { "action": action, "scheduleId": id } }),
        ));
        let expected = match *action {
            "disable" => format!("Schedule {id} disabled."),
            "enable" => format!("Schedule {id} enabled."),
            _ => format!("Schedule {id} removed. Run history stays in the ledger."),
        };
        assert_eq!(rendered, vec![expected], "{action}");
    }
}

#[test]
fn an_empty_log_answers_the_source_sentence() {
    let dir = temp_data_dir("schedule-log-empty");
    let mut add = json!({ "input": {
        "action": "add",
        "schedule": {
            "name": "morning",
            "trigger": { "adapter": "daily", "options": { "at": "09:00" } },
            "instruction": "say hi"
        }
    }});
    add["input"]["schedule"]["workspaceRoot"] = json!(dir.display().to_string());
    let id = match call(&dir, "l1", add) {
        DaemonResponse::Success { result, .. } => {
            result["schedule"]["id"].as_str().expect("an id").to_owned()
        }
        DaemonResponse::Failure { error, .. } => panic!("add refused: {error}"),
    };
    let rendered = texts(call(
        &dir,
        "l2",
        json!({ "input": { "action": "log", "scheduleId": id } }),
    ));
    assert_eq!(rendered, vec![format!("No runs recorded for {id}.")]);
}
