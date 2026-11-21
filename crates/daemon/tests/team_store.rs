//! `TeamStore` at the hub's data layer — fail-before pins for the team
//! slice's T1 (data layer, no network).
//!
//! Source: legacy `team/store.ts` (697 lines, read 2026-11-21), with the
//! wire types from `team/contracts.ts` and the coordination types from
//! `team/coordination.ts`. Deliberately not here: hub operations
//! (`team/operations.ts`), the team server and the channel client.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use openbutler_daemon::team::coordination::{PeerProfile, PeerWork, WorkState};
use openbutler_daemon::team::store::{MessageInput, Removal, TeamFile, TeamStore, WorkNotice};

fn temp_store(tag: &str) -> (PathBuf, TeamStore) {
    let dir = std::env::temp_dir().join(format!("ob-test-team-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("team.db");
    let store = TeamStore::open(&path).expect("store opens");
    (dir, store)
}

fn msg(msg_id: &str, sender: &str, recipient: &str, body: &str, at: i64) -> MessageInput {
    MessageInput {
        msg_id: msg_id.to_owned(),
        sender: sender.to_owned(),
        recipient: recipient.to_owned(),
        body: body.to_owned(),
        created_at: at,
        ..Default::default()
    }
}

fn profile(revision: i64) -> PeerProfile {
    PeerProfile {
        description: "runs the archive".to_owned(),
        revision,
        updated_at: 1_700_000_000_000,
    }
}

#[test]
fn a_fresh_store_creates_the_team_schema() {
    let (dir, store) = temp_store("fresh");
    assert!(!store.is_initialized().unwrap());
    assert_eq!(store.team_id().unwrap(), "");
    assert!(store.list_members().unwrap().is_empty());
    assert!(store.list_invites(0).unwrap().is_empty());
    assert!(store.list_pending_joins().unwrap().is_empty());
    assert_eq!(store.group_latest_seq().unwrap(), 0);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn older_databases_gain_the_message_kind_and_file_columns_in_place() {
    // store.ts constructor: PRAGMA table_info then ALTER TABLE — a
    // pre-kind database keeps working without a rebuild.
    let dir = std::env::temp_dir().join(format!("ob-test-team-migrate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("team.db");
    {
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE team_messages (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  msg_id TEXT NOT NULL UNIQUE,
  sender TEXT NOT NULL,
  recipient TEXT NOT NULL,
  body TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  reply_to TEXT
);",
            )
            .unwrap();
    }
    let store = TeamStore::open(&path).expect("store opens over the legacy table");
    let posted = store
        .post_message(&msg("m1", "alice", "group", "hello", 1_700_000_000_000))
        .unwrap();
    assert_eq!(posted.kind, "text");
    assert_eq!(posted.file_id, None);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_hub_initializes_exactly_once() {
    let (dir, store) = temp_store("init");
    store
        .init_hub("hub-1", "team-1", "Lab", "alice", 1_700_000_000_000)
        .expect("first init");
    assert!(store.is_initialized().unwrap());
    assert_eq!(store.team_id().unwrap(), "team-1");
    let members = store.list_members().unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].name, "alice");
    assert_eq!(members[0].created_at, 1_700_000_000_000);

    let error = store
        .init_hub("hub-2", "team-2", "Other", "bob", 1)
        .expect_err("second init refused");
    assert_eq!(
        error.to_string(),
        "This machine already initialized a team. Leave or reset it first."
    );
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn invites_are_single_use_and_prune_on_expiry() {
    let (dir, store) = temp_store("invite");
    store.create_invite("code-1", 1_000, 100).expect("create");
    assert_eq!(
        store.get_invite("code-1", 500).unwrap().unwrap().code,
        "code-1"
    );
    assert!(store.consume_invite("code-1", "bob", 600).unwrap());
    // Single-use race: a second consume fails and the invite is gone.
    assert!(!store.consume_invite("code-1", "eve", 700).unwrap());
    assert!(store.get_invite("code-1", 700).unwrap().is_none());

    store.create_invite("code-2", 10, 100).expect("create");
    assert!(store.get_invite("code-2", 200).unwrap().is_none());
    // Expiry prunes on read: the dead row is gone, not merely hidden.
    store.create_invite("code-3", 10, 100).expect("create");
    assert!(store.list_invites(200).unwrap().is_empty());
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_revoked_invite_disappears() {
    let (dir, store) = temp_store("revoke");
    store.create_invite("code-1", 10_000, 100).expect("create");
    assert!(store.revoke_invite("code-1").unwrap());
    assert!(!store.revoke_invite("code-1").unwrap());
    assert!(store.list_invites(200).unwrap().is_empty());
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn names_match_case_insensitively_and_retired_names_stay_taken() {
    let (dir, store) = temp_store("names");
    // A retired member's name is never reusable — retirement is exercised
    // through a joined member (the hub itself is never removable).
    store.init_hub("h", "t", "Lab", "Hub", 1).unwrap();
    store
        .create_join_request("jr_seed", "Alice", "code-1", 2)
        .unwrap();
    store
        .decide_join("jr_seed", true, Some("rt_seed"), Some("hx_seed"), 3)
        .unwrap();
    assert!(store.name_taken("alice").unwrap());
    assert!(store.name_taken("ALICE").unwrap());
    // The canonical spelling is the stored one (recipient resolution).
    assert_eq!(
        store.canonical_member_name("aLiCe").unwrap().as_deref(),
        Some("Alice")
    );
    assert_eq!(store.canonical_member_name("bob").unwrap(), None);

    assert_eq!(store.remove_member("alice", 500).unwrap(), Removal::Removed);
    assert_eq!(store.remove_member("alice", 600).unwrap(), Removal::Missing);
    // Retired names are never reusable — `nameTaken` ignores removal.
    assert!(store.name_taken("ALICE").unwrap());
    // …while canonical resolution only serves active members.
    assert_eq!(store.canonical_member_name("alice").unwrap(), None);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_hub_member_can_never_be_removed() {
    let (dir, store) = temp_store("hub-remove");
    store.init_hub("h", "t", "Lab", "alice", 1).unwrap();
    assert_eq!(store.remove_member("alice", 10).unwrap(), Removal::Hub);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn join_requests_decide_once_and_issue_a_ticket() {
    let (dir, store) = temp_store("join");
    store
        .create_join_request("req-1", "bob", "code-1", 100)
        .unwrap();
    let pending = store.list_pending_joins().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].status, "pending");

    // Approving without a ticket is a refusal, not a half-decision.
    let error = store
        .decide_join("req-1", true, None, None, 200)
        .expect_err("ticket required");
    assert_eq!(error.to_string(), "Approving a join needs a ticket.");

    assert!(
        store
            .decide_join("req-1", true, Some("raw-ticket"), Some("hash-1"), 300)
            .unwrap()
    );
    let members = store.list_members().unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].name, "bob");
    assert_eq!(
        store.find_by_ticket_hash("hash-1").unwrap().unwrap().name,
        "bob"
    );
    // Deciding twice is refused (status no longer pending).
    assert!(
        !store
            .decide_join("req-1", true, Some("t2"), Some("h2"), 400)
            .unwrap()
    );
    assert!(store.list_pending_joins().unwrap().is_empty());

    store
        .create_join_request("req-2", "eve", "code-1", 500)
        .unwrap();
    assert!(store.decide_join("req-2", false, None, None, 600).unwrap());
    assert_eq!(
        store.get_join_request("req-2").unwrap().unwrap().status,
        "denied"
    );
    assert_eq!(store.list_members().unwrap().len(), 1);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn approving_a_name_that_is_taken_decides_nothing() {
    let (dir, store) = temp_store("join-dup");
    store.init_hub("h", "t", "Lab", "alice", 1).unwrap();
    store
        .create_join_request("req-1", "ALICE", "code-1", 100)
        .unwrap();
    assert!(
        !store
            .decide_join("req-1", true, Some("raw"), Some("hash"), 200)
            .unwrap()
    );
    assert_eq!(
        store.get_join_request("req-1").unwrap().unwrap().status,
        "pending"
    );
    assert_eq!(store.list_members().unwrap().len(), 1);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_approved_ticket_survives_retries_until_confirmed_or_expires() {
    let (dir, store) = temp_store("ticket");
    store
        .create_join_request("req-1", "bob", "code-1", 100)
        .unwrap();
    store
        .decide_join("req-1", true, Some("raw-ticket"), Some("hash-1"), 200)
        .unwrap();
    // A lost response can retry: peek does not consume.
    assert_eq!(
        store.peek_ticket("req-1", 300, 60_000).unwrap().as_deref(),
        Some("raw-ticket")
    );
    assert_eq!(
        store.peek_ticket("req-1", 400, 60_000).unwrap().as_deref(),
        Some("raw-ticket")
    );
    store.confirm_ticket("req-1").unwrap();
    assert_eq!(store.peek_ticket("req-1", 500, 60_000).unwrap(), None);

    // Pickup TTL: an unconfirmed ticket is forgotten after the window.
    store
        .create_join_request("req-2", "eve", "code-1", 600)
        .unwrap();
    store
        .decide_join("req-2", true, Some("late"), Some("hash-2"), 700)
        .unwrap();
    assert_eq!(
        store.peek_ticket("req-2", 700 + 60_001, 60_000).unwrap(),
        None
    );
    assert_eq!(
        store.peek_ticket("req-2", 700 + 60_002, 60_000).unwrap(),
        None
    );
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn messages_carry_total_ordering_and_id_dedup() {
    let (dir, store) = temp_store("messages");
    let first = store
        .post_message(&msg("m1", "alice", "group", "one", 10))
        .unwrap();
    let second = store
        .post_message(&msg("m2", "bob", "group", "two", 20))
        .unwrap();
    assert!(second.seq > first.seq);
    assert_eq!(first.kind, "text");
    assert!(store.has_message("m1").unwrap());
    assert!(!store.has_message("m9").unwrap());
    assert_eq!(store.get_message("m2").unwrap().unwrap().sender, "bob");
    assert!(store.file_message("m1").unwrap().is_none());
    // Duplicate ids are refused by the unique index (retry dedup).
    assert!(
        store
            .post_message(&msg("m1", "alice", "group", "one", 30))
            .is_err()
    );
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn group_and_dm_reads_follow_their_cursors() {
    let (dir, store) = temp_store("reads");
    store
        .post_message(&msg("g1", "alice", "group", "a", 10))
        .unwrap();
    let g2 = store
        .post_message(&msg("g2", "bob", "group", "b", 20))
        .unwrap();
    store
        .post_message(&msg("d1", "alice", "bob", "hi", 30))
        .unwrap();
    store
        .post_message(&msg("d2", "bob", "alice", "yo", 40))
        .unwrap();

    // Group reads: strictly-after for live feeds, before-seq for history.
    let after = store.group_messages(1, 10).unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].msg_id, "g2");
    let history = store.group_history(0, 10).unwrap();
    assert_eq!(history[0].msg_id, "g2"); // newest first
    let page = store.group_history(g2.seq, 10).unwrap();
    assert_eq!(page[0].msg_id, "g1");
    assert_eq!(store.group_latest_seq().unwrap(), g2.seq);

    // Cursors only advance (MAX on conflict).
    assert_eq!(store.get_group_cursor("alice").unwrap(), 0);
    store.set_group_cursor("alice", 5).unwrap();
    store.set_group_cursor("alice", 2).unwrap();
    assert_eq!(store.get_group_cursor("alice").unwrap(), 5);

    // DM history is symmetric between the two members.
    let dm = store.dm_history("alice", "bob", 0, 10).unwrap();
    assert_eq!(dm.len(), 2);
    assert_eq!(dm[0].msg_id, "d2"); // newest first

    // Unread DMs: addressed only, acked ones drop out, re-ack counts zero.
    assert_eq!(store.unread_dm_count("bob").unwrap(), 1);
    let unread = store.unread_dms("bob", 10).unwrap();
    assert_eq!(unread[0].msg_id, "d1");
    assert_eq!(store.ack_dms("bob", &["d1"], 50).unwrap(), 1);
    assert_eq!(store.ack_dms("bob", &["d1"], 60).unwrap(), 0);
    assert_eq!(store.unread_dm_count("bob").unwrap(), 0);
    // Acks belong to their reader: alice still has hers unread.
    assert_eq!(store.unread_dm_count("alice").unwrap(), 1);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn peer_profiles_save_with_a_revision_guard() {
    let (dir, store) = temp_store("profile");
    assert!(store.peer_profile("alice").unwrap().is_none());
    // expected = 0 inserts once; a repeat insert is ignored.
    assert!(store.save_peer_profile("alice", &profile(1), 0).unwrap());
    assert!(!store.save_peer_profile("alice", &profile(2), 0).unwrap());
    // Updates must match the revision they read.
    assert!(!store.save_peer_profile("alice", &profile(3), 7).unwrap());
    assert!(store.save_peer_profile("alice", &profile(3), 1).unwrap());
    assert_eq!(store.peer_profile("alice").unwrap().unwrap().revision, 3);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn saving_work_posts_its_notice_atomically() {
    let (dir, store) = temp_store("work");
    store.init_hub("h", "t", "Lab", "alice", 1).unwrap();
    let work = PeerWork {
        id: "w-1".to_owned(),
        requester: "alice".to_owned(),
        assignee: "bob".to_owned(),
        original_terms: "ship it".to_owned(),
        terms: "ship it".to_owned(),
        terms_author: None,
        state: WorkState::Accepted,
        revision: 1,
        created_at: 100,
        updated_at: 200,
        last_actor: "bob".to_owned(),
        last_action: "accept".to_owned(),
        last_note: String::new(),
        result: None,
    };
    let notice = |msg_id: &str| WorkNotice {
        msg_id: msg_id.to_owned(),
        sender: "alice".to_owned(),
        recipient: "group".to_owned(),
        body: "accepted w-1".to_owned(),
        created_at: 300,
    };
    // Insert path: row and notice land together.
    assert!(store.save_peer_work(&work, 0, &notice("n1")).unwrap());
    assert!(store.has_message("n1").unwrap());
    assert_eq!(
        store.peer_work("w-1").unwrap().unwrap().state,
        WorkState::Accepted
    );
    // Revision mismatch: neither the row nor the notice changes.
    let bumped = PeerWork {
        revision: 2,
        ..work.clone()
    };
    assert!(!store.save_peer_work(&bumped, 9, &notice("n2")).unwrap());
    assert!(!store.has_message("n2").unwrap());
    assert_eq!(store.peer_work("w-1").unwrap().unwrap().revision, 1);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_audit_trail_is_append_only() {
    let (dir, store) = temp_store("audit");
    for index in 0..3 {
        store
            .append_audit("invite.created", "alice", Some("code"), 100 + index)
            .unwrap();
    }
    let entries = store.list_audit(2).unwrap();
    assert_eq!(entries.len(), 2);
    assert!(entries[0].seq > entries[1].seq); // most recent first
    assert_eq!(entries[0].kind, "invite.created");
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn presence_and_summaries_derive_from_the_store() {
    let (dir, store) = temp_store("presence");
    store.init_hub("h", "t", "Lab", "alice", 1_000).unwrap();
    store
        .create_join_request("req-1", "bob", "code-1", 2_000)
        .unwrap();
    store
        .decide_join("req-1", true, Some("raw"), Some("hash-1"), 3_000)
        .unwrap();
    store.touch_member("bob", 9_000).unwrap();
    store.save_peer_profile("alice", &profile(1), 0).unwrap();

    // The hub answers the query, so it is always online; a member is
    // online while seen inside the window.
    let online = store.online_member_names(10_000, 15_000).unwrap();
    assert!(online.contains("alice"));
    assert!(online.contains("bob"));
    let stale = store.online_member_names(50_000, 15_000).unwrap();
    assert!(stale.contains("alice")); // hub stays online
    assert!(!stale.contains("bob")); // last_seen 9_000 is outside 35_000

    let summaries = store.member_summaries(10_000, 15_000).unwrap();
    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0].name, "alice");
    assert!(summaries[0].online);
    assert_eq!(summaries[0].profile.as_ref().unwrap().revision, 1);
    assert_eq!(summaries[1].name, "bob");
    assert!(summaries[1].online);
    assert!(summaries[1].profile.is_none());
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn files_are_metadata_records_with_member_scoped_listing() {
    let (dir, store) = temp_store("files");
    let file = TeamFile {
        id: "f-1".to_owned(),
        name: "notes.txt".to_owned(),
        size: 12,
        sha256: "abc".to_owned(),
        sender: "alice".to_owned(),
        recipient: "bob".to_owned(),
        created_at: 100,
    };
    store.insert_file(&file).unwrap();
    let shared = TeamFile {
        id: "f-2".to_owned(),
        recipient: "group".to_owned(),
        created_at: 200,
        ..file.clone()
    };
    store.insert_file(&shared).unwrap();
    assert_eq!(store.get_file("f-1").unwrap().unwrap().name, "notes.txt");
    let listed = store.files_for_member("bob").unwrap();
    assert_eq!(listed.len(), 2); // direct + group share
    assert!(listed[0].created_at > listed[1].created_at); // newest first
    assert!(store.remove_file("f-1").unwrap());
    assert!(!store.remove_file("f-1").unwrap());
    assert_eq!(store.files_for_member("bob").unwrap().len(), 1);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}
