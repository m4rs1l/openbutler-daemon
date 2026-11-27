//! The team file store — fail-before pins for the team slice's T3a
//! (legacy `team/files.ts`, 116 lines, read 2026-11-22). Every transport
//! sentence and status is the source's `TeamTransportError`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::temp_data_dir;

use openbutler_daemon::team::files::{
    FileExpectation, FilesError, default_inbox_path, delete_file_bytes, file_exists, file_path,
    hash_file, open_file, outbound_file_path, store_file_bytes, write_verified_file,
};

fn expectation(data: &[u8], sha256: &str) -> FileExpectation {
    FileExpectation {
        size: data.len() as u64,
        sha256: sha256.to_owned(),
    }
}

/// sha256 of literal bytes, computed independently of the code under test.
fn sha(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

fn transport(error: FilesError) -> (String, bool, u16) {
    match error {
        FilesError::Transport(t) => (t.message, t.retryable, t.status),
        other => panic!("expected a transport error, got {other}"),
    }
}

/// Unique per call: temp_data_dir is deterministic per tag+pid, and this
/// binary runs every team_files test in one process — one shared tag
/// would let parallel tests wipe each other's files.
fn unique_dir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    temp_data_dir(&format!(
        "team-files-{}",
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

#[test]
fn file_paths_refuse_everything_that_is_not_a_file_id() {
    let dir = unique_dir();
    for id in [
        "",
        "abc",
        "f_123",
        "f_abcdefghi",
        "f_12345678x",
        "F_ABCDEF12",
        " f_12345678",
        "f_12345678/",
        "../../secrets",
    ] {
        let error = file_path(&dir, id).expect_err("refused");
        assert_eq!(
            error.to_string(),
            format!("\"{id}\" is not a valid file id."),
            "id: {id:?}"
        );
    }
}

#[test]
fn valid_file_ids_resolve_under_the_team_files_directory() {
    let dir = unique_dir();
    let short = file_path(&dir, "f_12345678").expect("short id");
    assert_eq!(short, dir.join("team-files").join("f_12345678"),);
    // The pattern is `f_[0-9a-f]{8,}` — unbounded above, lowercase only.
    let long_id = format!("f_{}", "deadbeef".repeat(8));
    let long = file_path(&dir, &long_id).expect("long id");
    assert_eq!(long, dir.join("team-files").join(&long_id));
    assert_eq!(
        outbound_file_path(&dir, "f_abcdef12").expect("outbound id"),
        dir.join("team-outbound-files").join("f_abcdef12")
    );
}

#[test]
fn a_verified_write_creates_parents_and_lands_byte_exact() {
    let dir = unique_dir();
    let payload = b"verified payload";
    let destination = dir.join("nested").join("deeper").join("out.bin");
    write_verified_file(
        &mut &payload[..],
        &destination,
        &expectation(payload, &sha(payload)),
    )
    .expect("verified write");
    assert_eq!(std::fs::read(&destination).expect("read back"), payload);
    assert_eq!(
        hash_file(&destination).expect("hash"),
        sha(payload),
        "hash_file streams sha256"
    );
    // The temporary file is gone — only the destination remains.
    let parent = destination.parent().unwrap();
    let entries: Vec<_> = std::fs::read_dir(parent)
        .expect("read dir")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert_eq!(entries.len(), 1);
}

#[test]
fn overshooting_the_declared_size_fails_with_the_source_sentence() {
    let dir = unique_dir();
    let destination = dir.join("big.bin");
    let data = b"0123456789";
    let error = write_verified_file(
        &mut &data[..],
        &destination,
        &FileExpectation {
            size: 4,
            sha256: sha(&data[..4]),
        },
    )
    .expect_err("overshoot refused");
    let (message, retryable, status) = transport(error);
    assert_eq!(message, "File exceeds the declared size.");
    assert!(!retryable);
    assert_eq!(status, 400);
    assert!(!destination.exists(), "no destination on failure");
}

#[test]
fn wrong_bytes_fail_the_size_or_hash_check() {
    let dir = unique_dir();
    let data = b"actual bytes";
    // Short source: size check.
    let short_error = write_verified_file(
        &mut &data[..],
        &dir.join("short.bin"),
        &FileExpectation {
            size: 999,
            sha256: sha(data),
        },
    )
    .expect_err("short source refused");
    assert_eq!(
        transport(short_error).0,
        "File bytes do not match the declared size or sha256."
    );
    // Exact size, wrong content: hash check.
    let hash_error = write_verified_file(
        &mut &data[..],
        &dir.join("hash.bin"),
        &FileExpectation {
            size: data.len() as u64,
            sha256: sha(b"other bytes!!!"),
        },
    )
    .expect_err("wrong hash refused");
    let (message, retryable, status) = transport(hash_error);
    assert_eq!(
        message,
        "File bytes do not match the declared size or sha256."
    );
    assert!(!retryable);
    assert_eq!(status, 400);
    assert!(!dir.join("hash.bin").exists());
}

#[test]
fn storing_is_content_addressed_and_idempotent() {
    let dir = unique_dir();
    let payload = b"same bytes both times";
    store_file_bytes(
        &mut &payload[..],
        &dir,
        "f_12345678",
        &expectation(payload, &sha(payload)),
    )
    .expect("first store");
    // Same id, same bytes: a second store is a no-op success, not a 409.
    store_file_bytes(
        &mut &payload[..],
        &dir,
        "f_12345678",
        &expectation(payload, &sha(payload)),
    )
    .expect("idempotent store");
    assert_eq!(
        std::fs::read(file_path(&dir, "f_12345678").expect("path")).expect("read"),
        payload
    );
    // The staging upload is cleaned up — only the committed id remains.
    let entries: Vec<_> = std::fs::read_dir(dir.join("team-files"))
        .expect("read dir")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert_eq!(entries.len(), 1);
}

#[test]
fn storing_different_bytes_under_a_used_id_conflicts_with_the_source_sentence() {
    let dir = unique_dir();
    let first = b"first payload";
    store_file_bytes(
        &mut &first[..],
        &dir,
        "f_abcdef12",
        &expectation(first, &sha(first)),
    )
    .expect("first store");
    let second = b"second payload!";
    let error = store_file_bytes(
        &mut &second[..],
        &dir,
        "f_abcdef12",
        &expectation(second, &sha(second)),
    )
    .expect_err("conflict");
    let (message, retryable, status) = transport(error);
    assert_eq!(message, "Transfer id already has different bytes.");
    assert!(!retryable);
    assert_eq!(status, 409);
    // The committed bytes are untouched.
    assert_eq!(
        std::fs::read(file_path(&dir, "f_abcdef12").expect("path")).expect("read"),
        first
    );
}

#[test]
fn existence_and_deletion_follow_the_source_error_split() {
    let dir = unique_dir();
    let payload = b"exists";
    store_file_bytes(
        &mut &payload[..],
        &dir,
        "f_aaaabbbb",
        &expectation(payload, &sha(payload)),
    )
    .expect("store");
    assert!(file_exists(&dir, "f_aaaabbbb"));
    assert!(!file_exists(&dir, "f_ccccdddd"), "missing id is false");
    assert!(!file_exists(&dir, "not-an-id"), "invalid id is false too");

    // Opening an existing id gives a handle; a missing id is an io error.
    let handle = open_file(&dir, "f_aaaabbbb").expect("open");
    assert_eq!(handle.metadata().expect("len").len(), payload.len() as u64);

    // delete: missing file is a silent success (force), invalid id throws.
    delete_file_bytes(&dir, "f_ccccdddd").expect("missing delete is fine");
    let error = delete_file_bytes(&dir, "not-an-id").expect_err("invalid id");
    assert_eq!(error.to_string(), "\"not-an-id\" is not a valid file id.");
    delete_file_bytes(&dir, "f_aaaabbbb").expect("stored delete");
    assert!(!file_exists(&dir, "f_aaaabbbb"));
}

#[test]
fn the_default_inbox_lands_by_basename_under_the_team_folder() {
    let dir = unique_dir();
    assert_eq!(
        default_inbox_path(&dir, "team", "report.pdf"),
        dir.join("team-inbox").join("team").join("report.pdf")
    );
    assert_eq!(
        default_inbox_path(&dir, "team", "sub/dir/report.pdf"),
        dir.join("team-inbox").join("team").join("report.pdf")
    );
    // `\` separates on Windows and is an ordinary character elsewhere, so
    // the basename follows the platform rule (`path.basename`).
    #[cfg(windows)]
    assert_eq!(
        default_inbox_path(&dir, "team", "C:\\tmp\\report.pdf"),
        dir.join("team-inbox").join("team").join("report.pdf")
    );
    #[cfg(unix)]
    assert_eq!(
        default_inbox_path(&dir, "team", "C:\\tmp\\report.pdf"),
        dir.join("team-inbox")
            .join("team")
            .join("C:\\tmp\\report.pdf")
    );
    // resolve() semantics: "." stays inside the team folder, ".." climbs
    // out of it, an empty name lands on the team folder itself.
    assert_eq!(
        default_inbox_path(&dir, "team", "."),
        dir.join("team-inbox").join("team")
    );
    assert_eq!(
        default_inbox_path(&dir, "team", ".."),
        dir.join("team-inbox")
    );
    assert_eq!(
        default_inbox_path(&dir, "team", ""),
        dir.join("team-inbox").join("team")
    );
    // The team key climbs too — resolve() is lexical across all segments.
    assert_eq!(
        default_inbox_path(&dir, "..", "report.pdf"),
        dir.join("report.pdf")
    );
}
