//! Session identity and location — the single place ids become paths.

use std::path::{Component, Path, PathBuf};

use super::StateError;

/// The machine custodian is an internal session, not a chat: reserved id on
/// every install, never listed, never removable.
pub const CUSTODIAN_SESSION_ID: &str = "00000000-0000-4000-8000-000000000000";

/// The sessions root inside a data directory.
pub fn sessions_directory(data_root: &Path) -> PathBuf {
    data_root.join("sessions")
}

/// `sessions/<id>` for a validated id. Validation happens before any path is
/// built, so a crafted id can never escape the root.
pub fn session_directory(data_root: &Path, id: &str) -> Result<PathBuf, StateError> {
    if !is_session_id(id) {
        return Err(StateError::InvalidSessionId);
    }
    Ok(sessions_directory(data_root).join(id))
}

/// Session id shape: 8-4-4-4-12 hex,
/// version nibble 1–8, variant `[89ab]`, case-insensitive.
pub fn is_session_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    let groups: [&[u8]; 5] = [
        &bytes[0..8],
        &bytes[9..13],
        &bytes[14..18],
        &bytes[19..23],
        &bytes[24..36],
    ];
    if bytes[8] != b'-' || bytes[13] != b'-' || bytes[18] != b'-' || bytes[23] != b'-' {
        return false;
    }
    if !groups
        .iter()
        .all(|group| group.iter().all(u8::is_ascii_hexdigit))
    {
        return false;
    }
    matches!(bytes[14], b'1'..=b'8')
        && matches!(bytes[19].to_ascii_lowercase(), b'8' | b'9' | b'a'..=b'b')
}

/// Path equality: lexical resolution of
/// `.`/`..`, then a case-insensitive compare on Windows.
pub fn same_path(left: &str, right: &str) -> bool {
    let left = normalize_lexical(Path::new(left));
    let right = normalize_lexical(Path::new(right));
    let left = left.to_string_lossy();
    let right = right.to_string_lossy();
    if cfg!(windows) {
        left.to_lowercase() == right.to_lowercase()
    } else {
        left == right
    }
}

/// Makes a path absolute against the current directory and folds `.`/`..`
/// lexically — without touching the filesystem (Node `path.resolve` semantics:
/// a path that does not exist still has a meaning).
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut base = if path.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    };
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => base.push(prefix.as_os_str()),
            Component::RootDir => base.push("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                base.pop();
            }
            Component::Normal(part) => base.push(part),
        }
    }
    base
}
