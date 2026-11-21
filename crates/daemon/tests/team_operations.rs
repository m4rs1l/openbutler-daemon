//! `TeamOperations` and the service helpers — fail-before pins for the team
//! slice's T2 (business rules over the store, still no network).
//!
//! Source: legacy `team/operations.ts` (254 lines) and `team/service.ts`
//! (116 lines), read 2026-11-22. Every refusal sentence and HTTP status is
//! the source's. Deliberately not here: the team server and channel client.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::temp_data_dir;

use openbutler_daemon::team::operations::{PostMessageResult, TeamError, TeamOperations};
use openbutler_daemon::team::service;
use openbutler_daemon::team::store::{Removal, TeamStore};

fn ops(dir: &std::path::Path) -> TeamStore {
    TeamStore::open(&dir.join("team.db")).expect("store opens")
}

fn error_text(error: &TeamError) -> String {
    error.to_string()
}

/// Seeds an active member through the store's own join path (fixture, not
/// the operation under test).
fn add_member(store: &TeamStore, name: &str) {
    store
        .create_join_request(&format!("jr_{name}"), name, "code-fixture", 2)
        .unwrap();
    store
        .decide_join(
            &format!("jr_{name}"),
            true,
            Some("rt_fixture"),
            Some("hx_fixture"),
            3,
        )
        .unwrap();
}

#[test]
fn member_and_team_names_validate_with_the_source_sentences() {
    assert_eq!(service::validate_member_name("  ab_-9 ").unwrap(), "ab_-9");
    let error = service::validate_member_name("bad name!").expect_err("space refused");
    assert_eq!(
        error,
        "\"bad name!\" is not a valid member name. Use 1-64 letters, digits, dashes, or underscores."
    );
    let reserved = service::validate_member_name("GROUP").expect_err("reserved");
    assert_eq!(
        reserved,
        "\"GROUP\" is reserved and cannot be used as a member name."
    );
    // The team name follows the member rule without the reserved set.
    assert_eq!(service::validate_team_name("lab-team").unwrap(), "lab-team");
    assert_eq!(
        service::validate_team_name("spaced team").expect_err("space refused"),
        "\"spaced team\" is not a valid team name. Use 1-64 letters, digits, dashes, or underscores."
    );
}

#[test]
fn ids_carry_their_prefixes_and_formats() {
    let message = service::make_message_id().unwrap();
    assert!(message.starts_with("m_") && message.len() == 26);
    let file = service::make_file_id().unwrap();
    assert!(file.starts_with("f_") && file.len() == 26);
    let ticket = service::make_ticket().unwrap();
    assert!(ticket.starts_with("rt_") && ticket.len() == 51);
    let team = service::make_team_id().unwrap();
    assert!(team.starts_with("t_") && team.len() == 26);
    let request = service::make_request_id().unwrap();
    assert!(request.starts_with("jr_") && request.len() == 27);
    let hub = service::make_hub_id().unwrap();
    assert_eq!(hub.len(), 32);
    assert!(hub.chars().all(|c| c.is_ascii_hexdigit()));
    // `kartal-4821`: word + exactly four digits.
    let code = service::make_invite_code().unwrap();
    let (word, digits) = code.rsplit_once('-').expect("word-dash-digits");
    assert_eq!(digits.len(), 4);
    assert!(digits.chars().all(|c| c.is_ascii_digit()));
    assert!(!word.is_empty());
    // Fingerprint: three uppercase hex groups of four.
    let fingerprint = service::fingerprint_of("deadbeef");
    assert_eq!(fingerprint.len(), 14);
    assert!(
        fingerprint
            .split(':')
            .all(|group| group.len() == 4 && group.chars().all(|c| c.is_ascii_hexdigit()))
    );
    assert!(
        fingerprint
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == ':')
    );
}

#[test]
fn hash_ticket_is_the_sha256_of_the_ticket() {
    let ticket = "rt_example";
    let expected = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(ticket.as_bytes());
        format!("{:x}", hasher.finalize())
    };
    assert_eq!(service::hash_ticket(ticket), expected);
    assert_eq!(service::hash_ticket(ticket).len(), 64);
}

#[test]
fn recipients_resolve_or_refuse() {
    let dir = temp_data_dir("ops-recipient");
    let store = ops(&dir);
    store.init_hub("h", "t", "Lab", "Alice", 1).unwrap();
    let operations = TeamOperations { store: &store };

    assert_eq!(operations.resolve_recipient(" GROUP ").unwrap(), "group");
    assert_eq!(operations.resolve_recipient("aLiCe").unwrap(), "Alice");
    let unknown = operations.resolve_recipient("bob").expect_err("unknown");
    assert_eq!(
        error_text(&unknown),
        "No member named \"bob\" in this team."
    );
    assert_eq!(unknown.status, 404);
    let invalid = operations.resolve_recipient("!!").expect_err("invalid");
    assert_eq!(error_text(&invalid), "\"!!\" is not a valid recipient.");
    assert_eq!(invalid.status, 400);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn group_reads_validate_their_limits() {
    let dir = temp_data_dir("ops-group");
    let store = ops(&dir);
    store.init_hub("h", "t", "Lab", "Alice", 1).unwrap();
    let operations = TeamOperations { store: &store };

    let error = operations.read_group("alice", 0).expect_err("limit floor");
    assert_eq!(error_text(&error), "Group limit must be 1-200.");
    let error = operations
        .read_group("alice", 201)
        .expect_err("limit ceiling");
    assert_eq!(error_text(&error), "Group limit must be 1-200.");

    let empty = operations.read_group("alice", 10).unwrap();
    assert!(empty.messages.is_empty());
    assert_eq!(empty.cursor, 0);

    let error = operations
        .acknowledge_group("alice", 5)
        .expect_err("ack past latest");
    assert_eq!(
        error_text(&error),
        "Invalid group acknowledgement sequence."
    );
    assert_eq!(error.status, 400);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn messages_validate_then_dedup_within_one_transaction() {
    let dir = temp_data_dir("ops-post");
    let store = ops(&dir);
    store.init_hub("h", "t", "Lab", "Alice", 1).unwrap();
    let operations = TeamOperations { store: &store };

    let error = operations
        .post_message(
            "alice",
            &openbutler_daemon::team::operations::NewMessage {
                recipient: "group".to_owned(),
                body: "   ".to_owned(),
                reply_to: None,
                msg_id: None,
                now: 10,
            },
        )
        .expect_err("empty body");
    assert_eq!(error_text(&error), "A message body cannot be empty.");

    let long = "x".repeat(8001);
    let error = operations
        .post_message(
            "alice",
            &openbutler_daemon::team::operations::NewMessage {
                recipient: "group".to_owned(),
                body: long,
                reply_to: None,
                msg_id: None,
                now: 10,
            },
        )
        .expect_err("long body");
    assert_eq!(
        error_text(&error),
        "A message body is limited to 8000 characters."
    );

    let body = "m_".repeat(200); // 400 chars > 256 budget
    let error = operations
        .post_message(
            "alice",
            &openbutler_daemon::team::operations::NewMessage {
                recipient: "group".to_owned(),
                body: "hello".to_owned(),
                reply_to: None,
                msg_id: Some(body),
                now: 10,
            },
        )
        .expect_err("long id");
    assert_eq!(error_text(&error), "Message id is too long.");

    let input = openbutler_daemon::team::operations::NewMessage {
        recipient: "GROUP".to_owned(),
        body: "hello".to_owned(),
        reply_to: None,
        msg_id: Some("m_fixed00000000000000000001".to_owned()),
        now: 20,
    };
    let first = operations.post_message("alice", &input).unwrap();
    let PostMessageResult::Sent { message } = first else {
        panic!("first post must send");
    };
    assert_eq!(message.recipient, "group"); // canonical recipient
    // Same id + same operation → duplicate, never a second row.
    let repeat = operations.post_message("alice", &input).unwrap();
    let PostMessageResult::Duplicate { msg_id } = repeat else {
        panic!("retry must dedup");
    };
    assert_eq!(msg_id, "m_fixed00000000000000000001");
    assert!(store.has_message("m_fixed00000000000000000001").unwrap());
    // Same id, different payload → the id belongs elsewhere.
    let conflict = operations.post_message(
        "alice",
        &openbutler_daemon::team::operations::NewMessage {
            recipient: "group".to_owned(),
            body: "different".to_owned(),
            reply_to: None,
            msg_id: Some("m_fixed00000000000000000001".to_owned()),
            now: 30,
        },
    );
    let error = conflict.expect_err("conflict");
    assert_eq!(
        error_text(&error),
        "Message id already belongs to a different operation."
    );
    assert_eq!(error.status, 409);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_reply_acknowledges_the_inbox_it_answers() {
    let dir = temp_data_dir("ops-reply");
    let store = ops(&dir);
    store.init_hub("h", "t", "Lab", "Alice", 1).unwrap();
    add_member(&store, "bob");
    let operations = TeamOperations { store: &store };

    operations
        .post_message(
            "alice",
            &openbutler_daemon::team::operations::NewMessage {
                recipient: "bob".to_owned(),
                body: "ping".to_owned(),
                reply_to: None,
                msg_id: Some("m_one0000000000000000000001".to_owned()),
                now: 10,
            },
        )
        .unwrap();
    assert_eq!(store.unread_dm_count("bob").unwrap(), 1);

    // Bob answers: the reply acknowledges the message in bob's own inbox.
    operations
        .post_message(
            "bob",
            &openbutler_daemon::team::operations::NewMessage {
                recipient: "alice".to_owned(),
                body: "pong".to_owned(),
                reply_to: Some("m_one0000000000000000000001".to_owned()),
                msg_id: Some("m_two0000000000000000000002".to_owned()),
                now: 20,
            },
        )
        .unwrap();
    assert_eq!(store.unread_dm_count("bob").unwrap(), 0);
    // Alice's inbox holds bob's reply (the canonical stored name).
    assert_eq!(store.unread_dm_count("Alice").unwrap(), 1);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn file_registration_validates_announces_once_and_scopes_access() {
    let dir = temp_data_dir("ops-file");
    let store = ops(&dir);
    store.init_hub("h", "t", "Lab", "Alice", 1).unwrap();
    add_member(&store, "bob");
    let operations = TeamOperations { store: &store };

    let bad_name = openbutler_daemon::team::operations::NewFile {
        recipient: "bob".to_owned(),
        name: "line\nbreak".to_owned(),
        size: 4,
        sha256: "a".repeat(64),
        file_id: None,
        now: 10,
    };
    let error = operations
        .register_file("alice", &bad_name)
        .expect_err("newline name");
    assert_eq!(
        error_text(&error),
        "A file name must be 1-255 characters without line breaks."
    );

    let bad_sha = openbutler_daemon::team::operations::NewFile {
        sha256: "NOTHEX".to_owned(),
        name: "notes.txt".to_owned(),
        ..bad_name.clone()
    };
    let error = operations
        .register_file("alice", &bad_sha)
        .expect_err("bad sha");
    assert_eq!(error_text(&error), "A file needs a lowercase hex sha256.");

    let oversized = openbutler_daemon::team::operations::NewFile {
        size: 1024 * 1024 * 1024 + 1,
        name: "notes.txt".to_owned(),
        ..bad_name.clone()
    };
    let error = operations
        .register_file("alice", &oversized)
        .expect_err("too big");
    assert_eq!(error_text(&error), "A file is limited to 1024 MB.");
    assert_eq!(error.status, 413);

    let negative = openbutler_daemon::team::operations::NewFile {
        size: -1,
        name: "notes.txt".to_owned(),
        ..bad_name.clone()
    };
    let error = operations
        .register_file("alice", &negative)
        .expect_err("negative");
    assert_eq!(
        error_text(&error),
        "A file size must be a non-negative integer."
    );

    let good = openbutler_daemon::team::operations::NewFile {
        recipient: "bob".to_owned(),
        name: "notes.txt".to_owned(),
        size: 4,
        sha256: "a".repeat(64),
        file_id: Some("f_0123456789abcdef0123".to_owned()),
        now: 20,
    };
    let announced = operations.register_file("alice", &good).unwrap();
    assert_eq!(announced.kind, "file");
    assert_eq!(announced.body, "notes.txt");
    let file_id = announced.file_id.clone().expect("file id announced");
    // Retry with the same payload → the same announcement, one row.
    let again = operations.register_file("alice", &good).unwrap();
    assert_eq!(again.msg_id, announced.msg_id);
    assert!(store.has_message(&announced.msg_id).unwrap());

    // Access is address-scoped: recipient, group share, or the sender.
    let file = operations.file_for_member("bob", &file_id).unwrap();
    assert_eq!(file.name, "notes.txt");
    let error = operations
        .file_for_member("mallory", &file_id)
        .expect_err("not addressed");
    assert_eq!(error_text(&error), "This file is not addressed to you.");
    assert_eq!(error.status, 403);
    let error = operations
        .file_for_member("mallory", "f_missing")
        .expect_err("unknown id");
    assert_eq!(error_text(&error), "No such file.");
    assert_eq!(error.status, 404);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn invites_need_an_initialized_hub_and_write_audit() {
    let dir = temp_data_dir("ops-invite");
    let store = ops(&dir);
    let operations = TeamOperations { store: &store };

    let error = operations
        .create_invite("alice", 60_000, 100)
        .expect_err("uninitialized");
    assert_eq!(
        error_text(&error),
        "Team store is not initialized. Run team init first."
    );

    operations.init_hub("h", "t", "Lab", "Alice", 1).unwrap();
    let created = operations.create_invite("alice", 60_000, 100).unwrap();
    assert!(created.code.contains('-'));
    assert_eq!(created.fingerprint.len(), 14);
    let audit = store.list_audit(10).unwrap();
    assert_eq!(audit[0].kind, "invite.created");
    assert_eq!(audit[0].detail.as_deref(), Some(created.code.as_str()));

    assert!(
        operations
            .revoke_invite("alice", &created.code, 200)
            .unwrap()
    );
    assert!(
        !operations
            .revoke_invite("alice", &created.code, 300)
            .unwrap()
    );
    let audit = store.list_audit(10).unwrap();
    assert_eq!(audit[0].kind, "invite.revoked"); // the failed revoke wrote nothing
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn join_requests_follow_the_lifecycle_sentences() {
    let dir = temp_data_dir("ops-join");
    let store = ops(&dir);
    let operations = TeamOperations { store: &store };
    operations.init_hub("h", "t", "Lab", "Alice", 1).unwrap();
    let invite = operations.create_invite("alice", 60_000, 100).unwrap();

    let error = operations
        .request_join("code-none", "bob", 150)
        .expect_err("unknown code");
    assert_eq!(error_text(&error), "Invite code is invalid.");
    assert_eq!(error.status, 404);

    let request = operations.request_join(&invite.code, "bob", 150).unwrap();
    assert_eq!(request.status, "pending");
    assert_eq!(store.list_pending_joins().unwrap().len(), 1);
    // The invite was consumed: a second join with it is invalid.
    let error = operations
        .request_join(&invite.code, "carol", 160)
        .expect_err("used code");
    assert_eq!(error_text(&error), "Invite code is invalid.");

    // A name already taken refuses at request time (the hub member exists).
    let invite2 = operations.create_invite("alice", 60_000, 170).unwrap();
    let error = operations
        .request_join(&invite2.code, "ALICE", 180)
        .expect_err("taken");
    assert_eq!(
        error_text(&error),
        "\"ALICE\" is already taken in this team. Pick another name."
    );
    assert_eq!(error.status, 409);

    let error = operations
        .decide_join("alice", "jr_missing", true, 200)
        .expect_err("unknown request");
    assert_eq!(error_text(&error), "Unknown join request.");
    assert_eq!(error.status, 404);

    let denied = operations
        .decide_join("alice", &request.id, false, 200)
        .unwrap();
    assert_eq!(denied.decided, "denied");
    assert_eq!(denied.name, "bob");
    let error = operations
        .decide_join("alice", &request.id, true, 210)
        .expect_err("already decided");
    assert_eq!(error_text(&error), "Request is already denied.");
    assert_eq!(error.status, 409);
    assert_eq!(store.list_audit(10).unwrap()[0].kind, "join.denied");
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn approving_a_taken_name_refuses_and_leaving_separates_from_removing() {
    let dir = temp_data_dir("ops-approve");
    let store = ops(&dir);
    let operations = TeamOperations { store: &store };
    operations.init_hub("h", "t", "Lab", "Alice", 1).unwrap();
    add_member(&store, "dave");
    let invite = operations.create_invite("alice", 60_000, 100).unwrap();
    let request = operations.request_join(&invite.code, "Carla", 150).unwrap();
    // The name becomes taken between request and approval: a member joins
    // under the same name first.
    let invite2 = operations.create_invite("alice", 60_000, 160).unwrap();
    let second = operations
        .request_join(&invite2.code, "carla", 170)
        .unwrap();
    let approved = operations
        .decide_join("alice", &second.id, true, 180)
        .unwrap();
    assert_eq!(approved.decided, "approved");
    // Now the first request's name is taken → the approval sentence.
    let error = operations
        .decide_join("alice", &request.id, true, 190)
        .expect_err("name taken at approval");
    assert_eq!(
        error_text(&error),
        "\"Carla\" is already taken in this team. Deny the request and invite them to pick another name."
    );
    assert_eq!(error.status, 409);

    // The hub cannot leave; leaving a member writes member.left, removing
    // writes member.removed.
    let error = operations.leave("alice", 200).expect_err("hub leaves");
    assert_eq!(
        error_text(&error),
        "The hub cannot leave its own team. Shut the team down instead."
    );
    assert_eq!(error.status, 403);

    assert_eq!(
        operations.remove_member("alice", "carla", 210).unwrap(),
        Removal::Removed
    );
    // Leaving is a different path: a fresh member's own exit writes member.left.
    assert_eq!(operations.leave("dave", 220).unwrap(), ());
    let kinds: Vec<String> = store
        .list_audit(10)
        .unwrap()
        .into_iter()
        .map(|entry| entry.kind)
        .collect();
    assert!(kinds.contains(&"member.removed".to_owned()));
    assert!(kinds.contains(&"member.left".to_owned()));
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn wake_rules_follow_the_source_truth_table() {
    // A burst wakes once: the highest seen sequence advances the cursor;
    // group/ambient never reaches here (count 0).
    assert!(!service::should_wake_for_team_dms(0, 5, 5));
    assert!(!service::should_wake_for_team_dms(0, 6, 5));
    assert!(!service::should_wake_for_team_dms(3, 5, 5));
    assert!(service::should_wake_for_team_dms(3, 6, 5));
    assert!(service::should_wake_for_team_dms(1, 9, 0));
}
