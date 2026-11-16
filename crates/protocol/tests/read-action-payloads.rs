//! Payload rules for the three disk-authoritative session reads
//! (`session.load`, `session.entries-range`, `session.find-attachment`).
//!
//! Fail-before pins: the legacy validation wording (`requireString` /
//! `requireInteger` — byte limits, JS `Number.isInteger` range semantics), the
//! absolute-index page math with its defaults and clamps, and the
//! inline-attachment filter. Source anchors: `daemon-host.ts` `requireString`
//! L4210, `requireInteger` L4216, and the three dispatch cases L3594-L3640
//! (read 2026-11-16).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use openbutler_protocol::ValidationError;
use openbutler_protocol::payload::{require_integer, require_string};
use openbutler_protocol::session::read::{
    DEFAULT_ENTRIES_LIMIT, EntriesRangeRequest, InlineAttachment, entries_range,
    find_attachment_id, find_inline_attachment, load_session_id,
};
use serde_json::{Map, Value, json};

fn payload(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

fn error_text(error: ValidationError) -> String {
    error.to_string()
}

// ---------------------------------------------------------------- require_string

#[test]
fn require_string_missing_key_reports_the_field() {
    let error = require_string(None, "sessionId", 64).expect_err("missing key must fail");
    assert_eq!(
        error_text(error),
        "sessionId must be a non-empty string under 64 bytes."
    );
}

#[test]
fn require_string_rejects_empty_and_non_strings_with_one_wording() {
    for value in [json!(""), json!(42), json!(null), json!(["x"])] {
        let error = require_string(Some(&value), "sessionId", 64)
            .map_err(|e| e.to_string())
            .expect_err("invalid value must fail");
        assert_eq!(
            error,
            "sessionId must be a non-empty string under 64 bytes."
        );
    }
}

#[test]
fn require_string_limits_utf8_bytes_not_utf16_units() {
    // 33 × é = 66 bytes but only 33 UTF-16 units: the legacy check is
    // Buffer.byteLength, so this fails on bytes while a UTF-16 count passes.
    let over_bytes = "é".repeat(33);
    let error = require_string(Some(&Value::String(over_bytes)), "sessionId", 64)
        .map_err(|e| e.to_string())
        .expect_err("66 bytes must fail a 64-byte cap");
    assert_eq!(
        error,
        "sessionId must be a non-empty string under 64 bytes."
    );

    let at_cap = "é".repeat(32); // exactly 64 bytes
    let accepted =
        require_string(Some(&Value::String(at_cap)), "sessionId", 64).expect("64 bytes fit");
    assert_eq!(accepted, "é".repeat(32));
}

// ---------------------------------------------------------------- require_integer

#[test]
fn require_integer_wording_pins_both_bounds() {
    let error = require_integer(Some(&json!(-1)), "before", 0, 9_007_199_254_740_991)
        .map_err(|e| e.to_string())
        .expect_err("below the floor must fail");
    assert_eq!(
        error,
        "before must be an integer between 0 and 9007199254740991."
    );

    let error = require_integer(Some(&json!(201)), "limit", 1, 200)
        .map_err(|e| e.to_string())
        .expect_err("above the cap must fail");
    assert_eq!(error, "limit must be an integer between 1 and 200.");
}

#[test]
fn require_integer_rejects_types_and_fractions_with_the_same_message() {
    for value in [json!("5"), json!(1.5), json!(true), json!(null)] {
        let error = require_integer(Some(&value), "limit", 1, 200)
            .map_err(|e| e.to_string())
            .expect_err("non-integer must fail");
        assert_eq!(error, "limit must be an integer between 1 and 200.");
    }
}

#[test]
fn require_integer_bounds_are_inclusive() {
    assert_eq!(
        require_integer(Some(&json!(0)), "before", 0, 9_007_199_254_740_991).unwrap(),
        0
    );
    assert_eq!(
        require_integer(
            Some(&json!(9_007_199_254_740_991u64)),
            "before",
            0,
            9_007_199_254_740_991
        )
        .unwrap(),
        9_007_199_254_740_991
    );
    assert_eq!(
        require_integer(Some(&json!(1)), "limit", 1, 200).unwrap(),
        1
    );
    assert_eq!(
        require_integer(Some(&json!(200)), "limit", 1, 200).unwrap(),
        200
    );
}

// ---------------------------------------------------------------- typed payloads

#[test]
fn load_payload_reads_session_id_with_legacy_rules() {
    let map = payload(&[("sessionId", json!("abc-123"))]);
    assert_eq!(load_session_id(&map).unwrap(), "abc-123");

    let error = load_session_id(&Map::new())
        .map_err(|e| e.to_string())
        .expect_err("missing sessionId must fail");
    assert_eq!(
        error,
        "sessionId must be a non-empty string under 64 bytes."
    );
}

#[test]
fn attachment_payload_reads_id_with_legacy_rules() {
    let map = payload(&[("attachmentId", json!("att-1"))]);
    assert_eq!(find_attachment_id(&map).unwrap(), "att-1");

    let at_cap = payload(&[("attachmentId", json!("a".repeat(128)))]);
    assert_eq!(find_attachment_id(&at_cap).unwrap(), "a".repeat(128));

    let error = find_attachment_id(&payload(&[("attachmentId", json!("a".repeat(129)))]))
        .map_err(|e| e.to_string())
        .expect_err("129 bytes must fail the 128-byte cap");
    assert_eq!(
        error,
        "attachmentId must be a non-empty string under 128 bytes."
    );
}

#[test]
fn range_payload_defaults_before_to_the_end_and_limit_to_fifty() {
    let request = entries_range(&Map::new()).unwrap();
    assert_eq!(request.before, None);
    assert_eq!(request.limit, DEFAULT_ENTRIES_LIMIT);

    // Absent `before` means "up to total": 120 entries, page of 50 → 70..120.
    assert_eq!(request.resolve(120), (70, 120));
    // A short tail fits whole: 3 entries, page of 50 → 0..3.
    assert_eq!(request.resolve(3), (0, 3));
}

#[test]
fn range_payload_clamps_before_to_total_and_floors_the_start() {
    let over = entries_range(&payload(&[("before", json!(999))])).unwrap();
    assert_eq!(over.resolve(120), (70, 120));

    let window = entries_range(&payload(&[("before", json!(30)), ("limit", json!(10))])).unwrap();
    assert_eq!(window.resolve(120), (20, 30));

    let empty = entries_range(&payload(&[("before", json!(0))])).unwrap();
    assert_eq!(empty.resolve(120), (0, 0));
}

#[test]
fn range_payload_rejects_out_of_range_before_and_limit() {
    let error = entries_range(&payload(&[("before", json!(-1))]))
        .map_err(|e| e.to_string())
        .expect_err("negative before must fail");
    assert_eq!(
        error,
        "before must be an integer between 0 and 9007199254740991."
    );

    let error = entries_range(&payload(&[("limit", json!(201))]))
        .map_err(|e| e.to_string())
        .expect_err("limit above 200 must fail");
    assert_eq!(error, "limit must be an integer between 1 and 200.");
}

// ---------------------------------------------------------------- inline filter

fn user_entry(attachments: Option<Vec<Value>>) -> openbutler_protocol::SessionEntry {
    let mut entry = json!({ "kind": "user", "text": "hello" });
    if let Some(attachments) = attachments {
        entry["attachments"] = json!(attachments);
    }
    serde_json::from_value(entry).expect("user entry parses")
}

fn attachment(id: &str, path: &str, media_type: &str) -> Value {
    json!({
        "id": id,
        "path": path,
        "name": "file.bin",
        "mediaType": media_type,
        "sizeBytes": 10,
        "sha256": "abc",
    })
}

#[test]
fn inline_filter_matches_id_and_inline_media_type_only() {
    let entries = vec![
        user_entry(Some(vec![attachment("a1", "/w/shot.png", "image/png")])),
        user_entry(Some(vec![attachment(
            "p1",
            "/w/doc.pdf",
            "application/pdf",
        )])),
        user_entry(None),
    ];

    let found = find_inline_attachment(&entries, "a1").expect("png must resolve");
    assert_eq!(found.path, "/w/shot.png");
    assert_eq!(found.media_type, "image/png");

    assert!(
        find_inline_attachment(&entries, "p1").is_none(),
        "a pdf must never resolve, whatever the id"
    );
    assert!(find_inline_attachment(&entries, "missing").is_none());
}

#[test]
fn inline_attachment_result_serialises_with_legacy_keys() {
    let result = InlineAttachment {
        path: "/w/shot.png".to_owned(),
        media_type: "image/png".to_owned(),
    };
    assert_eq!(
        serde_json::to_value(result).unwrap(),
        json!({ "path": "/w/shot.png", "mediaType": "image/png" })
    );
}

#[test]
fn entries_range_request_is_plain_data() {
    // The struct itself is what dispatch serialises fields from; pin its shape
    // so a stray rename shows up here rather than on the wire.
    let request = EntriesRangeRequest {
        before: Some(5),
        limit: 10,
    };
    assert_eq!(request.before, Some(5));
    assert_eq!(request.limit, 10);
}
