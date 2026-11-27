#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Account and session stores, plus the sign-in lock: what `login` stands on.
//!
//! Fail-before: every rule here (salted hash and no plaintext on disk, the
//! username as part of the credential, a broken store admitting nobody, the
//! password floor, the device cap, expiry, revocation, the five-failure lock)
//! is pinned against a scratch data directory.

use std::path::PathBuf;

use openbutler_daemon::auth::sessions::SESSION_TTL_MS;
use openbutler_daemon::auth::throttle::{MAX_SIGN_IN_FAILURES, SIGN_IN_LOCK_MS};
use openbutler_daemon::auth::{AdminAccount, AuthError, SessionStore, SignInGate};

fn scratch(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("ob-account-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

// --- Account -----------------------------------------------------------------

#[test]
fn an_account_is_created_with_a_hash_and_no_plaintext() {
    let dir = scratch("create");
    let account = AdminAccount::new(&dir);

    assert!(!account.is_configured(), "a fresh directory has no account");
    let summary = account
        .create("owner", "correct horse battery")
        .expect("create");
    assert_eq!(summary.username, "owner");
    assert!(summary.created_at.ends_with('Z'), "the stamp is ISO");
    assert!(account.is_configured(), "the account is usable");

    let raw = std::fs::read_to_string(dir.join("admin.json")).expect("account file");
    assert!(
        raw.contains("$argon2id$"),
        "the record keeps a PHC string, not a bare hash"
    );
    assert!(
        !raw.contains("correct horse battery"),
        "the plaintext password must never be written to disk"
    );

    assert!(
        account.verify("owner", "correct horse battery").unwrap(),
        "the right password verifies"
    );
    assert!(
        !account.verify("owner", "wrong").unwrap(),
        "a wrong password does not"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_username_is_part_of_the_credential() {
    let dir = scratch("username");
    let account = AdminAccount::new(&dir);
    account
        .create("owner", "correct horse battery")
        .expect("create");

    assert!(
        !account
            .verify("someone-else", "correct horse battery")
            .unwrap(),
        "the right password under another username is not the account"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_second_create_is_refused_and_a_broken_store_stops_everything() {
    let dir = scratch("twice");
    let account = AdminAccount::new(&dir);
    account
        .create("owner", "correct horse battery")
        .expect("create");
    let error = account
        .create("second", "another password")
        .expect_err("a second account must be refused");
    assert!(
        error.to_string().contains("already exists"),
        "the refusal says why: {error}"
    );

    std::fs::write(dir.join("admin.json"), "{ not json").expect("break the store");
    assert!(!account.is_configured(), "a broken store is not an account");
    assert!(
        account.verify("owner", "correct horse battery").is_err(),
        "a broken store verifies as an error, never as an admit"
    );
    let error = account
        .create("owner", "correct horse battery")
        .expect_err("a broken store must not be silently replaced");
    assert!(
        matches!(error, AuthError::AccountNotJson { .. }),
        "the refusal names the file: {error}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_short_password_is_refused() {
    let dir = scratch("short");
    let account = AdminAccount::new(&dir);
    let error = account
        .create("owner", "short")
        .expect_err("a short password must be refused");
    assert_eq!(
        error.to_string(),
        "The password needs at least 8 characters."
    );
    assert!(!account.is_configured(), "nothing was written");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn changing_the_password_requires_the_current_one() {
    let dir = scratch("passwd");
    let account = AdminAccount::new(&dir);
    account
        .create("owner", "correct horse battery")
        .expect("create");

    let error = account
        .change_password("not it", "a new long password")
        .expect_err("the current password is required");
    assert_eq!(error.to_string(), "That is not the current admin password.");

    account
        .change_password("correct horse battery", "a new long password")
        .expect("change");
    assert!(
        !account.verify("owner", "correct horse battery").unwrap(),
        "the old password stops working"
    );
    assert!(
        account.verify("owner", "a new long password").unwrap(),
        "the new one works"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// --- Sessions ----------------------------------------------------------------

#[test]
fn a_session_mints_verifies_and_revokes() {
    let dir = scratch("session");
    let store = SessionStore::new(&dir);
    let now = 1_700_000_000_000;

    let issued = store.mint("laptop", now).expect("mint");
    assert!(
        issued.token.starts_with("obt_"),
        "the token keeps its prefix"
    );
    assert_eq!(issued.expires_at_ms, now + SESSION_TTL_MS);
    assert_eq!(
        store.verify(&issued.token, now).unwrap().as_deref(),
        Some(issued.id.as_str()),
        "the token verifies and names its session"
    );
    assert_eq!(
        store.verify("obt_wrong", now).unwrap(),
        None,
        "a foreign token verifies as nothing"
    );

    let raw = std::fs::read_to_string(dir.join("sessions.json")).expect("store");
    assert!(
        !raw.contains(&issued.token),
        "the plaintext token must never be written to disk"
    );

    assert_eq!(store.list(now).unwrap().len(), 1);
    assert!(store.revoke(&issued.id, now).unwrap(), "revoke finds it");
    assert_eq!(
        store.verify(&issued.token, now).unwrap(),
        None,
        "a revoked session stops verifying"
    );
    assert!(
        !store.revoke(&issued.id, now).unwrap(),
        "a second revoke finds nothing"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_expired_session_stops_verifying() {
    let dir = scratch("expiry");
    let store = SessionStore::new(&dir);
    let now = 1_700_000_000_000;

    let issued = store.mint("phone", now).expect("mint");
    let later = now + SESSION_TTL_MS;
    assert_eq!(
        store.verify(&issued.token, later).unwrap(),
        None,
        "a session is refused at its expiry instant"
    );
    assert!(
        store.list(later).unwrap().is_empty(),
        "an expired session is not listed"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_session_store_caps_the_account() {
    let dir = scratch("cap");
    let store = SessionStore::new(&dir);
    let now = 1_700_000_000_000;
    for index in 0..10 {
        store.mint(&format!("device-{index}"), now).expect("mint");
    }
    let error = store
        .mint("one-too-many", now)
        .expect_err("the eleventh device must be refused");
    assert!(
        error.to_string().contains("already has 10 devices"),
        "the refusal names the ceiling: {error}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// --- The lock ----------------------------------------------------------------

#[test]
fn the_lock_arms_after_five_failures_and_clears_on_success() {
    let gate = SignInGate::new();
    let now = 1_700_000_000_000;

    assert!(!gate.is_locked(now), "a fresh gate is open");
    for _ in 0..MAX_SIGN_IN_FAILURES {
        gate.record_failure(now);
    }
    assert!(gate.is_locked(now), "five failures shut the door");
    assert!(
        gate.is_locked(now + SIGN_IN_LOCK_MS - 1),
        "the door stays shut for the window"
    );
    assert!(
        !gate.is_locked(now + SIGN_IN_LOCK_MS),
        "the door reopens when the window ends"
    );

    gate.record_success();
    assert!(!gate.is_locked(now), "a success clears the armed lock");
    for _ in 0..(MAX_SIGN_IN_FAILURES - 1) {
        gate.record_failure(now);
    }
    assert!(!gate.is_locked(now), "four fresh failures do not lock");
    gate.record_failure(now);
    assert!(gate.is_locked(now), "the fifth does");
}
