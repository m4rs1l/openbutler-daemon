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
