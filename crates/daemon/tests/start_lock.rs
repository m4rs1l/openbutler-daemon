//! The entry start lock — source protocol pins (`daemon.ts`
//! `acquireDaemonStartLock` / `removeStaleDaemonStartLock` /
//! `releaseDaemonStartLock`): create-with-record, contention deadline,
//! stale reclaim, token-checked release, and the real "already running"
//! answer from a second entry against a live host.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::path::PathBuf;
use std::time::Duration;

use openbutler_daemon::start_lock::{StartLockOptions, acquire_with};

fn temp_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "ob-daemon-lock-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn options(wait_ms: u64, stale: Duration) -> StartLockOptions {
    StartLockOptions {
        wait: Duration::from_millis(wait_ms),
        stale,
    }
}

#[tokio::test]
async fn the_lock_records_its_owner_and_releases_on_drop() {
    let dir = temp_dir("create");
    let lock = acquire_with(&dir, options(500, Duration::from_secs(45)))
        .await
        .expect("the first attempt wins");
    let path = dir.join("daemon.start.lock");
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(record["pid"], std::process::id());
    assert!(record["startedAt"].is_string());
    assert!(record["token"].as_str().unwrap().len() >= 32);

    drop(lock);
    assert!(!path.exists(), "drop releases the lock");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_fresh_lock_times_out_with_the_source_sentence() {
    let dir = temp_dir("deadline");
    let _held = acquire_with(&dir, options(500, Duration::from_secs(45)))
        .await
        .expect("the first attempt wins");
    let error = acquire_with(&dir, options(300, Duration::from_secs(45)))
        .await
        .expect_err("a fresh lock is not reclaimable");
    assert_eq!(
        error,
        "Another OpenButler host is still starting. Retry after its startup finishes."
    );
    drop(_held);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_stale_lock_is_reclaimed_and_the_old_owner_cannot_unlink_the_new_one() {
    let dir = temp_dir("stale");
    let first = acquire_with(&dir, options(500, Duration::from_secs(45)))
        .await
        .expect("the first attempt wins");
    // Zero stale window: any existing lock is reclaimable at once.
    let second = acquire_with(&dir, options(500, Duration::ZERO))
        .await
        .expect("the stale lock is reclaimed");
    let path = dir.join("daemon.start.lock");

    // The old owner's release is token-checked: it must leave the
    // successor's lock in place (source `releaseDaemonStartLock`).
    drop(first);
    assert!(path.exists(), "the old token must not unlink the new lock");

    drop(second);
    assert!(!path.exists(), "the owner's own drop releases it");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_second_entry_reports_the_running_host_and_fails_fast() {
    let dir = temp_dir("e2e");
    let binary = env!("CARGO_BIN_EXE_openbutlerd");
    let mut first = std::process::Command::new(binary)
        .arg("run")
        .env("OPENBUTLER_DATA_DIR", &dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("the first entry starts");
    let serving = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if openbutler_daemon::start_lock::host_pid(&dir)
                .await
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    assert!(serving.is_ok(), "the first host never answered");

    // The second entry contends: status-first, the source's sentence,
    // and a failing exit — never a silent double bind.
    let output = std::process::Command::new(binary)
        .arg("run")
        .env("OPENBUTLER_DATA_DIR", &dir)
        .output()
        .expect("the second entry runs");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("An OpenButler host is already running (pid "),
        "{stderr}"
    );

    first.kill().ok();
    let _ = first.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
