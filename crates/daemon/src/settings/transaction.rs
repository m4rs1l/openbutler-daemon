//! The settings durability boundary: lock, journal, atomic writes.
//!
//! Ported from the TypeScript `settings-transaction.ts` (read 2026-11-16),
//! with one deliberate upgrade: the cross-process lock is an OS advisory
//! lock on `settings.lock` instead of a PID file with age-based stealing —
//! the kernel releases it when the holder dies, so a crashed writer can
//! never leave a lock behind, and no staleness heuristic is needed.
//!
//! The contract this layer guarantees, for any payload:
//!
//! - a reader never observes half of a two-file commit (one lock covers
//!   reads and writes);
//! - an interrupted commit heals — or refuses loudly — before the next
//!   operation touches anything;
//! - a malformed journal overwrites nothing.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// The lock file name.
const LOCK_FILE: &str = "settings.lock";
/// The journal that carries a pending pair commit.
const JOURNAL_FILE: &str = "settings-transaction.json";
/// How long an acquisition waits before giving up.
const LOCK_WAIT: Duration = Duration::from_secs(10);
/// Poll interval while waiting for a foreign holder.
const LOCK_POLL: Duration = Duration::from_millis(25);

/// Everything this boundary can fail with, in the words callers surface.
#[derive(Debug, thiserror::Error)]
/// Why a settings write transaction was refused.
pub enum TransactionError {
    /// The lock stayed held past the wait.
    #[error("Timed out waiting for the settings transaction lock.")]
    Timeout,
    /// The journal exists but does not describe a valid pending commit.
    #[error("The settings transaction journal is invalid. No configuration was overwritten.")]
    InvalidJournal,
    /// An I/O failure underneath (the message is the OS's own).
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Where the pending pair commit lives.
pub fn journal_path(data_dir: &Path) -> PathBuf {
    data_dir.join(JOURNAL_FILE)
}

/// One pending pair commit: both documents, versioned.
#[derive(Serialize, Deserialize)]
struct Journal {
    version: u32,
    settings: String,
    config: String,
}

/// A held lock on the settings boundary. The guard's lifetime *is* the
/// critical section: dropping it releases the OS lock.
pub struct SettingsLock {
    _file: File,
}

impl std::fmt::Debug for SettingsLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsLock").finish_non_exhaustive()
    }
}

/// Acquires the boundary and heals any interrupted commit before handing
/// the guard back — every operation starts from a consistent pair.
pub fn acquire(data_dir: &Path, wait: Duration) -> Result<SettingsLock, TransactionError> {
    std::fs::create_dir_all(data_dir)?;
    let file = OpenOptions::new()
        .create(true)
        // Never truncate: the content names an owner process, and
        // this side only needs the lock the handle holds.
        .truncate(false)
        .write(true)
        .open(data_dir.join(LOCK_FILE))?;

    let deadline = Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => break,
            // `TryLockError` has exactly one variant: the lock is held.
            Err(_) => {
                if Instant::now() >= deadline {
                    return Err(TransactionError::Timeout);
                }
                std::thread::sleep(LOCK_POLL);
            }
        }
    }

    let lock = SettingsLock { _file: file };
    recover(data_dir)?;
    Ok(lock)
}

/// Applies a pending journal, if one exists. Runs on every acquisition, so
/// a crash between the journal write and the renames heals before anything
/// new reads or writes.
fn recover(data_dir: &Path) -> Result<(), TransactionError> {
    let journal = journal_path(data_dir);
    let raw = match std::fs::read_to_string(&journal) {
        Ok(raw) => raw,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(TransactionError::Io(error)),
    };
    // A journal that does not parse, or does not claim version 1 with both
    // documents, refuses: nothing is overwritten, and the file stays for
    // inspection.
    let pending: Journal =
        serde_json::from_str(&raw).map_err(|_| TransactionError::InvalidJournal)?;
    if pending.version != 1 {
        return Err(TransactionError::InvalidJournal);
    }

    atomic_write(&data_dir.join("settings.yaml"), pending.settings.as_bytes())?;
    atomic_write(&data_dir.join("config.yaml"), pending.config.as_bytes())?;
    std::fs::remove_file(&journal)?;
    Ok(())
}

impl SettingsLock {
    /// Journals both documents, then applies them as one logical commit —
    /// the pair is either wholly old or wholly new.
    pub fn commit_pair(
        &self,
        data_dir: &Path,
        settings: &str,
        config: &str,
    ) -> Result<(), TransactionError> {
        let pending = Journal {
            version: 1,
            settings: settings.to_owned(),
            config: config.to_owned(),
        };
        let encoded = serde_json::to_vec(&pending)
            .map_err(|error| TransactionError::Io(std::io::Error::other(error.to_string())))?;
        atomic_write(&journal_path(data_dir), &encoded)?;
        recover(data_dir)
    }

    /// A one-file update through the same boundary: no journal needed, the
    /// atomic rename is the whole transaction.
    pub fn commit_single(&self, data_dir: &Path, payload: &str) -> Result<(), TransactionError> {
        atomic_write(&data_dir.join("config.yaml"), payload.as_bytes())?;
        Ok(())
    }
}

/// Writes `bytes` to `path` atomically: a brand-new
/// temporary file beside the target (exclusive create, so a stale one can
/// never be silently reused), flushed to disk, then renamed into place. An
/// interrupted write can only ever leave the previous file behind.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("settings path has no file name"))?;
    let temp = path.with_file_name(format!(
        "{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id()
    ));

    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let result = (|| {
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// The default wait the whole layer uses.
pub fn default_wait() -> Duration {
    LOCK_WAIT
}
