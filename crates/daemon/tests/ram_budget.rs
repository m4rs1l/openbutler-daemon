#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The idle memory budget, as an executable constraint.
//!
//! The architecture promise is a daemon that stays small: the Node host it
//! replaces idled above 100 MB and grew past 600 MB under load, so "idle
//! fits inside this ceiling" is not a nice-to-have but the reason the
//! rewrite exists. This test starts a real daemon binary, lets it settle,
//! samples its working set, and fails when the sample crosses the line.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

/// The hard ceiling for an idle daemon (IPC only, no listeners asked for).
/// Calibrated against a measured baseline of 8.5 MiB on Windows
/// (2026-11-16, five stable samples) with room for platform variance —
/// well under the 20 MiB architecture target, and tight enough that a
/// regression towards the Node host's footprint cannot hide.
const IDLE_CEILING_BYTES: u64 = 32 * 1024 * 1024;

fn scratch(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("ob-ram-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

/// The process's current working set, from the platform's own inspector.
#[cfg(windows)]
fn working_set(pid: u32) -> Option<u64> {
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!("(Get-Process -Id {pid} -ErrorAction Stop).WorkingSet64"),
        ])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

#[cfg(unix)]
fn working_set(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    let kibibytes: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kibibytes * 1024)
}

#[test]
fn an_idle_daemon_stays_inside_the_memory_ceiling() {
    let data = scratch("idle");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_openbutlerd"))
        .arg("run")
        .env("OPENBUTLER_DATA_DIR", &data)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the daemon binary must start");

    // Let the runtime, the pipe, and the first allocations settle.
    std::thread::sleep(Duration::from_millis(1_500));
    assert!(
        child.try_wait().expect("child status").is_none(),
        "the daemon died during warm-up"
    );

    let mut samples = Vec::new();
    for _ in 0..5 {
        samples.push(working_set(child.id()));
        std::thread::sleep(Duration::from_millis(150));
    }

    child.kill().expect("stop the measured process");
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&data);

    let latest = samples
        .last()
        .expect("one sample at least")
        .expect("the platform must report a working set for its own child");
    let mib = |bytes: u64| bytes as f64 / 1_048_576.0;
    eprintln!(
        "idle working set: {:.1} MiB (samples: {})",
        mib(latest),
        samples
            .iter()
            .map(|sample| match sample {
                Some(bytes) => format!("{:.1}", mib(*bytes)),
                None => "n/a".to_owned(),
            })
            .collect::<Vec<_>>()
            .join(", ")
    );

    assert!(
        latest <= IDLE_CEILING_BYTES,
        "an idle daemon must fit in {} MiB; measured {:.1} MiB",
        IDLE_CEILING_BYTES / 1_048_576,
        mib(latest)
    );
}
