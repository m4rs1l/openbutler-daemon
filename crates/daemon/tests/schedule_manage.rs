//! `schedule.manage` pins: the capability list, the CRUD persisted in
//! the schedule's own store (`schedules.json` — never `settings.yaml`),
//! and the source's exact refusals.
//!
//! Source: `daemon-host.ts` `schedule.manage` → `schedule-service.ts`
//! `executeBackground` (read 2026-11-16). The runtime half — `run`,
//! `list`, `log` — belongs to the ledger/scheduler slice and is not
//! implemented in this build.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

use openbutler_daemon::dispatch::{DispatchState, Host, handle};
use openbutler_protocol::DaemonRequest;
use serde_json::{Value, json};

fn temp_data_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ob-sched-{}-{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn manage(input: Value) -> DaemonRequest {
    DaemonRequest::new("t-sched", "schedule.manage").with("input", input)
}

fn apply(host: &Host, dir: &Path, input: Value) -> openbutler_protocol::DaemonResponse {
    let mut state = DispatchState::new();
    handle(host, dir, &mut state, &manage(input))
}

fn success_json(response: openbutler_protocol::DaemonResponse) -> Value {
    let openbutler_protocol::DaemonResponse::Success { result, .. } = response else {
        panic!("expected success, got {response:?}");
    };
    result
}

fn failure_text(response: openbutler_protocol::DaemonResponse) -> String {
    let openbutler_protocol::DaemonResponse::Failure { error, .. } = response else {
        panic!("expected failure, got {response:?}");
    };
    error
}

fn daily_schedule(id: &str, root: &Path) -> Value {
    json!({
        "id": id,
        "name": id,
        "enabled": true,
        "workspaceRoot": root.display().to_string(),
        "instruction": "i",
        "trigger": { "adapter": "daily", "options": { "at": "02:00" } }
    })
}

#[test]
fn capabilities_lists_every_adapter() {
    let dir = temp_data_dir("caps");
    let host = Host::new();
    let result = success_json(apply(&host, &dir, json!({ "action": "capabilities" })));

    let adapters: Vec<&str> = result["adapters"]
        .as_array()
        .expect("adapters array")
        .iter()
        .map(|entry| entry["adapter"].as_str().expect("adapter id"))
        .collect();
    assert_eq!(
        adapters,
        vec![
            "manual",
            "interval",
            "at",
            "window",
            "daily",
            "filesystem",
            "memory_pressure",
        ],
        "the capability order is part of the contract"
    );
    assert_eq!(
        result["adapters"][4]["description"],
        "Run at a wall-clock time, optionally on selected weekdays. The never-drifting way to say \"every night at 02:00\".",
        "descriptions are verbatim"
    );
}

#[test]
fn add_persists_a_generated_id_and_survives_reload() {
    let dir = temp_data_dir("add");
    let host = Host::new();
    host.reload_settings(&dir).expect("boot load");

    let input = json!({
        "action": "add",
        "schedule": {
            "name": "Nightly backup",
            "workspaceRoot": dir.display().to_string(),
            "instruction": "i",
            "trigger": { "adapter": "daily", "options": { "at": "02:00" } }
        }
    });
    let result = success_json(apply(&host, &dir, input));

    assert_eq!(result["status"], "completed");
    assert_eq!(result["saved"], true);
    let id = result["schedule"]["id"].as_str().expect("generated id");
    assert!(
        id.starts_with("nightly-backup-") && id.len() == "nightly-backup-".len() + 4,
        "readable slug plus four hex characters: {id}"
    );
    assert_eq!(result["schedule"]["enabled"], true, "enabled defaults on");

    let saved = fs::read_to_string(dir.join("schedules.json")).expect("store written");
    assert!(saved.contains(id), "the schedule lands in schedules.json");
    assert!(
        !dir.join("settings.yaml").exists(),
        "a schedule is not configuration: the settings file never appears"
    );
}

#[test]
fn add_without_a_workspace_root_is_refused() {
    let dir = temp_data_dir("noroot");
    let host = Host::new();
    let input = json!({
        "action": "add",
        "schedule": {
            "name": "n",
            "instruction": "i",
            "trigger": { "adapter": "manual", "options": {} }
        }
    });
    let error = failure_text(apply(&host, &dir, input));
    assert_eq!(
        error,
        "Choose a workspace with --workspace or set workspaceRoot in the schedule."
    );
}

#[test]
fn a_bad_trigger_option_is_refused_with_the_range_message() {
    let dir = temp_data_dir("trigger");
    let host = Host::new();
    let input = json!({
        "action": "add",
        "schedule": {
            "name": "n",
            "workspaceRoot": dir.display().to_string(),
            "instruction": "i",
            "trigger": { "adapter": "interval", "options": { "everySeconds": 5 } }
        }
    });
    let error = failure_text(apply(&host, &dir, input));
    assert_eq!(
        error,
        "everySeconds must be an integer between 10 and 31536000."
    );
}

#[test]
fn duplicate_ids_and_missing_schedules_are_refused() {
    let dir = temp_data_dir("dup");
    let host = Host::new();
    host.reload_settings(&dir).expect("boot load");
    let schedule = daily_schedule("a", &dir);
    apply(
        &host,
        &dir,
        json!({ "action": "add", "schedule": schedule.clone() }),
    );

    let duplicate = failure_text(apply(
        &host,
        &dir,
        json!({ "action": "add", "schedule": schedule }),
    ));
    assert_eq!(duplicate, "A schedule with that id already exists.");

    let missing = failure_text(apply(
        &host,
        &dir,
        json!({ "action": "update", "schedule": daily_schedule("ghost", &dir) }),
    ));
    assert_eq!(missing, "Schedule does not exist.");

    let missing_id = failure_text(apply(
        &host,
        &dir,
        json!({ "action": "enable", "scheduleId": "ghost" }),
    ));
    assert_eq!(missing_id, "Schedule does not exist.");
}

#[test]
fn enable_disable_and_remove_rewrite_the_store() {
    let dir = temp_data_dir("toggle");
    let host = Host::new();
    host.reload_settings(&dir).expect("boot load");
    apply(
        &host,
        &dir,
        json!({ "action": "add", "schedule": daily_schedule("a", &dir) }),
    );

    let disabled = success_json(apply(
        &host,
        &dir,
        json!({ "action": "disable", "scheduleId": "a" }),
    ));
    assert_eq!(disabled["status"], "completed");
    assert_eq!(disabled["scheduleId"], "a");
    let saved = fs::read_to_string(dir.join("schedules.json")).expect("store written");
    assert!(
        saved.contains("\"enabled\": false"),
        "disable persists: {saved}"
    );

    let enabled = success_json(apply(
        &host,
        &dir,
        json!({ "action": "enable", "scheduleId": "a" }),
    ));
    assert_eq!(enabled["saved"], true);
    let saved = fs::read_to_string(dir.join("schedules.json")).expect("store written");
    assert!(
        saved.contains("\"enabled\": true"),
        "enable persists: {saved}"
    );

    let removed = success_json(apply(
        &host,
        &dir,
        json!({ "action": "remove", "scheduleId": "a" }),
    ));
    assert_eq!(removed["saved"], true);
    let saved = fs::read_to_string(dir.join("schedules.json")).expect("store written");
    assert!(!saved.contains("\"a\""), "the schedule is gone: {saved}");

    let after = failure_text(apply(
        &host,
        &dir,
        json!({ "action": "enable", "scheduleId": "a" }),
    ));
    assert_eq!(after, "Schedule does not exist.");
}

#[test]
fn an_unknown_action_is_named() {
    let dir = temp_data_dir("unknown");
    let host = Host::new();
    let error = failure_text(apply(&host, &dir, json!({ "action": "teleport" })));
    assert_eq!(error, "Unknown schedule action: teleport");
}
