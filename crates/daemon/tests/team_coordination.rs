//! `TeamCoordination` — fail-before pins for the team slice's T3a
//! (legacy `team/coordination.ts`, 254 lines, read 2026-11-22). The local
//! and HTTP channels share this one rule owner, so every refusal sentence,
//! status, revision guard and the wake message body are the source's.
//!
//! Documented JS-coercion edges: `limit`/`offset` go through `Number()`
//! (strings coerce), while `expectedRevision` and `toolCount` use
//! `Number.isSafeInteger(value)` directly (no coercion — `"1"` is rejected).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::temp_data_dir;

use openbutler_daemon::team::coordination::{
    PeerProfile, TeamCoordination, WorkOutcome, parse_capabilities,
};
use openbutler_daemon::team::operations::TeamError;
use openbutler_daemon::team::store::TeamStore;
use serde_json::json;

fn fixture() -> (std::path::PathBuf, TeamStore) {
    // temp_data_dir is deterministic per tag+pid — every test in this
    // binary shares one process, so each fixture needs its own tag or
    // parallel tests wipe each other's team.db.
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let unique = format!("team-coord-{}", COUNTER.fetch_add(1, Ordering::Relaxed));
    let dir = temp_data_dir(&unique);
    let store = TeamStore::open(&dir.join("team.db")).expect("store opens");
    store.init_hub("h", "t", "Lab", "Hub", 1).unwrap();
    add_member(&store, "Alice");
    add_member(&store, "Bob");
    (dir, store)
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

fn refused(error: TeamError) -> (String, u16) {
    (error.message, error.status)
}

// ---------------------------------------------------------------- profiles

#[test]
fn profile_update_validates_text_and_revision_with_the_source_sentences() {
    let (_dir, store) = fixture();
    let coord = TeamCoordination::new(&store);

    let empty = coord
        .profile(
            "Alice",
            &json!({"action": "update", "description": "  ", "expectedRevision": 0}),
        )
        .expect_err("empty description");
    assert_eq!(
        refused(empty),
        (
            "description must be non-empty text of at most 4000 characters.".to_owned(),
            400
        )
    );

    let non_string = coord
        .profile(
            "Alice",
            &json!({"action": "update", "description": 42, "expectedRevision": 0}),
        )
        .expect_err("non-string description");
    assert_eq!(
        refused(non_string),
        (
            "description must be non-empty text of at most 4000 characters.".to_owned(),
            400
        )
    );
}

#[test]
fn profile_text_budget_counts_utf16_units() {
    let (_dir, store) = fixture();
    let coord = TeamCoordination::new(&store);
    // 4000 astral characters are 8000 UTF-16 units — the source's
    // `description.length` refuses these even though chars() would not.
    let description = "\u{1F600}".repeat(4000);
    let error = coord
        .profile(
            "Alice",
            &json!({"action": "update", "description": description, "expectedRevision": 0}),
        )
        .expect_err("surrogate pairs count double");
    assert_eq!(
        refused(error),
        (
            "description must be non-empty text of at most 4000 characters.".to_owned(),
            400
        )
    );
}

#[test]
fn profile_expected_revision_requires_an_actual_safe_integer() {
    let (_dir, store) = fixture();
    let coord = TeamCoordination::new(&store);
    let sentence = "expectedRevision must be a non-negative integer.";
    for value in [json!(null), json!(-1), json!("1"), json!(1.5), json!(true)] {
        let error = coord
            .profile(
                "Alice",
                &json!({"action": "update", "description": "ok", "expectedRevision": value}),
            )
            .expect_err("refused");
        assert_eq!(refused(error), (sentence.to_owned(), 400));
    }
    // A JSON `1.0` parses as a number and `Number.isSafeInteger(1.0)` is
    // true — accepted, and it creates the profile (revision 1).
    let created = coord
        .profile(
            "Alice",
            &json!({"action": "update", "description": "first", "expectedRevision": 0.0}),
        )
        .expect("integer-valued float accepted");
    assert_eq!(created.expect("created").revision, 1);
}

#[test]
fn profile_reads_unknown_actors_out_and_writes_own_profile_only() {
    let (_dir, store) = fixture();
    let coord = TeamCoordination::new(&store);

    let unknown = coord
        .profile("Ghost", &json!({"action": "read"}))
        .expect_err("unknown member");
    assert_eq!(
        refused(unknown),
        ("No such active team member.".to_owned(), 404)
    );

    // No profile yet — reads return null, they do not invent one.
    assert_eq!(
        coord.profile("Alice", &json!({"action": "read"})).unwrap(),
        None
    );

    let foreign = coord
        .profile(
            "Alice",
            &json!({"action": "update", "member": "Bob", "description": "mine", "expectedRevision": 0}),
        )
        .expect_err("cross-member update");
    assert_eq!(
        refused(foreign),
        ("You may update only your own profile.".to_owned(), 403)
    );

    let bad_action = coord
        .profile("Alice", &json!({"action": "replace"}))
        .expect_err("unknown action");
    assert_eq!(
        refused(bad_action),
        ("Profile action must be read or update.".to_owned(), 400)
    );
}

#[test]
fn profile_update_bumps_once_and_treats_a_retry_as_idempotent() {
    let (_dir, store) = fixture();
    let coord = TeamCoordination::new(&store);

    let first = coord
        .profile(
            "Alice",
            &json!({"action": "update", "description": "first", "expectedRevision": 0}),
        )
        .unwrap()
        .expect("created");
    assert_eq!(
        first,
        PeerProfile {
            description: "first".to_owned(),
            revision: 1,
            updated_at: first.updated_at,
        }
    );

    // The same description at the old expected revision is a retry: the
    // current profile comes back untouched (revision stays 1).
    let retry = coord
        .profile(
            "Alice",
            &json!({"action": "update", "description": "first", "expectedRevision": 0}),
        )
        .unwrap()
        .expect("still there");
    assert_eq!(retry.revision, 1);
    assert_eq!(retry.description, "first");

    // A changed description at the old revision is the conflict sentence.
    let stale = coord
        .profile(
            "Alice",
            &json!({"action": "update", "description": "second", "expectedRevision": 0}),
        )
        .expect_err("stale write");
    assert_eq!(
        refused(stale),
        (
            "Profile changed. Read the latest profile before updating.".to_owned(),
            409
        )
    );

    // The stored read agrees with the returned profile.
    let stored = coord.profile("Alice", &json!({"action": "read"})).unwrap();
    assert_eq!(stored.expect("profile is persisted").revision, 1);
}

// ------------------------------------------------------------ capabilities

#[test]
fn capability_reports_validate_in_the_source_order() {
    let sentence = "Invalid host capability report.";
    let base = || {
        json!({
            "platform": "win32",
            "mcpServers": [],
            "mcpTools": [],
            "custodianBusy": false,
        })
    };
    for mutate in [
        Box::new(|mut v: serde_json::Value| {
            v["custodianBusy"] = json!("yes");
            v
        }) as Box<dyn Fn(serde_json::Value) -> serde_json::Value>,
        Box::new(|mut v: serde_json::Value| {
            v["mcpServers"] = json!(vec![json!({"name": "x", "toolCount": 1}); 101]);
            v
        }),
        Box::new(|mut v: serde_json::Value| {
            v["mcpTools"] = json!(vec![json!("x"); 1001]);
            v
        }),
        Box::new(|mut v: serde_json::Value| {
            v["custodianBusy"] = json!(0);
            v
        }),
    ] {
        let error = parse_capabilities(&mutate(base()), 7).expect_err("refused");
        assert_eq!(refused(error), (sentence.to_owned(), 400));
    }

    // Platform goes through text() before the group check: an empty
    // platform wins even while the group body is already invalid.
    let platform = parse_capabilities(
        &json!({
            "platform": "",
            "mcpServers": [],
            "mcpTools": [],
            "custodianBusy": 0,
        }),
        7,
    )
    .expect_err("empty platform");
    assert_eq!(
        refused(platform),
        (
            "platform must be non-empty text of at most 32 characters.".to_owned(),
            400
        )
    );
}

#[test]
fn capability_rows_validate_name_then_count_without_string_coercion() {
    // null row: not an object.
    let null_row = parse_capabilities(
        &json!({
            "platform": "win32",
            "mcpServers": [null],
            "mcpTools": [],
            "custodianBusy": false,
        }),
        7,
    )
    .expect_err("null row");
    assert_eq!(
        refused(null_row),
        ("Invalid MCP server report.".to_owned(), 400)
    );

    // Missing name: text() on undefined — before the count check.
    let no_name = parse_capabilities(
        &json!({
            "platform": "win32",
            "mcpServers": [{"toolCount": 1}],
            "mcpTools": [],
            "custodianBusy": false,
        }),
        7,
    )
    .expect_err("missing name");
    assert_eq!(
        refused(no_name),
        (
            "server name must be non-empty text of at most 128 characters.".to_owned(),
            400
        )
    );

    // Count bounds and the no-coercion rule for toolCount.
    for count in [json!(-1), json!(10001), json!("5")] {
        let error = parse_capabilities(
            &json!({
                "platform": "win32",
                "mcpServers": [{"name": "srv", "toolCount": count}],
                "mcpTools": [],
                "custodianBusy": false,
            }),
            7,
        )
        .expect_err("bad count");
        assert_eq!(refused(error), ("Invalid tool count.".to_owned(), 400));
    }

    // Tool names carry their own budget.
    let tool = parse_capabilities(
        &json!({
            "platform": "win32",
            "mcpServers": [],
            "mcpTools": [""],
            "custodianBusy": false,
        }),
        7,
    )
    .expect_err("empty tool");
    assert_eq!(
        refused(tool),
        (
            "tool name must be non-empty text of at most 256 characters.".to_owned(),
            400
        )
    );
}

#[test]
fn a_valid_capability_report_stamps_the_injected_observation_time() {
    let report = parse_capabilities(
        &json!({
            "platform": "linux",
            "mcpServers": [{"name": "srv", "toolCount": 3}],
            "mcpTools": ["srv_tool"],
            "custodianBusy": true,
        }),
        1_700_000_000_123,
    )
    .expect("valid");
    assert_eq!(report.observed_at, 1_700_000_000_123);
    assert_eq!(report.platform, "linux");
    assert!(report.custodian_busy);
    assert_eq!(report.mcp_servers[0].name, "srv");
    assert_eq!(report.mcp_servers[0].tool_count, 3);
    assert_eq!(report.mcp_tools, vec!["srv_tool"]);
}

#[test]
fn reporting_capabilities_checks_the_actor_before_parsing_the_body() {
    let (_dir, store) = fixture();
    let coord = TeamCoordination::new(&store);
    // Argument order in the source: member(actor) resolves first, so an
    // unknown actor wins over an invalid body.
    let error = coord
        .report_capabilities("Ghost", &json!({"platform": ""}))
        .expect_err("unknown member");
    assert_eq!(
        refused(error),
        ("No such active team member.".to_owned(), 404)
    );

    coord
        .report_capabilities(
            "Alice",
            &json!({
                "platform": "win32",
                "mcpServers": [],
                "mcpTools": [],
                "custodianBusy": false,
            }),
        )
        .expect("stored");
    let stored = store.peer_capabilities("Alice").unwrap().expect("row");
    assert_eq!(stored.platform, "win32");
    // Observed at wall time now, not a caller-supplied timestamp.
    assert!(stored.observed_at >= 1_700_000_000_000);
}

// -------------------------------------------------------------------- work

#[test]
fn work_list_validates_paging_before_touching_the_store() {
    let (_dir, store) = fixture();
    let coord = TeamCoordination::new(&store);
    coord
        .work("Alice", &json!({"action": "list"}))
        .expect("default list is empty");

    for (input, sentence) in [
        (json!({"action": "list", "limit": 0}), "limit must be 1-50."),
        (
            json!({"action": "list", "limit": 51}),
            "limit must be 1-50.",
        ),
        (
            json!({"action": "list", "limit": 10.5}),
            "limit must be 1-50.",
        ),
        (
            json!({"action": "list", "limit": "ten"}),
            "limit must be 1-50.",
        ),
        (
            json!({"action": "list", "offset": -1}),
            "offset must be 0-10000.",
        ),
        (
            json!({"action": "list", "offset": 10001}),
            "offset must be 0-10000.",
        ),
        (
            json!({"action": "list", "includeClosed": "true"}),
            "includeClosed must be boolean.",
        ),
    ] {
        let error = coord.work("Alice", &input).expect_err("refused");
        assert_eq!(refused(error), (sentence.to_owned(), 400));
    }

    // `Number("10")` is 10 — strings coerce for paging, unlike revisions.
    coord
        .work(
            "Alice",
            &json!({"action": "list", "limit": "10", "offset": "0"}),
        )
        .expect("coerced paging");

    let unknown = coord
        .work("Ghost", &json!({"action": "list"}))
        .expect_err("unknown member");
    assert_eq!(
        refused(unknown),
        ("No such active team member.".to_owned(), 404)
    );
}

#[test]
fn work_ids_are_text_bounded_then_pattern_checked() {
    let (_dir, store) = fixture();
    let coord = TeamCoordination::new(&store);

    let missing = coord
        .work("Alice", &json!({"action": "read"}))
        .expect_err("no id");
    assert_eq!(
        refused(missing),
        (
            "id must be non-empty text of at most 128 characters.".to_owned(),
            400
        )
    );

    let pattern = coord
        .work("Alice", &json!({"action": "read", "id": "not an id"}))
        .expect_err("spaces");
    assert_eq!(
        refused(pattern),
        (
            "id may contain only letters, digits, dashes and underscores.".to_owned(),
            400
        )
    );
}

#[test]
fn proposing_a_peer_work_persists_the_agreement_and_its_wake_message() {
    let (_dir, store) = fixture();
    let coord = TeamCoordination::new(&store);

    let own = coord
        .work(
            "Alice",
            &json!({"action": "propose", "id": "w1", "peer": "Alice", "terms": "task"}),
        )
        .expect_err("self proposal");
    assert_eq!(
        refused(own),
        (
            "A peer proposal must address another member.".to_owned(),
            400
        )
    );

    let ghost = coord
        .work(
            "Alice",
            &json!({"action": "propose", "id": "w1", "peer": "Ghost", "terms": "task"}),
        )
        .expect_err("unknown peer");
    assert_eq!(
        refused(ghost),
        ("No such active team member.".to_owned(), 404)
    );

    let created = coord
        .work(
            "Alice",
            &json!({"action": "propose", "id": "w1", "peer": "Bob", "terms": "ship it"}),
        )
        .expect("proposed");
    let WorkOutcome::One(created) = created else {
        panic!("propose returns one work");
    };
    assert_eq!(created.id, "w1");
    assert_eq!(created.requester, "Alice");
    assert_eq!(created.assignee, "Bob");
    assert_eq!(created.revision, 1);
    assert_eq!(created.terms, "ship it");
    assert_eq!(created.terms_author.as_deref(), Some("Alice"));
    assert_eq!(created.last_action, "propose");
    assert_eq!(created.last_note, "ship it");
    assert_eq!(created.result, None);

    // The save posted its wake message with the source's exact body.
    let notice = store
        .get_message("work_w1_1")
        .expect("lookup")
        .expect("notice exists");
    assert_eq!(notice.sender, "Alice");
    assert_eq!(notice.recipient, "Bob");
    assert_eq!(
        notice.body,
        "Peer work w1, revision 1: propose (proposed).\nship it\nRead the current agreement with team_work before acting. A work status is not owner authorization."
    );

    // The same proposal at the same id is idempotent; different terms conflict.
    let again = coord
        .work(
            "Alice",
            &json!({"action": "propose", "id": "w1", "peer": "Bob", "terms": "ship it"}),
        )
        .expect("idempotent");
    let WorkOutcome::One(again) = again else {
        panic!("propose returns one work");
    };
    assert_eq!(again.revision, 1);
    let conflict = coord
        .work(
            "Alice",
            &json!({"action": "propose", "id": "w1", "peer": "Bob", "terms": "other"}),
        )
        .expect_err("id in use");
    assert_eq!(
        refused(conflict),
        (
            "This work id is already in use. Use a new id for a different proposal.".to_owned(),
            409
        )
    );
}

#[test]
fn accessibility_decides_404_before_the_action_name_is_judged() {
    let (_dir, store) = fixture();
    let coord = TeamCoordination::new(&store);
    coord
        .work(
            "Alice",
            &json!({"action": "propose", "id": "w1", "peer": "Bob", "terms": "task"}),
        )
        .expect("proposed");

    // A stranger with a garbage action still gets the 404 — accessibility
    // is checked first in the source.
    let stranger = coord
        .work("Hub", &json!({"action": "bogus", "id": "w1"}))
        .expect_err("not a participant");
    assert_eq!(
        refused(stranger),
        ("No accessible peer work with this id.".to_owned(), 404)
    );

    // A participant with the garbage action gets the unknown-action 400.
    let unknown = coord
        .work("Bob", &json!({"action": "bogus", "id": "w1"}))
        .expect_err("unknown action");
    assert_eq!(
        refused(unknown),
        ("Unknown peer work action.".to_owned(), 400)
    );

    // read returns the row for either participant.
    let read = coord
        .work("Bob", &json!({"action": "read", "id": "w1"}))
        .expect("readable");
    let WorkOutcome::One(read) = read else {
        panic!("read returns one work");
    };
    assert_eq!(read.revision, 1);
}

#[test]
fn guards_let_each_side_play_only_its_part() {
    let (_dir, store) = fixture();
    let coord = TeamCoordination::new(&store);
    coord
        .work(
            "Alice",
            &json!({"action": "propose", "id": "w1", "peer": "Bob", "terms": "task"}),
        )
        .expect("proposed");

    // The requester cannot accept or counter its own fresh offer.
    let self_accept = coord
        .work(
            "Alice",
            &json!({"action": "accept", "id": "w1", "expectedRevision": 1, "note": "ok"}),
        )
        .expect_err("self answer");
    assert_eq!(
        refused(self_accept),
        (
            "Only the other participant may answer pending terms.".to_owned(),
            409
        )
    );
    let self_counter = coord
        .work(
            "Alice",
            &json!({
                "action": "counter",
                "id": "w1",
                "expectedRevision": 1,
                "note": "change",
                "terms": "task v2",
            }),
        )
        .expect_err("self counter");
    assert_eq!(
        refused(self_counter),
        (
            "Only the other participant may counter pending terms.".to_owned(),
            409
        )
    );

    // Bob accepts (revision 1 → 2).
    let accepted = coord
        .work(
            "Bob",
            &json!({"action": "accept", "id": "w1", "expectedRevision": 1, "note": "yes"}),
        )
        .expect("accepted");
    let WorkOutcome::One(accepted) = accepted else {
        panic!("one");
    };
    assert_eq!(accepted.revision, 2);
    assert_eq!(accepted.last_action, "accept");

    // The requester may not drive accepted work.
    let start_by_requester = coord
        .work(
            "Alice",
            &json!({"action": "start", "id": "w1", "expectedRevision": 2, "note": "go"}),
        )
        .expect_err("not the assignee");
    assert_eq!(
        refused(start_by_requester),
        (
            "Only the assignee may start accepted work or resume blocked work.".to_owned(),
            409
        )
    );

    // The assignee starts; resume is refused while working (it maps
    // blocked → working) — the same sentence from the other side.
    coord
        .work(
            "Bob",
            &json!({"action": "start", "id": "w1", "expectedRevision": 2, "note": "go"}),
        )
        .expect("started");
    let resume_working = coord
        .work(
            "Bob",
            &json!({"action": "resume", "id": "w1", "expectedRevision": 3, "note": "back"}),
        )
        .expect_err("resume means blocked");
    assert_eq!(
        refused(resume_working),
        (
            "Only the assignee may start accepted work or resume blocked work.".to_owned(),
            409
        )
    );

    // The requester cannot complete active work either — the
    // assignee-only sentence from the requester's side.
    let requester_complete = coord
        .work(
            "Alice",
            &json!({"action": "complete", "id": "w1", "expectedRevision": 3, "note": "done"}),
        )
        .expect_err("requester cannot complete");
    assert_eq!(
        refused(requester_complete),
        (
            "Only the assignee may report an accepted agreement.".to_owned(),
            409
        )
    );

    // The assignee blocks straight from working (rev 3 → 4).
    coord
        .work(
            "Bob",
            &json!({"action": "block", "id": "w1", "expectedRevision": 3, "note": "wait"}),
        )
        .expect("blocked");

    // The assignee cannot cancel; only the requester can. Blocked is
    // non-terminal here, so this fires the cancel guard, not the closed
    // one.
    let assignee_cancel = coord
        .work(
            "Bob",
            &json!({"action": "cancel", "id": "w1", "expectedRevision": 4, "note": "no"}),
        )
        .expect_err("assignee cannot cancel");
    assert_eq!(
        refused(assignee_cancel),
        (
            "Only the requester may cancel; report a failure if you cannot continue.".to_owned(),
            403
        )
    );

    // Blocked counts as active in the source's ACTIVE_WORK_STATES, so the
    // assignee may complete straight from blocked and the result is the
    // completion note. (Test-caught: an earlier expectation here had
    // complete limited to accepted/working — the source's array decides.)
    let from_blocked = coord
        .work(
            "Bob",
            &json!({"action": "complete", "id": "w1", "expectedRevision": 4, "note": "done"}),
        )
        .expect("blocked is active");
    let WorkOutcome::One(from_blocked) = from_blocked else {
        panic!("one");
    };
    assert_eq!(from_blocked.revision, 5);
    assert_eq!(from_blocked.state.as_str(), "completed");
    assert_eq!(from_blocked.result.as_deref(), Some("done"));

    // Terminal work refuses everything with the closed sentence.
    let after_close = coord
        .work(
            "Bob",
            &json!({"action": "start", "id": "w1", "expectedRevision": 5, "note": "again"}),
        )
        .expect_err("closed");
    assert_eq!(
        refused(after_close),
        (
            "This work is closed. Propose new work instead.".to_owned(),
            409
        )
    );

    // A requester cancel succeeds on its own fresh agreement (rev 1 → 2)
    // and marks the row cancelled.
    coord
        .work(
            "Alice",
            &json!({"action": "propose", "id": "w2", "peer": "Bob", "terms": "second"}),
        )
        .expect("proposed");
    let cancelled = coord
        .work(
            "Alice",
            &json!({"action": "cancel", "id": "w2", "expectedRevision": 1, "note": "no"}),
        )
        .expect("requester cancels");
    let WorkOutcome::One(cancelled) = cancelled else {
        panic!("one");
    };
    assert_eq!(cancelled.revision, 2);
    assert_eq!(cancelled.state.as_str(), "cancelled");
}

#[test]
fn stale_revisions_and_retries_follow_the_source_sequence() {
    let (_dir, store) = fixture();
    let coord = TeamCoordination::new(&store);
    coord
        .work(
            "Alice",
            &json!({"action": "propose", "id": "w1", "peer": "Bob", "terms": "task"}),
        )
        .expect("proposed");

    // Missing expectedRevision on a real action: the revision sentence.
    let missing = coord
        .work("Bob", &json!({"action": "read", "id": "w1"}))
        .expect("read works without a revision");
    assert!(matches!(missing, WorkOutcome::One(_)));
    let missing = coord
        .work("Bob", &json!({"action": "note", "id": "w1", "note": "hi"}))
        .expect_err("no revision");
    assert_eq!(
        refused(missing),
        (
            "expectedRevision must be a non-negative integer.".to_owned(),
            400
        )
    );

    // A stale expected revision is the read-again sentence.
    coord
        .work(
            "Bob",
            &json!({"action": "note", "id": "w1", "expectedRevision": 1, "note": "first"}),
        )
        .expect("note applied");
    let stale = coord
        .work(
            "Bob",
            &json!({"action": "note", "id": "w1", "expectedRevision": 1, "note": "second"}),
        )
        .expect_err("stale");
    assert_eq!(
        refused(stale),
        (
            "Work changed. Read its current state before acting.".to_owned(),
            409
        )
    );

    // Replaying the applied note at the old revision returns the current
    // row untouched — revision stays 2, no second message.
    let retry = coord
        .work(
            "Bob",
            &json!({"action": "note", "id": "w1", "expectedRevision": 1, "note": "first"}),
        )
        .expect("idempotent retry");
    let WorkOutcome::One(retry) = retry else {
        panic!("one");
    };
    assert_eq!(retry.revision, 2);
    assert!(store.get_message("work_w1_2").unwrap().is_some());
    assert!(store.get_message("work_w1_3").unwrap().is_none());

    // A note keeps the state and writes the wake message at the new revision.
    let noted = coord
        .work(
            "Alice",
            &json!({"action": "note", "id": "w1", "expectedRevision": 2, "note": "ping"}),
        )
        .expect("noted");
    let WorkOutcome::One(noted) = noted else {
        panic!("one");
    };
    assert_eq!(noted.state.as_str(), "proposed");
    assert_eq!(noted.revision, 3);
    let notice = store
        .get_message("work_w1_3")
        .expect("lookup")
        .expect("notice exists");
    assert_eq!(notice.recipient, "Bob");
    assert!(
        notice
            .body
            .starts_with("Peer work w1, revision 3: note (proposed).\nping\n")
    );
}

#[test]
fn the_list_returns_rows_for_the_actor_newest_first() {
    let (_dir, store) = fixture();
    let coord = TeamCoordination::new(&store);
    for id in ["w1", "w2"] {
        coord
            .work(
                "Alice",
                &json!({"action": "propose", "id": id, "peer": "Bob", "terms": id}),
            )
            .expect("proposed");
    }
    // The stranger sees nothing.
    let stranger = coord.work("Hub", &json!({"action": "list"})).expect("list");
    let WorkOutcome::List(rows) = stranger else {
        panic!("list");
    };
    assert!(rows.is_empty());

    let mine = coord.work("Bob", &json!({"action": "list"})).expect("list");
    let WorkOutcome::List(rows) = mine else {
        panic!("list");
    };
    assert_eq!(rows.len(), 2);
    // Closed rows stay out of the default list.
    coord
        .work(
            "Bob",
            &json!({"action": "accept", "id": "w1", "expectedRevision": 1, "note": "yes"}),
        )
        .expect("accepted");
    coord
        .work(
            "Bob",
            &json!({"action": "start", "id": "w1", "expectedRevision": 2, "note": "go"}),
        )
        .expect("started");
    coord
        .work(
            "Bob",
            &json!({"action": "complete", "id": "w1", "expectedRevision": 3, "note": "done"}),
        )
        .expect("completed");
    let open = coord.work("Bob", &json!({"action": "list"})).expect("list");
    let WorkOutcome::List(rows) = open else {
        panic!("list");
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "w2");
    let all = coord
        .work(
            "Bob",
            &json!({"action": "list", "includeClosed": true, "limit": 50}),
        )
        .expect("list");
    let WorkOutcome::List(rows) = all else {
        panic!("list");
    };
    assert_eq!(rows.len(), 2);
}
