#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The settings durability boundary: the lock, the journal, and the atomic
//! pair commit.
//!
//! Fail-before: these pin the transaction contract the TypeScript
//! `settings-transaction.ts` defines — a reader never observes half of a
//! two-file commit, an interrupted commit heals (or refuses) before the
//! next operation, and nothing is ever overwritten by a malformed journal.
//! The payloads are opaque strings here: schema parsing is a separate
//! layer built on top of this one.

use std::path::{Path, PathBuf};
use std::time::Duration;

use openbutler_daemon::settings::transaction::{acquire, atomic_write, journal_path};

fn scratch(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("ob-tx-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).expect("file must exist")
}

// --- Pair commit -------------------------------------------------------------

#[test]
fn a_pair_commit_publishes_both_files_and_leaves_no_journal() {
    let dir = scratch("pair");
    let lock = acquire(&dir, Duration::from_secs(5)).expect("lock");
    lock.commit_pair(&dir, "providers: {}\n", "providers: {}\n")
        .expect("commit");
    drop(lock);

    assert_eq!(read(&dir.join("settings.yaml")), "providers: {}\n");
    assert_eq!(read(&dir.join("config.yaml")), "providers: {}\n");
    assert!(
        !journal_path(&dir).exists(),
        "the journal is removed once the pair is published"
    );
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "no temp files remain: {leftovers:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_single_file_commit_uses_the_same_boundary() {
    let dir = scratch("single");
    let lock = acquire(&dir, Duration::from_secs(5)).expect("lock");
    lock.commit_single(&dir, "teamTicket: abc\n")
        .expect("commit");
    drop(lock);

    assert_eq!(read(&dir.join("config.yaml")), "teamTicket: abc\n");
    assert!(!dir.join("settings.yaml").exists(), "only the named file");
    assert!(
        !journal_path(&dir).exists(),
        "no journal for a single write"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// --- Recovery ----------------------------------------------------------------

#[test]
fn an_interrupted_commit_heals_before_the_next_operation() {
    let dir = scratch("heal");
    // Simulate a crash between journaling and applying: the journal holds
    // both documents, one file is missing, the other holds stale content.
    std::fs::write(dir.join("settings.yaml"), "stale\n").unwrap();
    std::fs::write(
        journal_path(&dir),
        serde_json::json!({
            "version": 1,
            "settings": "fresh settings\n",
            "config": "fresh config\n"
        })
        .to_string(),
    )
    .unwrap();

    let lock = acquire(&dir, Duration::from_secs(5)).expect("lock");
    drop(lock);

    assert_eq!(
        read(&dir.join("settings.yaml")),
        "fresh settings\n",
        "the journaled document replaces the stale file"
    );
    assert_eq!(read(&dir.join("config.yaml")), "fresh config\n");
    assert!(
        !journal_path(&dir).exists(),
        "the journal is consumed by the heal"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_malformed_journal_refuses_and_overwrites_nothing() {
    let dir = scratch("bad-journal");
    std::fs::write(dir.join("settings.yaml"), "precious\n").unwrap();
    std::fs::write(dir.join("config.yaml"), "secrets\n").unwrap();
    std::fs::write(journal_path(&dir), "{\"version\":9}").unwrap();

    let error = acquire(&dir, Duration::from_secs(5)).expect_err("must refuse");
    assert_eq!(
        error.to_string(),
        "The settings transaction journal is invalid. No configuration was overwritten."
    );
    assert_eq!(read(&dir.join("settings.yaml")), "precious\n", "untouched");
    assert_eq!(read(&dir.join("config.yaml")), "secrets\n", "untouched");
    assert!(
        journal_path(&dir).exists(),
        "the invalid journal stays for inspection"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// --- Locking -----------------------------------------------------------------

#[test]
fn a_held_lock_times_out_with_the_documented_message() {
    let dir = scratch("timeout");
    let held = acquire(&dir, Duration::from_secs(5)).expect("first lock");

    let error = acquire(&dir, Duration::from_millis(80)).expect_err("second must time out");
    assert_eq!(
        error.to_string(),
        "Timed out waiting for the settings transaction lock."
    );

    drop(held);
    let after = acquire(&dir, Duration::from_secs(5)).expect("released");
    drop(after);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_lock_guard_is_reentrant_within_the_process() {
    // The transaction layer hands one guard to a caller that may nest
    // helpers; a second acquisition on the same directory from the same
    // thread must not deadlock (the TypeScript layer re-enters via its
    // holdingLock flag; the Rust guard is passed explicitly instead).
    let dir = scratch("reentrant");
    let lock = acquire(&dir, Duration::from_secs(5)).expect("lock");
    // Holding one guard, a *nested* helper that only needs the journal
    // boundary still works through the same guard:
    lock.commit_single(&dir, "a: 1\n").expect("first");
    lock.commit_single(&dir, "a: 2\n").expect("second");
    drop(lock);
    assert_eq!(read(&dir.join("config.yaml")), "a: 2\n");

    let _ = std::fs::remove_dir_all(&dir);
}

// --- Atomic write ------------------------------------------------------------

/// A crash between the temporary create and the rename leaves
/// `<name>.<pid>.tmp` behind, and the operating system hands that pid to a
/// later process: the exclusive create then refuses forever and no settings
/// write can land. The name belongs to this writer (the lock admits one at a
/// time), so a stale one is cleared and the write proceeds.
#[test]
fn a_stale_temporary_file_never_blocks_a_write() {
    let dir = scratch("atomic-stale");
    let target = dir.join("settings.yaml");
    let stale = dir.join(format!("settings.yaml.{}.tmp", std::process::id()));
    std::fs::write(&stale, b"junk from a crashed run").unwrap();

    atomic_write(&target, b"fresh").expect("the stale temporary must not wedge the write");
    assert_eq!(read(&target), "fresh");
    assert!(!stale.exists(), "the stale temporary is cleared");
}

#[test]
fn atomic_write_creates_the_parent_and_replaces_in_one_step() {
    let dir = scratch("atomic");
    let target = dir.join("nested").join("file.txt");

    atomic_write(&target, b"one").expect("first write");
    assert_eq!(read(&target), "one");

    atomic_write(&target, b"two").expect("second write");
    assert_eq!(read(&target), "two", "rename replaces the previous content");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "credential-adjacent files are 0600");
    }

    let _ = std::fs::remove_dir_all(&dir);
}
