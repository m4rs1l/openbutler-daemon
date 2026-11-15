//! Data directory authority: where OpenButler keeps state.
//!
//! Everything the daemon writes goes through here — nothing is written inside
//! the checkout (a tool that scatters caches next to its source breaks on a
//! read-only install and pollutes the user's git status). Mirrors the legacy
//! `src/paths.ts`, including the one-time adoption of the pre-rename `ranite`
//! directory so an existing install never looks wiped.

use std::fs;
use std::path::PathBuf;

/// Resolves the data directory: `OPENBUTLER_DATA_DIR` when set, otherwise the
/// platform default, adopting the legacy `ranite` directory on first run.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("OPENBUTLER_DATA_DIR").filter(|dir| !dir.is_empty()) {
        return PathBuf::from(dir);
    }
    let current = default_data_dir("openbutler");
    let legacy = default_data_dir("ranite");
    migrate_legacy(current, legacy)
}

/// A path inside the data directory.
pub fn data_path(name: &str) -> PathBuf {
    data_dir().join(name)
}

fn default_data_dir(product: &str) -> PathBuf {
    #[cfg(windows)]
    {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join("AppData").join("Local"));
        base.join(product)
    }
    #[cfg(target_os = "macos")]
    {
        home_dir()
            .join("Library")
            .join("Application Support")
            .join(product)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join(".local").join("share"));
        base.join(product)
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// First run after the rename adopts `ranite`'s state *only* when the new
/// directory does not exist yet: two populated directories are resolved in
/// favor of the current one rather than merged blindly.
fn migrate_legacy(current: PathBuf, legacy: PathBuf) -> PathBuf {
    if current.exists() || !legacy.exists() {
        return current;
    }
    match fs::rename(&legacy, &current) {
        Ok(()) => current,
        // A locked or read-only legacy directory must not make the daemon
        // unusable: keep reading the old location until a later run.
        Err(_) => legacy,
    }
}
