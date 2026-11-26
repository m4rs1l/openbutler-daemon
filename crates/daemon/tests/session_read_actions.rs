//! The three disk-authoritative session reads at the dispatch surface.
//!
//! Fail-before pins for `session.load`, `session.entries-range`, and
//! `session.find-attachment`: the fixed wording (custodian refusal, not
//! found, validation messages), the connection-scoped chat the paging and
//! attachment actions address, and the wire shapes. Source anchors:
//! `daemon-host.ts` cases L3594-L3640 (read 2026-11-16).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{bound_state, pretty_session, temp_data_dir, uuid, with_binding, write_session};
use openbutler_protocol::payload::require_string;
use openbutler_protocol::session::read::{InlineAttachment, load_session_id};
use openbutler_protocol::{DaemonRequest, DaemonResponse};
use serde_json::{Map, Value, json};

use openbutler_daemon::dispatch::{self, DispatchState, Host};

fn request(id: &str, action: &str, pairs: &[(&str, Value)]) -> DaemonRequest {
    let mut request = DaemonRequest::new(id, action);
    for (key, value) in pairs {
        request = request.with(*key, value.clone());
    }
    request
}

fn call(
    host: &Host,
    data_dir: &std::path::Path,
    state: &mut DispatchState,
    request: DaemonRequest,
) -> DaemonResponse {
    let request = with_binding(state, request);
    dispatch::handle(host, data_dir, state, &request)
}

fn success_result(response: DaemonResponse) -> Value {
    let DaemonResponse::Success { result, .. } = response else {
        panic!("expected a success envelope, got {response:?}");
    };
    result
}

fn failure_text(response: DaemonResponse) -> String {
    let DaemonResponse::Failure { error, .. } = response else {
        panic!("expected a failure envelope, got {response:?}");
    };
    error
}

/// A record whose transcript is exactly `texts`, one user entry each — the
/// paging fixture (a fixed 120 entries keeps the default page arithmetic
/// observable without heuristics).
fn session_with_entries(id: &str, texts: &[String]) -> String {
    let entries: Vec<Value> = texts
        .iter()
        .map(|text| json!({ "kind": "user", "text": text, "source": "terminal" }))
        .collect();
    format!(
        "{{\n  \"version\": 1,\n  \"id\": \"{id}\",\n  \"title\": \"paging\",\
         \n  \"createdAt\": \"2026-11-15T20:00:00.000Z\",\
         \n  \"updatedAt\": \"2026-11-15T20:00:00.000Z\",\
         \n  \"lastActivityAt\": \"2026-11-15T20:05:00.000Z\",\
         \n  \"providerId\": \"vercel\",\n  \"modelId\": \"xiaomi/mimo-v2.6-flash\",\n  \
         \"hasUserMessage\": true,\n  \"preview\": \"start\",\n  \"firstUserMessage\": \"start\",\
         \n  \"lastAssistantMessage\": \"done\",\n  \"entries\": [\n    {}\n  ]\n}}\n",
        entries
            .iter()
            .map(|entry| entry.to_string())
            .collect::<Vec<_>>()
            .join(",\n    ")
    )
}

/// A record with one attachment-bearing user entry (png) plus a pdf one, for
/// `session.find-attachment`.
fn session_with_attachments(id: &str) -> String {
    format!(
        "{{\n  \"version\": 1,\n  \"id\": \"{id}\",\n  \"title\": \"shots\",\
         \n  \"createdAt\": \"2026-11-15T20:00:00.000Z\",\
         \n  \"updatedAt\": \"2026-11-15T20:00:00.000Z\",\
         \n  \"providerId\": \"vercel\",\n  \"modelId\": \"xiaomi/mimo-v2.6-flash\",\n  \
         \"hasUserMessage\": true,\n  \"preview\": \"start\",\n  \"firstUserMessage\": \"start\",\
         \n  \"lastAssistantMessage\": \"done\",\n  \"entries\": [\n    {{\n      \"kind\": \"user\",\n      \
         \"text\": \"see these\",\n      \"attachments\": [\n        {{\n          \"id\": \"att-png\",\n          \
         \"path\": \"/w/shot.png\",\n          \"name\": \"shot.png\",\n          \"mediaType\": \"image/png\",\n          \
         \"sizeBytes\": 11,\n          \"sha256\": \"aa\"\n        }},\n        {{\n          \"id\": \"att-pdf\",\n          \
         \"path\": \"/w/doc.pdf\",\n          \"name\": \"doc.pdf\",\n          \"mediaType\": \"application/pdf\",\n          \
         \"sizeBytes\": 22,\n          \"sha256\": \"bb\"\n        }}\n      ]\n    }}\n  ]\n}}\n"
    )
}

// ---------------------------------------------------------------- session.load

#[test]
fn load_returns_the_saved_record_with_meta_and_entries() {
    let dir = temp_data_dir("load-view");
    let id = uuid(1);
    write_session(&dir, &id, &pretty_session(&id, None, "A chat", true));

    let host = Host::new();
    let mut state = bound_state();
    let result = success_result(call(
        &host,
        &dir,
        &mut state,
        request("r1", "session.load", &[("sessionId", json!(id))]),
    ));

    assert_eq!(result["id"], json!(id));
    assert_eq!(result["title"], json!("A chat"));
    assert_eq!(result["providerId"], json!("vercel"));
    assert_eq!(result["preview"], json!("earlier turn"));
    assert_eq!(result["entries"].as_array().unwrap().len(), 1);
    assert_eq!(result["entries"][0]["kind"], json!("user"));
}

#[test]
fn load_answers_a_tail_window_with_markers() {
    let dir = temp_data_dir("load-window");
    let id = uuid(3);
    let texts: Vec<String> = (0..250).map(|index| format!("m{index:03}")).collect();
    write_session(&dir, &id, &session_with_entries(&id, &texts));

    let host = Host::new();
    let mut state = bound_state();
    let result = success_result(call(
        &host,
        &dir,
        &mut state,
        request("r1", "session.load", &[("sessionId", json!(id))]),
    ));
    // The default window (200) is the tail; the markers name it so a
    // client knows what it holds and what to page for.
    assert_eq!(result["totalEntries"], json!(250));
    assert_eq!(result["entriesFrom"], json!(50));
    let entries = result["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 200);
    assert_eq!(entries[0]["text"], json!("m050"));
    assert_eq!(entries[199]["text"], json!("m249"));
}

#[test]
fn load_honors_an_explicit_entries_window() {
    let dir = temp_data_dir("load-window-explicit");
    let id = uuid(4);
    let texts: Vec<String> = (0..120).map(|index| format!("m{index:03}")).collect();
    write_session(&dir, &id, &session_with_entries(&id, &texts));

    let host = Host::new();
    let mut state = bound_state();
    let result = success_result(call(
        &host,
        &dir,
        &mut state,
        request(
            "r1",
            "session.load",
            &[("sessionId", json!(id)), ("entriesWindow", json!(10))],
        ),
    ));
    assert_eq!(result["entriesFrom"], json!(110));
    assert_eq!(result["totalEntries"], json!(120));
    assert_eq!(result["entries"].as_array().unwrap().len(), 10);

    // A window larger than the transcript is the whole transcript, still
    // marked as a window.
    let result = success_result(call(
        &host,
        &dir,
        &mut state,
        request(
            "r2",
            "session.load",
            &[("sessionId", json!(id)), ("entriesWindow", json!(2000))],
        ),
    ));
    assert_eq!(result["entriesFrom"], json!(0));
    assert_eq!(result["entries"].as_array().unwrap().len(), 120);
}

#[test]
fn load_refuses_an_invalid_entries_window() {
    let dir = temp_data_dir("load-window-bad");
    let id = uuid(5);
    write_session(&dir, &id, &pretty_session(&id, None, "A chat", true));
    let host = Host::new();
    let mut state = bound_state();
    for value in [json!(0), json!(2001), json!("x"), json!(1.5)] {
        let error = failure_text(call(
            &host,
            &dir,
            &mut state,
            request(
                "r1",
                "session.load",
                &[("sessionId", json!(id)), ("entriesWindow", value)],
            ),
        ));
        assert_eq!(
            error,
            "entriesWindow must be an integer between 1 and 2000."
        );
    }
}

#[test]
fn the_windowed_load_feeds_entries_range_for_the_rest() {
    let dir = temp_data_dir("load-window-range");
    let id = uuid(6);
    let texts: Vec<String> = (0..120).map(|index| format!("m{index:03}")).collect();
    write_session(&dir, &id, &session_with_entries(&id, &texts));
    let host = Host::new();
    let mut state = bound_state();
    // Only the tail arrives with the load ...
    let loaded = success_result(call(
        &host,
        &dir,
        &mut state,
        request(
            "r1",
            "session.load",
            &[("sessionId", json!(id)), ("entriesWindow", json!(10))],
        ),
    ));
    assert_eq!(loaded["entriesFrom"], json!(110));
    // ... and the start of the same chat is one page away.
    let page = success_result(call(
        &host,
        &dir,
        &mut state,
        request(
            "r2",
            "session.entries-range",
            &[("before", json!(10)), ("limit", json!(10))],
        ),
    ));
    assert_eq!(page["from"], json!(0));
    assert_eq!(page["to"], json!(10));
    let entries = page["entries"].as_array().unwrap();
    assert_eq!(entries[0]["text"], json!("m000"));
    assert_eq!(entries.len(), 10);
}

#[test]
fn load_refuses_the_machine_custodian_with_the_fixed_sentence() {
    let dir = temp_data_dir("load-custodian");
    let host = Host::new();
    let mut state = bound_state();

    let error = failure_text(call(
        &host,
        &dir,
        &mut state,
        request(
            "r1",
            "session.load",
            &[(
                "sessionId",
                json!(openbutler_daemon::state::CUSTODIAN_SESSION_ID),
            )],
        ),
    ));
    assert_eq!(
        error,
        "The machine custodian is the host's own conversation; it is not a chat to open."
    );
}

#[test]
fn load_reports_an_unknown_chat_as_not_found() {
    let dir = temp_data_dir("load-unknown");
    let host = Host::new();
    let mut state = bound_state();

    let error = failure_text(call(
        &host,
        &dir,
        &mut state,
        request("r1", "session.load", &[("sessionId", json!(uuid(9)))]),
    ));
    assert_eq!(error, "Chat not found.");
}

#[test]
fn load_validates_the_payload_with_the_fixed_wording() {
    let dir = temp_data_dir("load-validate");
    let host = Host::new();
    let mut state = bound_state();

    let error = failure_text(call(
        &host,
        &dir,
        &mut state,
        request("r1", "session.load", &[]),
    ));
    assert_eq!(
        error,
        "sessionId must be a non-empty string under 64 bytes."
    );

    let error = failure_text(call(
        &host,
        &dir,
        &mut state,
        request(
            "r1",
            "session.load",
            &[("sessionId", json!("a".repeat(65)))],
        ),
    ));
    assert_eq!(
        error,
        "sessionId must be a non-empty string under 64 bytes."
    );
}

// ---------------------------------------------------------------- entries-range

#[test]
fn range_pages_the_chat_this_connection_loaded() {
    let dir = temp_data_dir("range-pages");
    let id = uuid(2);
    let texts: Vec<String> = (0..120).map(|index| format!("m{index:03}")).collect();
    write_session(&dir, &id, &session_with_entries(&id, &texts));

    let host = Host::new();
    let mut state = bound_state();
    // A client pages the chat it just opened: load first, then ask.
    call(
        &host,
        &dir,
        &mut state,
        request("r1", "session.load", &[("sessionId", json!(id))]),
    );

    // Defaults: before = total (120), limit = 50 → the last 50 entries.
    let result = success_result(call(
        &host,
        &dir,
        &mut state,
        request("r2", "session.entries-range", &[]),
    ));
    assert_eq!(result["from"], json!(70));
    assert_eq!(result["to"], json!(120));
    assert_eq!(result["total"], json!(120));
    let entries = result["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 50);
    assert_eq!(entries[0]["text"], json!("m070"));

    // An explicit window: before 30, limit 10 → entries 20..30.
    let result = success_result(call(
        &host,
        &dir,
        &mut state,
        request(
            "r3",
            "session.entries-range",
            &[("before", json!(30)), ("limit", json!(10))],
        ),
    ));
    assert_eq!(result["from"], json!(20));
    assert_eq!(result["to"], json!(30));
    assert_eq!(result["total"], json!(120));

    // `before` beyond the transcript clamps to total.
    let result = success_result(call(
        &host,
        &dir,
        &mut state,
        request("r4", "session.entries-range", &[("before", json!(999))]),
    ));
    assert_eq!(result["to"], json!(120));
}

#[test]
fn range_without_a_loaded_chat_is_not_found() {
    let dir = temp_data_dir("range-nochat");
    let host = Host::new();
    let mut state = bound_state();

    let error = failure_text(call(
        &host,
        &dir,
        &mut state,
        request("r1", "session.entries-range", &[]),
    ));
    assert_eq!(error, "Chat not found.");
}

#[test]
fn range_validates_limit_and_before_with_the_fixed_wording() {
    let dir = temp_data_dir("range-validate");
    let id = uuid(3);
    let texts: Vec<String> = (0..5).map(|index| format!("m{index}")).collect();
    write_session(&dir, &id, &session_with_entries(&id, &texts));
    let host = Host::new();
    let mut state = bound_state();
    call(
        &host,
        &dir,
        &mut state,
        request("r1", "session.load", &[("sessionId", json!(id))]),
    );

    let error = failure_text(call(
        &host,
        &dir,
        &mut state,
        request("r2", "session.entries-range", &[("limit", json!(201))]),
    ));
    assert_eq!(error, "limit must be an integer between 1 and 200.");

    let error = failure_text(call(
        &host,
        &dir,
        &mut state,
        request("r2", "session.entries-range", &[("before", json!(-1))]),
    ));
    assert_eq!(
        error,
        "before must be an integer between 0 and 9007199254740991."
    );
}

#[test]
fn range_follows_the_last_loaded_chat_on_the_same_connection() {
    let dir = temp_data_dir("range-switch");
    let first = uuid(4);
    let second = uuid(5);
    let first_texts: Vec<String> = (0..120).map(|index| format!("a{index:03}")).collect();
    let second_texts: Vec<String> = (0..10).map(|index| format!("b{index:03}")).collect();
    write_session(&dir, &first, &session_with_entries(&first, &first_texts));
    write_session(&dir, &second, &session_with_entries(&second, &second_texts));

    let host = Host::new();
    let mut state = bound_state();
    call(
        &host,
        &dir,
        &mut state,
        request("r1", "session.load", &[("sessionId", json!(first))]),
    );
    call(
        &host,
        &dir,
        &mut state,
        request("r2", "session.load", &[("sessionId", json!(second))]),
    );

    let result = success_result(call(
        &host,
        &dir,
        &mut state,
        request("r3", "session.entries-range", &[]),
    ));
    assert_eq!(result["total"], json!(10));
    assert_eq!(result["from"], json!(0));
}

// ---------------------------------------------------------- session.find-attachment

#[test]
fn find_attachment_resolves_inline_images_and_nulls_everything_else() {
    let dir = temp_data_dir("find-attachment");
    let id = uuid(6);
    write_session(&dir, &id, &session_with_attachments(&id));

    let host = Host::new();
    let mut state = bound_state();
    call(
        &host,
        &dir,
        &mut state,
        request("r1", "session.load", &[("sessionId", json!(id))]),
    );

    let result = success_result(call(
        &host,
        &dir,
        &mut state,
        request(
            "r2",
            "session.find-attachment",
            &[("attachmentId", json!("att-png"))],
        ),
    ));
    assert_eq!(
        result,
        serde_json::to_value(InlineAttachment {
            path: "/w/shot.png".to_owned(),
            media_type: "image/png".to_owned(),
        })
        .unwrap()
    );

    // Present but non-inline (pdf) → explicit null, not a failure.
    let result = success_result(call(
        &host,
        &dir,
        &mut state,
        request(
            "r3",
            "session.find-attachment",
            &[("attachmentId", json!("att-pdf"))],
        ),
    ));
    assert_eq!(result, Value::Null);

    // Absent id → null as well.
    let result = success_result(call(
        &host,
        &dir,
        &mut state,
        request(
            "r4",
            "session.find-attachment",
            &[("attachmentId", json!("nope"))],
        ),
    ));
    assert_eq!(result, Value::Null);
}

#[test]
fn find_attachment_validates_the_payload_and_needs_a_loaded_chat() {
    let dir = temp_data_dir("find-attachment-guard");
    let host = Host::new();
    let mut state = bound_state();

    let error = failure_text(call(
        &host,
        &dir,
        &mut state,
        request("r1", "session.find-attachment", &[]),
    ));
    assert_eq!(
        error,
        "attachmentId must be a non-empty string under 128 bytes."
    );

    // Valid id, no chat on this connection → not found: this build records
    // a chat only through a successful load on the same connection.
    let error = failure_text(call(
        &host,
        &dir,
        &mut state,
        request(
            "r2",
            "session.find-attachment",
            &[("attachmentId", json!("att-png"))],
        ),
    ));
    assert_eq!(error, "Chat not found.");
}

// The payload helpers are part of the daemon's contract too: re-pin the two
// entry points so a signature drift fails here as well as in the protocol
// crate's own suite.
#[test]
fn payload_entry_points_stay_re_exported() {
    let error = require_string(None, "sessionId", 64).map_err(|e| e.to_string());
    assert_eq!(
        error.unwrap_err(),
        "sessionId must be a non-empty string under 64 bytes."
    );
    let map = Map::new();
    let _ = load_session_id(&map);
}

// The current-chat shortcut (L3621) and the busy gate (L3626): loading your
// own chat succeeds mid-run; switching away from a running chat does not.
// The pair is the contract — the gate without the shortcut breaks the first.
#[test]
fn loading_the_current_chat_skips_the_busy_gate() {
    let dir = temp_data_dir("load-shortcut");
    let id = uuid(1);
    write_session(&dir, &id, &pretty_session(&id, None, "Busy chat", true));
    let host = Host::new();
    let executor = common::RecordingExecutor::default();
    host.set_executor(std::sync::Arc::new(executor.clone()));
    let mut state = bound_state();
    let first = call(
        &host,
        &dir,
        &mut state,
        request("r1", "session.load", &[("sessionId", json!(id))]),
    );
    success_result(first);
    *executor.busy.lock().unwrap() = true;

    // L3621: the current chat answers itself before any busy gate.
    let again = call(
        &host,
        &dir,
        &mut state,
        request("r2", "session.load", &[("sessionId", json!(id))]),
    );
    let result = success_result(again);
    assert_eq!(result["id"], json!(id));
}

#[test]
fn switching_away_from_a_running_chat_is_refused() {
    let dir = temp_data_dir("load-busy");
    let current = uuid(1);
    let other = uuid(2);
    write_session(
        &dir,
        &current,
        &pretty_session(&current, None, "Running", true),
    );
    write_session(&dir, &other, &pretty_session(&other, None, "Target", true));
    let host = Host::new();
    let executor = common::RecordingExecutor::default();
    host.set_executor(std::sync::Arc::new(executor.clone()));
    let mut state = bound_state();
    let first = call(
        &host,
        &dir,
        &mut state,
        request("r1", "session.load", &[("sessionId", json!(current))]),
    );
    success_result(first);
    *executor.busy.lock().unwrap() = true;

    // L3626: the controller is busy, so the switch is refused by sentence.
    let error = failure_text(call(
        &host,
        &dir,
        &mut state,
        request("r2", "session.load", &[("sessionId", json!(other))]),
    ));
    assert_eq!(error, "Wait for the current run before switching chats.");
}
