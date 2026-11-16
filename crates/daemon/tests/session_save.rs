//! c1a — the session write path: `prepareSessionSave` recompute rules, the
//! header field-order contract, and the atomic `saveSession` write.
//!
//! Source: legacy `src/session-store.ts` L1048-L1141 (`prepareSessionSave`,
//! `saveSession`). The listing reads a header cut from the front of the file,
//! so the denormalised display fields must serialise **before** `entries` and
//! the file is pretty-printed with two-space indent and a trailing newline.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use openbutler_daemon::state::{SaveOutcome, parse_record, save_session};

fn temp_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "ob-session-save-{tag}-{}-{nanos:x}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A loaded record with **stale** display fields and both conversation
/// arrays — exactly what a load-then-save round trip hands the writer.
fn record_json(entries: &str, conversation: &str) -> String {
    format!(
        r#"{{
  "version": 1,
  "id": "11111111-1111-4111-8111-111111111111",
  "title": "A session",
  "createdAt": "2026-01-01T00:00:00.000Z",
  "updatedAt": "2026-01-02T00:00:00.000Z",
  "providerId": "prov",
  "modelId": "model",
  "hasUserMessage": false,
  "preview": "stale preview",
  "firstUserMessage": "stale first",
  "lastAssistantMessage": "stale last",
  "entries": {entries},
  "conversation": {conversation}
}}"#
    )
}

const WITH_USER: &str = r#"[
  {"kind":"user","text":"hello world","at":1700000000000,"source":"terminal"},
  {"kind":"assistant","text":"assistant reply","at":1700000001000}
]"#;
const ASSISTANT_ONLY: &str = r#"[{"kind":"assistant","text":"only reply","at":1700000001000}]"#;
const CONVERSATION: &str = r#"[
  {"role":"user","content":"hello world"},
  {"role":"assistant","content":"assistant reply"}
]"#;

#[test]
fn saves_recompute_display_fields_and_keep_them_before_the_entries_boundary() {
    let root = temp_root("order");
    let record = parse_record(&record_json(WITH_USER, CONVERSATION)).expect("record parses");

    let SaveOutcome::Written(path) = save_session(&root, &record, false).expect("save succeeds")
    else {
        panic!("a record with a user entry is written");
    };
    let text = fs::read_to_string(&path).unwrap();

    // Pretty two-space JSON with the legacy trailing newline.
    assert!(
        text.contains("\n  \"entries\": ["),
        "the header reader's structural boundary must exist: {text}"
    );
    assert!(
        text.ends_with("}\n"),
        "trailing newline like JSON.stringify + \\n"
    );

    // Recompute: stale values must never survive a save.
    assert!(!text.contains("stale preview"));
    assert!(!text.contains("stale first"));
    assert!(!text.contains("stale last"));
    assert!(text.contains("\"preview\": \"hello world\""));
    assert!(text.contains("\"firstUserMessage\": \"hello world\""));
    assert!(text.contains("\"lastAssistantMessage\": \"assistant reply\""));
    assert!(text.contains("\"hasUserMessage\": true"));
    assert!(
        !text.contains("\"updatedAt\": \"2026-01-02T00:00:00.000Z\""),
        "updatedAt is refreshed on every save"
    );

    // Field order: every display field before `entries`; conversation after.
    let entries_at = text.find("\n  \"entries\":").expect("entries marker");
    for key in [
        "\"hasUserMessage\"",
        "\"preview\"",
        "\"firstUserMessage\"",
        "\"lastAssistantMessage\"",
    ] {
        let at = text.find(key).unwrap_or_else(|| panic!("{key} missing"));
        assert!(
            at < entries_at,
            "{key} must serialise before entries ({at} !< {entries_at})"
        );
    }
    let conversation_at = text.find("\"conversation\"").expect("conversation field");
    assert!(conversation_at > entries_at, "conversation follows entries");

    // The saved file parses back as a record.
    let reloaded = parse_record(&text).expect("saved output is a valid record");
    assert_eq!(reloaded.meta.id, record.meta.id);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_session_without_user_entries_is_skipped_unless_allowed() {
    let root = temp_root("empty");
    let record = parse_record(&record_json(ASSISTANT_ONLY, CONVERSATION)).expect("record parses");

    match save_session(&root, &record, false).expect("save runs") {
        SaveOutcome::Skipped => {}
        SaveOutcome::Written(_) => panic!("legacy returns null and writes nothing"),
    }
    let sessions = root.join("sessions");
    let any_file = fs::read_dir(&sessions)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(any_file, 0, "no session file exists after a skip");

    // allowEmpty writes it (the title/metadata-only save path).
    assert!(matches!(
        save_session(&root, &record, true).expect("allowed save"),
        SaveOutcome::Written(_)
    ));

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn an_invalid_session_is_refused_with_the_source_message() {
    let root = temp_root("invalid");
    let json = record_json(WITH_USER, CONVERSATION)
        .replace("11111111-1111-4111-8111-111111111111", "not-a-uuid");
    let record = parse_record(&json).expect("record parses");

    let error = save_session(&root, &record, false).expect_err("id is invalid");
    assert_eq!(error.to_string(), "Cannot save an invalid session.");
    assert!(
        !root.join("sessions").exists(),
        "a refused save creates nothing"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn writes_are_atomic_reusable_and_private() {
    let root = temp_root("atomic");
    let record = parse_record(&record_json(WITH_USER, CONVERSATION)).expect("record parses");

    let SaveOutcome::Written(path) = save_session(&root, &record, false).unwrap() else {
        panic!("written");
    };
    let first = fs::read(&path).unwrap();

    // No temporary file survives a successful write.
    let leftovers: Vec<String> = fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");

    // Overwriting the same record works (the debounced save path).
    let SaveOutcome::Written(second) = save_session(&root, &record, false).unwrap() else {
        panic!("written again");
    };
    assert_eq!(path, second);
    let second_bytes = fs::read(&path).unwrap();
    assert_eq!(first.len(), second_bytes.len(), "stable record size");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "session files are private (0600)");
    }

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_save_preserves_the_conversation_arrays() {
    let root = temp_root("conversation");
    let archive = r#"[{"role":"user","content":"archived"}]"#;
    let json = record_json(WITH_USER, CONVERSATION).replace(
        "\"conversation\": [",
        &format!("\"conversationArchive\": {archive},\n  \"conversation\": ["),
    );
    let record = parse_record(&json).expect("record parses");

    let SaveOutcome::Written(path) = save_session(&root, &record, false).unwrap() else {
        panic!("written");
    };
    let saved: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let original: serde_json::Value = serde_json::from_str(&json).unwrap();

    assert_eq!(
        saved["conversation"], original["conversation"],
        "the model-visible history survives the round trip"
    );
    assert_eq!(
        saved["conversationArchive"], original["conversationArchive"],
        "the compaction archive survives the round trip"
    );

    let _ = fs::remove_dir_all(&root);
}
