//! Session identity and location — the single place ids become paths,
//! and the one workspace-path canonicalizer every scope decision shares.

use std::path::{Component, Path, PathBuf};

use super::StateError;
use crate::shape::is_absolute_path;

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

/// `fs::canonicalize` on Windows returns the `\\?\` extended form; the
/// source's `realpathSync` never does. One helper, one spelling: every
/// path that answers the wire strips it here (the schedule's workspace
/// root, the project store's folders).
pub(crate) fn strip_windows_prefix(path: &std::path::Path) -> std::path::PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return std::path::PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return std::path::PathBuf::from(rest);
    }
    path.to_path_buf()
}

/// The custodian workspace root, spelled the way `canonical_scope_root`
/// spells a workspace it accepted: the data directory is resolved first, so
/// a second spelling of it (an 8.3 short name, a junction, a `..` leg, a
/// case difference) cannot hide the child behind a string comparison.
pub(crate) fn canonical_child_root(parent: &Path, child: &str) -> String {
    let resolved = std::fs::canonicalize(parent)
        .map(|path| strip_windows_prefix(&path))
        .unwrap_or_else(|_| parent.to_path_buf());
    resolved.join(child).to_string_lossy().into_owned()
}

/// `canonicalScopeRoot`: expand `~`, resolve against the process
/// directory, follow symlinks, require a directory. The single path
/// normalizer — every layer used to carry its own variant of this
/// (the source consolidated it for the same reason, workspace-origin.ts).
pub(crate) fn canonical_scope_root(path: &str) -> Result<String, String> {
    let requested = path.trim();
    if requested.is_empty() {
        return Err("Workspace path cannot be empty.".to_owned());
    }
    let home = home_dir().to_string_lossy().into_owned();
    let expanded = if requested == "~" {
        home.clone()
    } else if let Some(rest) = requested
        .strip_prefix("~/")
        .or_else(|| requested.strip_prefix("~\\"))
    {
        format!("{home}{rest}")
    } else {
        requested.to_owned()
    };
    let absolute = if is_absolute_path(&expanded) {
        PathBuf::from(expanded)
    } else {
        std::env::current_dir()
            .map_err(|error| error.to_string())?
            .join(expanded)
    };
    let real = std::fs::canonicalize(&absolute).map_err(|error| error.to_string())?;
    let real = strip_windows_prefix(&real);
    if !real.is_dir() {
        return Err(format!(
            "Workspace path is not a directory: {}",
            real.display()
        ));
    }
    Ok(real.to_string_lossy().into_owned())
}

/// The home directory the source gets from `os.homedir()`: the platform
/// variable first (USERPROFILE on Windows, HOME elsewhere), the other as a
/// fallback. One authority — the settings paths, the skills roots and the
/// shell's `~` expansion all read this.
pub(crate) fn home_dir() -> PathBuf {
    #[cfg(windows)]
    let (first, second) = ("USERPROFILE", "HOME");
    #[cfg(not(windows))]
    let (first, second) = ("HOME", "USERPROFILE");
    std::env::var_os(first)
        .or_else(|| std::env::var_os(second))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}
