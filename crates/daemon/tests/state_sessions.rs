#![allow(clippy::unwrap_used, clippy::expect_used)]
//! State-layer contract tests: session paths, header-only reads, listing.
//!
//! Fail-before: these run against fixture directories built per test, so the
//! record protocol (pretty JSON, boundary marker, id verification,
//! denormalised-field derivation, custodian exclusion, ordering, damage
//! tolerance) is pinned without touching any real install.

use std::fs;
use std::path::PathBuf;

use openbutler_daemon::state::{self, WorkspaceFilter};
use openbutler_protocol::SessionSummary;

mod common;

use common::{pretty_session, temp_data_dir as temp_root, uuid, write_session};

#[test]
fn list_reads_headers_orders_and_filters() {
    let root = temp_root("list");
    let older = uuid(1);
    let newer = uuid(2);
    let other_workspace = uuid(3);
    let no_message = uuid(4);
    let custodian = state::CUSTODIAN_SESSION_ID;

    // Older activity first-in-file, newer activity later: list must invert.
    write_session(
        &root,
        &older,
        &pretty_session(&older, None, "Older chat", true).replace("20:05:00.000Z", "20:01:00.000Z"),
    );
    write_session(
        &root,
        &newer,
        &pretty_session(&newer, None, "Newer chat", true),
    );
    write_session(
        &root,
        &other_workspace,
        &pretty_session(&other_workspace, Some("C:\\other"), "Elsewhere", true),
    );
    write_session(
        &root,
        &no_message,
        &pretty_session(&no_message, None, "Draft", false),
    );
    write_session(
        &root,
        custodian,
        &pretty_session(custodian, None, "Head", true),
    );

    let unfiltered = state::list_sessions(&root, &WorkspaceFilter::Any).expect("listing");
    let ids: Vec<&str> = unfiltered.iter().map(|entry| entry.id.as_str()).collect();
    // custodian never appears; drafts (no user message) never appear;
    // newest activity first, ties broken by id ascending: newer and
    // other_workspace share 20:05 (ids break the tie), older sits last.
    assert_eq!(
        ids,
        [newer.as_str(), other_workspace.as_str(), older.as_str()]
    );

    let rootless = state::list_sessions(&root, &WorkspaceFilter::Rootless).expect("filter");
    assert_eq!(
        rootless
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>(),
        [newer.as_str(), older.as_str()]
    );

    let exactly = state::list_sessions(&root, &WorkspaceFilter::Exactly("C:\\other".into()))
        .expect("exact filter");
    assert_eq!(exactly.len(), 1);
    assert_eq!(exactly[0].title, "Elsewhere");

    let entry = &unfiltered[0];
    assert_eq!(entry.title, "Newer chat");
    assert_eq!(entry.provider_id, "vercel");
    assert_eq!(
        entry.last_activity_at.as_deref(),
        Some("2026-11-15T20:05:00.000Z")
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn a_flat_layout_file_is_not_a_session() {
    let root = temp_root("flat");
    let id = uuid(5);
    fs::write(
        root.join("sessions").join(format!("{id}.json")),
        pretty_session(&id, None, "Stray file", true),
    )
    .unwrap();

    let sessions = state::list_sessions(&root, &WorkspaceFilter::Any).expect("listing");
    assert!(
        sessions.is_empty(),
        "one layout only: sessions/<id>/<id>.json"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn one_damaged_file_never_blocks_the_list() {
    let root = temp_root("damage");
    let good = uuid(6);
    write_session(&root, &good, &pretty_session(&good, None, "Survivor", true));
    let broken = uuid(7);
    fs::create_dir_all(root.join("sessions").join(&broken)).unwrap();
    fs::write(
        root.join("sessions")
            .join(&broken)
            .join(format!("{broken}.json")),
        "{ this is not json",
    )
    .unwrap();
    let wrong_id = uuid(8);
    write_session(
        &root,
        &wrong_id,
        &pretty_session(&uuid(9), None, "Impostor", true),
    );

    let sessions =
        state::list_sessions(&root, &WorkspaceFilter::Any).expect("listing survives damage");
    assert_eq!(sessions.len(), 1, "only the intact matching file lists");
    assert_eq!(sessions[0].id, good);

    fs::remove_dir_all(&root).ok();
}

#[test]
fn header_read_cuts_at_the_entries_boundary() {
    let root = temp_root("header");
    let id = uuid(10);
    write_session(&root, &id, &pretty_session(&id, None, "Header chat", true));
    let path = root.join("sessions").join(&id).join(format!("{id}.json"));

    let header = state::read_session_header(&path, &id).expect("header");
    assert_eq!(header.id, id);
    assert_eq!(header.title, "Header chat");
    assert!(header.has_user_message);
    assert_eq!(header.preview, "earlier turn");

    fs::remove_dir_all(&root).ok();
}

#[test]
fn header_read_rejects_a_mismatched_id() {
    let root = temp_root("idcheck");
    let id = uuid(11);
    write_session(&root, &id, &pretty_session(&id, None, "Real", true));
    let path = root.join("sessions").join(&id).join(format!("{id}.json"));

    // Parsing succeeded but the record names another session: the read must
    // refuse rather than hand back a header under the wrong identity.
    assert!(state::read_session_header(&path, &uuid(12)).is_none());

    fs::remove_dir_all(&root).ok();
}

#[test]
fn missing_denormalised_fields_are_derived_from_entries() {
    let root = temp_root("derive");
    let id = uuid(14);
    // A record saved before preview/hasUserMessage existed: header fields are
    // absent, but the transcript is there, so the four summaries derive.
    let body = format!(
        "{{\n  \"version\": 1,\n  \"id\": \"{id}\",\n  \"title\": \"Old record\",\n  \"createdAt\": \"2026-06-01T10:00:00.000Z\",\n  \"updatedAt\": \"2026-06-01T10:00:00.000Z\",\n  \"providerId\": \"vercel\",\n  \"modelId\": \"xiaomi/mimo-v2.6-flash\",\n  \"entries\": [\n    {{ \"kind\": \"user\", \"text\": \"  first   message  \" }},\n    {{ \"kind\": \"assistant\", \"text\": \"the  answer\" }}\n  ],\n  \"conversation\": []\n}}\n"
    );
    write_session(&root, &id, &body);
    let path = root.join("sessions").join(&id).join(format!("{id}.json"));

    let header = state::read_session_header(&path, &id).expect("derived header");
    assert!(header.has_user_message, "derived from the user entry");
    assert_eq!(header.preview, "first message");
    assert_eq!(header.first_user_message, "first message");
    assert_eq!(header.last_assistant_message, "the answer");

    fs::remove_dir_all(&root).ok();
}

#[test]
fn path_helpers_validate_ids_and_stay_inside_root() {
    // Non-UUID ids are refused before any path is built.
    assert!(state::session_directory(&root_placeholder(), "not-a-uuid").is_err());
    assert!(state::session_directory(&root_placeholder(), "../../escape").is_err());
    // Custodian id is valid shape; variant rules are part of validation.
    assert!(state::session_directory(&root_placeholder(), state::CUSTODIAN_SESSION_ID).is_ok());
    assert!(
        state::session_directory(&root_placeholder(), "00000000-0000-4000-8000-00000000000z")
            .is_err()
    );

    let root = temp_root("paths");
    let id = uuid(13);
    let dir = state::session_directory(&root, &id).expect("valid id");
    assert!(dir.starts_with(root.join("sessions")));

    fs::remove_dir_all(&root).ok();
}

fn root_placeholder() -> PathBuf {
    PathBuf::from("unused")
}

// The listing rows are protocol summaries: one type, no local duplicate.
#[allow(dead_code)]
fn _summary_type_check(entry: &SessionSummary) -> &str {
    &entry.id
}
