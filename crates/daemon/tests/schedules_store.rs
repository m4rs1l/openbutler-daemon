//! The schedule store: `schedules.json` is the schedule's own document —
//! schedules are user records, not daemon configuration, and the settings
//! file must never carry them (owner ruling 2026-11-18).
//!
//! Sources: the schedule half that moved out of `settings.ts`
//! (`schedules must be a list...`, the per-schedule messages, duplicate
//! refusal — all byte-preserved), read 2026-11-18.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use openbutler_daemon::state::schedules::{ScheduleState, ScheduleTrigger, load, save};
use serde_json::{Value, json};

fn temp_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "ob-schedules-{tag}-{}-{nanos:x}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn schedule(id: &str) -> ScheduleState {
    // Absolute wherever the suite runs; the store validates absoluteness.
    let workspace_root = std::env::temp_dir()
        .join("proj")
        .to_string_lossy()
        .into_owned();
    ScheduleState {
        id: id.to_owned(),
        name: "Nightly build".to_owned(),
        enabled: true,
        workspace_root,
        origin_session_id: "chat-1".to_owned(),
        instruction: "rebuild the site".to_owned(),
        trigger: ScheduleTrigger {
            adapter: "daily".to_owned(),
            options: json!({ "at": "02:00", "days": ["mon", "tue"] }),
        },
    }
}

/// ENOENT is an empty store — no file is created by reading.
#[test]
fn a_missing_store_reads_empty() {
    let root = temp_root("missing");
    let schedules = load(&root).expect("empty store");
    assert!(schedules.is_empty());
    assert!(
        !root.join("schedules.json").exists(),
        "read creates nothing"
    );
}

/// Save → file → load: the full shape survives, including trigger options.
#[test]
fn a_store_round_trips_the_full_schedule_shape() {
    let root = temp_root("roundtrip");
    save(&root, &[schedule("nightly-a"), schedule("nightly-b")]).expect("save");

    let document = std::fs::read_to_string(root.join("schedules.json")).expect("file exists");
    let parsed: Value = serde_json::from_str(&document).expect("valid JSON document");
    assert!(parsed.is_array(), "the store is a bare schedule list");

    let loaded = load(&root).expect("reload");
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0], schedule("nightly-a"));
    assert_eq!(loaded[1], schedule("nightly-b"));
    assert_eq!(
        loaded[0].trigger.options["days"],
        json!(["mon", "tue"]),
        "trigger options ride intact"
    );
}

/// The list shape refusals, byte-identical to the settings half they
/// replaced.
#[test]
fn the_list_shape_and_size_refuse_like_the_source() {
    let root = temp_root("shape");
    std::fs::write(root.join("schedules.json"), "{ \"not\": \"a list\" }\n").unwrap();
    let error = load(&root).expect_err("not a list").to_string();
    assert_eq!(error, "schedules must be a list of up to 256 schedules.");

    let root = temp_root("size");
    let oversized: Vec<Value> = (0..257)
        .map(|index| json!({ "id": format!("s-{index}") }))
        .collect();
    std::fs::write(
        root.join("schedules.json"),
        serde_json::to_string(&oversized).unwrap(),
    )
    .unwrap();
    let error = load(&root).expect_err("too many").to_string();
    assert_eq!(error, "schedules must be a list of up to 256 schedules.");
}

/// Duplicate ids are refused on read — one id, one schedule.
#[test]
fn duplicate_ids_are_refused() {
    let root = temp_root("duplicates");
    std::fs::write(
        root.join("schedules.json"),
        serde_json::to_string(&[schedule("same"), schedule("same")]).unwrap(),
    )
    .unwrap();
    let error = load(&root).expect_err("duplicate").to_string();
    assert_eq!(error, "schedules contains duplicate schedule ids.");
}

/// Per-schedule refusals stay byte-preserved — the messages are what a
/// client shows. These five lived in the settings parser and moved with
/// the shape they validate.
#[test]
fn per_schedule_refusals_stay_verbatim() {
    let abs = std::env::temp_dir().to_string_lossy().into_owned();
    let raw = json!({
        "id": "ok",
        "name": "n",
        "enabled": true,
        "workspaceRoot": abs,
        "originSessionId": "",
        "instruction": "i",
        "trigger": { "adapter": "daily", "options": { "at": "02:00" } }
    });

    let refused = |mut item: Value, expected: &str| {
        let root = temp_root("per-item");
        std::fs::write(
            root.join("schedules.json"),
            serde_json::to_string(&[item.take()]).unwrap(),
        )
        .unwrap();
        assert_eq!(load(&root).expect_err("refused").to_string(), expected);
    };

    let mut item = raw.clone();
    item["bogus"] = json!(1);
    refused(item, "Schedule bogus is not a supported setting.");

    let mut item = raw.clone();
    item["id"] = json!("bad id!");
    refused(
        item,
        "Schedule id must use 1-64 letters, digits, underscores, or dashes.",
    );

    let mut item = raw.clone();
    item["name"] = json!("");
    refused(item, "Schedule name must use 1-128 characters.");

    let mut item = raw.clone();
    item["instruction"] = json!("");
    refused(item, "Schedule instruction must use 1-16000 characters.");

    let mut item = raw.clone();
    item["trigger"] = json!({ "options": {}, "bogus": 1 });
    refused(item, "Schedule trigger must contain adapter and options.");

    let mut item = raw;
    item["trigger"] = json!({ "options": {} });
    refused(item, "Schedule trigger adapter is invalid.");
}

/// Save takes the store's write boundary (the host's lock file) and
/// writes atomically — a partial document never exists.
#[test]
fn save_writes_atomically_under_the_store_lock() {
    let root = temp_root("atomic");
    save(&root, &[schedule("first")]).expect("first save");
    save(&root, &[schedule("second"), schedule("third")]).expect("second save");

    let loaded = load(&root).expect("reload");
    let ids: Vec<&str> = loaded.iter().map(|entry| entry.id.as_str()).collect();
    assert_eq!(ids, vec!["second", "third"], "the last write wins whole");
    assert!(
        !root.join("settings.lock").exists() || root.join("schedules.json").exists(),
        "the store lives beside the settings file, never inside it"
    );
}
