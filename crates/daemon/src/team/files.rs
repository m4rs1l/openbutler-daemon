//! The team file store — the source's `team/files.ts` (116 lines, read
//! 2026-11-22). Every receive path verifies bytes before atomically
//! replacing its destination; transfer ids are content-addressed on disk
//! (`f_[0-9a-f]{8,}`) while metadata lives in `team.db`.
//!
//! Port note: the source is stream-async (`pipeline` + `AbortSignal`);
//! these are synchronous readers because the team layer is synchronous
//! today — the abort signal is not ported yet (T3b's HTTP handler can
//! spawn_blocking around a size-capped reader). Basename follows the
//! platform rule node uses: both separators on Windows, `/` only on
//! POSIX (plus the win32 lone-drive quirk, `C:\` → `""`).

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

use super::contracts::TeamTransportError;

/// Declared bytes a verified write must match exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileExpectation {
    pub size: u64,
    pub sha256: String,
}

/// A file-store failure: id refusals, transport refusals with their HTTP
/// status, and plain IO (the source's thrown errors).
#[derive(Debug, thiserror::Error)]
pub enum FilesError {
    #[error("\"{0}\" is not a valid file id.")]
    InvalidId(String),
    #[error(transparent)]
    Transport(#[from] TeamTransportError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// `FILE_ID = /^f_[0-9a-f]{8,}$/` — checked byte for byte.
fn check_file_id(id: &str) -> Result<(), FilesError> {
    let valid = id.strip_prefix("f_").is_some_and(|hex| {
        hex.len() >= 8
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    });
    if valid {
        Ok(())
    } else {
        Err(FilesError::InvalidId(id.to_owned()))
    }
}

/// The hub's on-disk file store (`teamFilesDir`): bytes live here, keyed
/// by file id.
pub fn team_files_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("team-files")
}

/// Resolves a file id to its on-disk path (`filePath`), refusing anything
/// that is not a file id.
pub fn file_path(data_dir: &Path, id: &str) -> Result<PathBuf, FilesError> {
    check_file_id(id)?;
    Ok(team_files_dir(data_dir).join(id))
}

/// Outbound bytes are derived from their identity, never from a path
/// supplied by a queue document (`outboundFilePath`).
pub fn outbound_file_path(data_dir: &Path, id: &str) -> Result<PathBuf, FilesError> {
    check_file_id(id)?;
    Ok(data_dir.join("team-outbound-files").join(id))
}

/// Creates the file store directory (`ensureFilesDir`).
pub fn ensure_files_dir(data_dir: &Path) -> Result<(), FilesError> {
    fs::create_dir_all(team_files_dir(data_dir))?;
    Ok(())
}

/// Appends `.<uuid>.part` to any destination path.
fn temporary_beside(destination: &Path, suffix: &str) -> PathBuf {
    let mut name = destination.as_os_str().to_owned();
    name.push(format!(".{}{suffix}", uuid::Uuid::new_v4()));
    PathBuf::from(name)
}

/// Every receive path verifies bytes before atomically replacing its
/// destination (`writeVerifiedFile`): meter while hashing, refuse an
/// overshoot mid-stream, re-check size and hash at the end, rename, and
/// always drop the temporary file.
pub fn write_verified_file(
    source: &mut impl Read,
    destination: &Path,
    expected: &FileExpectation,
) -> Result<(), FilesError> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = temporary_beside(destination, ".part");
    let result = (|| -> Result<(), FilesError> {
        let mut file = File::create(&temporary)?;
        let mut hasher = Sha256::new();
        let mut received: u64 = 0;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = source.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            received += read as u64;
            if received > expected.size {
                return Err(
                    TeamTransportError::new("File exceeds the declared size.", false, 400).into(),
                );
            }
            hasher.update(&buffer[..read]);
            file.write_all(&buffer[..read])?;
        }
        file.flush()?;
        drop(file); // Windows: close before rename.
        let digest = format!("{:x}", hasher.finalize());
        if received != expected.size || digest != expected.sha256 {
            return Err(TeamTransportError::new(
                "File bytes do not match the declared size or sha256.",
                false,
                400,
            )
            .into());
        }
        fs::rename(&temporary, destination)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Publish immutable hub bytes without overwriting an already committed
/// transfer id (`storeFileBytes`): stage, verify, hard-link, and if the id
/// already exists, accept only identical bytes — anything else is a 409.
pub fn store_file_bytes(
    source: &mut impl Read,
    data_dir: &Path,
    id: &str,
    expected: &FileExpectation,
) -> Result<(), FilesError> {
    ensure_files_dir(data_dir)?;
    let destination = file_path(data_dir, id)?;
    let staged = temporary_beside(&destination, ".upload");
    let result = (|| -> Result<(), FilesError> {
        write_verified_file(source, &staged, expected)?;
        match fs::hard_link(&staged, &destination) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let existing = fs::metadata(&destination)?.len();
                if existing != expected.size || hash_file(&destination)? != expected.sha256 {
                    return Err(TeamTransportError::new(
                        "Transfer id already has different bytes.",
                        false,
                        409,
                    )
                    .into());
                }
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    })();
    let _ = fs::remove_file(&staged);
    result
}

/// Whether the id's bytes are present (`fileExists`) — invalid ids and
/// misses are both `false`, because the source's try wraps the resolve too.
pub fn file_exists(data_dir: &Path, id: &str) -> bool {
    match file_path(data_dir, id) {
        Ok(path) => fs::metadata(&path).is_ok_and(|meta| meta.is_file()),
        Err(_) => false,
    }
}

/// Deletes an id's bytes (`deleteFileBytes`): a missing file is a silent
/// success (force), an invalid id still throws.
pub fn delete_file_bytes(data_dir: &Path, id: &str) -> Result<(), FilesError> {
    let path = file_path(data_dir, id)?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Opens an id's bytes for reading (`readFileStream`).
pub fn open_file(data_dir: &Path, id: &str) -> Result<File, FilesError> {
    Ok(File::open(file_path(data_dir, id)?)?)
}

/// Streams a local file through sha256 without loading it into memory
/// (`hashFile`).
pub fn hash_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Node's `path.basename` on the running platform (win32 keeps both
/// separators; posix treats `\` as a normal character).
fn basename(name: &str) -> &str {
    #[cfg(windows)]
    let is_separator: fn(char) -> bool = |character| character == '/' || character == '\\';
    #[cfg(not(windows))]
    let is_separator: fn(char) -> bool = |character| character == '/';
    let trimmed = name.trim_end_matches(is_separator);
    #[cfg(windows)]
    if trimmed.len() <= 3 && trimmed.ends_with(':') {
        // win32: a lone drive (`C:`) basenames to the empty string.
        return "";
    }
    match trimmed.rfind(is_separator) {
        Some(index) => &trimmed[index + 1..],
        None => trimmed,
    }
}

/// Lexical `path.resolve` over one absolute base: `.` disappears, `..`
/// pops the previous segment, and climbing past the root is a no-op.
fn resolve(base: &Path, rest: &[&str]) -> PathBuf {
    let mut parts: Vec<Component<'_>> = Vec::new();
    for component in base.components() {
        parts.push(component);
    }
    for segment in rest {
        for component in Path::new(segment).components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    if matches!(parts.last(), Some(Component::Normal(_))) {
                        parts.pop();
                    }
                }
                other => parts.push(other),
            }
        }
    }
    parts.into_iter().collect()
}

/// The default landing spot for a received file
/// (`defaultInboxPath`): `appdata/openbutler/team-inbox/<team>/<name>`,
/// basename first, then `resolve()` across every segment.
pub fn default_inbox_path(data_dir: &Path, team_key: &str, name: &str) -> PathBuf {
    let base = data_dir.join("team-inbox");
    resolve(&base, &[team_key, basename(name)])
}
