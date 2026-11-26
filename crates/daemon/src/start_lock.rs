//! The daemon-entry start lock (source: `daemon.ts`
//! `acquireDaemonStartLock` L858-890, `removeStaleDaemonStartLock` L944,
//! `releaseDaemonStartLock` L928): one entry attempt holds
//! `daemon.start.lock` from before the status check until the host
//! stops; a contender gets the source's status-first answer, then the
//! stale rule, then the deadline sentence.
//!
//! Deliberate divergences, both listed: no heartbeat and no owner-pid
//! liveness check. The source's heartbeat only fed the freshness rule
//! its pid-record branch consumed; this build keeps no pid record, and
//! a contender probes the endpoint **before** it ever looks at the
//! file, so a running host is answered by the probe and a crashed one
//! is reclaimed by the stale window (a hard kill within 45 s of boot
//! costs one wait, not a wrong answer).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::json;

/// The source's `DAEMON_START_LOCK_*` constants, injectable for tests.
pub struct StartLockOptions {
    /// How long a contender waits for a starting host (30 s).
    pub wait: Duration,
    /// After this much silence the lock is reclaimable (45 s).
    pub stale: Duration,
}

impl Default for StartLockOptions {
    fn default() -> Self {
        Self {
            wait: Duration::from_secs(30),
            stale: Duration::from_secs(45),
        }
    }
}

/// A held start lock. Dropping it releases — token-checked, so a lock
/// reclaimed by a newer startup is never unlinked by the old owner.
#[derive(Debug)]
pub struct StartLock {
    path: PathBuf,
    token: String,
}

impl Drop for StartLock {
    fn drop(&mut self) {
        let Ok(bytes) = std::fs::read(&self.path) else {
            return;
        };
        let Ok(record) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            return;
        };
        if record.get("token").and_then(serde_json::Value::as_str) == Some(self.token.as_str()) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Acquires the lock with the source's timings.
pub async fn acquire(data_dir: &Path) -> Result<StartLock, String> {
    acquire_with(data_dir, StartLockOptions::default()).await
}

/// The acquisition loop with injectable timings (the test seam).
pub async fn acquire_with(data_dir: &Path, options: StartLockOptions) -> Result<StartLock, String> {
    std::fs::create_dir_all(data_dir).map_err(|error| {
        format!(
            "Could not prepare the data directory {}: {error}",
            data_dir.display()
        )
    })?;
    let path = data_dir.join("daemon.start.lock");
    let deadline = tokio::time::Instant::now() + options.wait;
    loop {
        let token = uuid::Uuid::new_v4().to_string();
        let record = json!({
            "pid": std::process::id(),
            "startedAt": chrono::Utc::now().to_rfc3339(),
            "token": &token,
        });
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                if let Err(error) = serde_json::to_writer(&mut file, &record) {
                    let _ = std::fs::remove_file(&path);
                    return Err(format!(
                        "Could not write the start lock {}: {error}",
                        path.display()
                    ));
                }
                return Ok(StartLock { path, token });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(format!(
                    "Could not create the start lock {}: {error}",
                    path.display()
                ));
            }
        }

        // Source order on contention: the host's own answer first, so a
        // running host is reported at once instead of waited out.
        if let Some(pid) = host_pid(data_dir).await {
            return Err(format!(
                "An OpenButler host is already running (pid {pid})."
            ));
        }
        // ...then the stale rule (age only; see the module doc).
        if remove_stale(&path, options.stale) {
            continue;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(
                "Another OpenButler host is still starting. Retry after its startup finishes."
                    .to_owned(),
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The source's stale rule over the file's age: a lock silent for the
/// stale window is reclaimable; a missing file already counts as gone
/// (the source's ENOENT branch).
fn remove_stale(path: &Path, stale: Duration) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return true;
    };
    // `lstat` must show a regular file; symlinks and other entries are
    // never removed (the source's guard).
    if !meta.is_file() {
        return false;
    }
    let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default();
    if age < stale {
        return false;
    }
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

/// Asks a running host for its pid (the source's `getDaemonStatus`
/// inside the contention loop); `None` means nobody answers.
pub async fn host_pid(data_dir: &Path) -> Option<u32> {
    host_status(data_dir)
        .await?
        .get("pid")
        .and_then(serde_json::Value::as_u64)
        .map(|pid| pid as u32)
}

/// The running host's `status` answer as the wire carries it; `None` means
/// nobody answers.
pub async fn host_status(data_dir: &Path) -> Option<serde_json::Value> {
    use openbutler_protocol::endpoint::Endpoint;
    use openbutler_protocol::{
        DaemonRequest, DaemonResponse, FrameDecoder, MAX_DAEMON_FRAME_BYTES,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[cfg(windows)]
    type Stream = tokio::net::windows::named_pipe::NamedPipeClient;
    #[cfg(unix)]
    type Stream = tokio::net::unix::Stream;

    async fn connect(endpoint: &Endpoint) -> Option<Stream> {
        #[cfg(windows)]
        if let Endpoint::NamedPipe(name) = endpoint {
            return tokio::net::windows::named_pipe::ClientOptions::new()
                .open(name)
                .ok();
        }
        #[cfg(unix)]
        if let Endpoint::UnixSocket(path) = endpoint {
            return tokio::net::unix::Stream::connect(path).await.ok();
        }
        None
    }

    let endpoint = openbutler_protocol::endpoint::endpoint_for(data_dir);
    let mut stream = tokio::time::timeout(Duration::from_secs(1), connect(endpoint.target()))
        .await
        .ok()??;
    let request = serde_json::to_vec(&DaemonRequest::new(
        uuid::Uuid::new_v4().to_string(),
        "status",
    ))
    .ok()?;
    stream.write_all(&request).await.ok()?;
    stream.write_all(b"\n").await.ok()?;

    let mut decoder = FrameDecoder::new(MAX_DAEMON_FRAME_BYTES);
    let mut buffer = vec![0u8; 8192];
    loop {
        let read = tokio::time::timeout(Duration::from_secs(1), stream.read(&mut buffer))
            .await
            .ok()?
            .ok()?;
        if read == 0 {
            return None;
        }
        if let Some(frame) = decoder.append(&buffer[..read]).ok()?.into_iter().next() {
            let value: serde_json::Value = serde_json::from_slice(&frame).ok()?;
            let response: DaemonResponse = serde_json::from_value(value).ok()?;
            return match response {
                DaemonResponse::Success { result, .. } => Some(result),
                DaemonResponse::Failure { .. } => None,
            };
        }
    }
}
