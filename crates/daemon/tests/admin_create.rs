#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `admin.create`: the account is created on the host machine only, and the
//! answer carries the host's own sentence (the client prints, it does not
//! compose).

mod common;

use common::temp_data_dir;
use openbutler_daemon::auth::AdminAccount;
use openbutler_daemon::dispatch::{self, DispatchState, Host};
use openbutler_protocol::{DaemonRequest, DaemonResponse};
use serde_json::Value;

fn create(dir: &std::path::Path, remote: bool) -> Result<Value, String> {
    let host = Host::new();
    let mut state = DispatchState::new();
    state.remote = remote;
    let request = DaemonRequest::new("admin-1", "admin.create")
        .with("username", "owner")
        .with("password", "correct horse battery");
    match dispatch::handle(&host, dir, &mut state, &request) {
        DaemonResponse::Success { result, .. } => Ok(result),
        DaemonResponse::Failure { error, .. } => Err(error),
    }
}

#[test]
fn a_local_create_writes_the_account_and_answers_with_one_line() {
    let dir = temp_data_dir("admin-create-local");
    let result = create(&dir, false).expect("the local host creates the account");

    assert_eq!(result["username"], "owner");
    let lines = result["lines"]
        .as_array()
        .expect("the answer carries lines");
    assert_eq!(lines.len(), 1, "one sentence, composed by the host");
    assert_eq!(lines[0]["tone"], "success");
    let text = lines[0]["text"].as_str().expect("text");
    assert!(
        text.contains("Admin account owner created"),
        "the host composes the sentence: {text}"
    );
    assert!(
        AdminAccount::new(&dir).is_configured(),
        "the account is on disk"
    );
}

#[test]
fn a_remote_create_is_refused_and_writes_nothing() {
    let dir = temp_data_dir("admin-create-remote");
    let error = create(&dir, true).expect_err("a remote caller must be refused");
    assert_eq!(
        error,
        "The admin account is created on the host machine only."
    );
    assert!(
        !AdminAccount::new(&dir).is_configured(),
        "a refused create must not touch the disk"
    );
}

#[test]
fn a_second_create_answers_with_the_store_s_refusal() {
    let dir = temp_data_dir("admin-create-twice");
    create(&dir, false).expect("first create");
    let error = create(&dir, false).expect_err("the second is refused");
    assert!(
        error.contains("already exists"),
        "the refusal comes from the store: {error}"
    );
}
