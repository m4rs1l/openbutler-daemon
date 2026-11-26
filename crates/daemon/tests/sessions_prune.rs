//! `sessions.prune` pins: the selection from `cli-main.ts` `runSessionsCommand`
//! (L1465-1526), `retention.ts` `selectFilesToPrune` and `session-store.ts`
//! `listSessionFiles` / `removeSavedSession` (read 2026-11-26) — a strict
//! modification-time cutoff, the custodian never selected, a preview that
//! deletes nothing, an apply that deletes exactly the selection, and the
//! local-only gate.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::temp_data_dir;
use openbutler_daemon::dispatch::{DispatchState, Host, handle};
use openbutler_protocol::{DaemonRequest, DaemonResponse};
use serde_json::{Value, json};

const OLD: u64 = 1_577_934_245; // 2020-01-02T03:04:05Z, the row's UTC date
const CUSTODIAN: &str = "00000000-0000-4000-8000-000000000000";
const OLD_ID: &str = "11111111-1111-4111-8111-111111111111";
const FRESH_ID: &str = "22222222-2222-4222-8222-222222222222";

fn old_moment() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(OLD)
}

fn call(host: &Host, dir: &Path, payload: Value) -> DaemonResponse {
    let mut request = DaemonRequest::new("r1", "sessions.prune");
    if let Value::Object(fields) = payload {
        request.payload = fields;
    }
    handle(host, dir, &mut DispatchState::new(), &request)
}

fn result_of(response: DaemonResponse) -> Value {
    let DaemonResponse::Success { result, .. } = response else {
        panic!("expected success, got {response:?}");
    };
    result
}

fn error_of(response: DaemonResponse) -> String {
    let DaemonResponse::Failure { error, .. } = response else {
        panic!("expected a refusal, got {response:?}");
    };
    error
}

/// One saved record at `sessions/<id>/<id>.json` with a chosen modification
/// time; the body length is the row's byte count.
fn seed(dir: &Path, id: &str, body_len: usize, modified: SystemTime) -> std::path::PathBuf {
    let folder = dir.join("sessions").join(id);
    fs::create_dir_all(&folder).unwrap();
    let file = folder.join(format!("{id}.json"));
    fs::write(&file, "x".repeat(body_len)).unwrap();
    let handle = fs::OpenOptions::new().write(true).open(&file).unwrap();
    handle.set_modified(modified).unwrap();
    file
}

fn host_over(dir: &Path) -> Host {
    let host = Host::new();
    host.reload_settings(dir).expect("settings load");
    host
}

fn tones(result: &Value) -> Vec<(&str, &str)> {
    result["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| {
            (
                line["tone"].as_str().unwrap(),
                line["text"].as_str().unwrap(),
            )
        })
        .collect()
}

#[test]
fn a_preview_lists_only_files_older_than_the_cutoff() {
    let dir = temp_data_dir("prune-preview");
    let host = host_over(&dir);
    seed(&dir, OLD_ID, 1024, old_moment());
    seed(&dir, FRESH_ID, 10, SystemTime::now());

    let result = result_of(call(&host, &dir, json!({ "olderThan": "30d" })));
    assert_eq!(result["count"], json!(1));
    assert_eq!(result["totalBytes"], json!(1024));
    let expected_row = format!("  2020-01-02  {OLD_ID}  1.0 KB");
    assert_eq!(
        tones(&result),
        vec![
            ("plain", expected_row.as_str()),
            (
                "info",
                "1 session file(s), 1.0 KB total. Nothing was deleted; re-run with --yes to delete them."
            ),
        ]
    );
    // A preview deletes nothing.
    assert!(dir.join("sessions").join(OLD_ID).is_dir());
}

#[test]
fn an_empty_selection_says_so_with_the_asked_window() {
    let dir = temp_data_dir("prune-empty");
    let host = host_over(&dir);
    seed(&dir, FRESH_ID, 10, SystemTime::now());

    let result = result_of(call(&host, &dir, json!({ "olderThan": "12h" })));
    assert_eq!(result["count"], json!(0));
    assert_eq!(
        result["lines"][0]["text"],
        json!("No session files are older than 12h; nothing to prune.")
    );
}

#[test]
fn a_bad_duration_is_refused_with_the_source_sentence() {
    let dir = temp_data_dir("prune-duration");
    let host = host_over(&dir);
    assert_eq!(
        error_of(call(&host, &dir, json!({ "olderThan": "3x" }))),
        "Invalid duration \"3x\". Use a value like 30d, 12h, 90m, or 2w."
    );
    assert_eq!(
        error_of(call(&host, &dir, json!({}))),
        "Invalid duration \"\". Use a value like 30d, 12h, 90m, or 2w."
    );
}

#[test]
fn apply_must_be_a_boolean() {
    let dir = temp_data_dir("prune-apply-type");
    let host = host_over(&dir);
    assert_eq!(
        error_of(call(
            &host,
            &dir,
            json!({ "olderThan": "30d", "apply": "yes" })
        )),
        "apply must be a boolean."
    );
}

#[test]
fn apply_deletes_exactly_the_selection() {
    let dir = temp_data_dir("prune-apply");
    let host = host_over(&dir);
    seed(&dir, OLD_ID, 1024, old_moment());
    seed(&dir, FRESH_ID, 10, SystemTime::now());

    let result = result_of(call(
        &host,
        &dir,
        json!({ "olderThan": "30d", "apply": true }),
    ));
    assert_eq!(result["deleted"], json!(1));
    assert_eq!(result["failed"], json!(0));
    assert_eq!(
        tones(&result).last().unwrap(),
        &("success", "Deleted 1 session file(s) (1.0 KB).")
    );
    assert!(!dir.join("sessions").join(OLD_ID).exists());
    assert!(dir.join("sessions").join(FRESH_ID).is_dir());
}

#[test]
fn the_machine_custodian_is_never_selected_or_deleted() {
    let dir = temp_data_dir("prune-custodian");
    let host = host_over(&dir);
    seed(&dir, CUSTODIAN, 32, old_moment());

    let result = result_of(call(
        &host,
        &dir,
        json!({ "olderThan": "1m", "apply": true }),
    ));
    assert_eq!(result["count"], json!(0));
    assert!(dir.join("sessions").join(CUSTODIAN).is_dir());
}

#[test]
fn a_remote_connection_cannot_prune_session_files() {
    let dir = temp_data_dir("prune-remote");
    let host = host_over(&dir);
    seed(&dir, OLD_ID, 32, old_moment());

    let mut request = DaemonRequest::new("r1", "sessions.prune");
    if let Value::Object(fields) = json!({ "olderThan": "30d", "apply": true }) {
        request.payload = fields;
    }
    let mut state = DispatchState::new();
    state.set_remote(true);
    assert_eq!(
        error_of(handle(&host, &dir, &mut state, &request)),
        "Session files are pruned on the host machine only."
    );
    assert!(dir.join("sessions").join(OLD_ID).is_dir());
}
