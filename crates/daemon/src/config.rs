//! Data directory authority: where OpenButler keeps state.
//!
//! Everything the daemon writes goes through here — nothing is written inside
//! the checkout (a tool that scatters caches next to its source breaks on a
//! read-only install and pollutes the user's git status). The resolution has
//! exactly two rungs: the environment, then the platform convention. No other
//! location on the machine is ever consulted.

use std::path::PathBuf;

/// Resolves the data directory: `OPENBUTLER_DATA_DIR` when set, otherwise the
/// platform default.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("OPENBUTLER_DATA_DIR").filter(|dir| !dir.is_empty()) {
        return PathBuf::from(dir);
    }
    default_data_dir()
}

/// The dated scratch root (`getScratchRoot`, legacy `paths.ts` L83):
/// `<tmpdir>/openbutler/YYYY-MM-DD` — outside the data directory because
/// it is disposable, dated so a day of scratch can be discarded, and
/// reclaimed by the OS at any time (that is the point).
pub fn scratch_root() -> PathBuf {
    std::env::temp_dir()
        .join("openbutler")
        .join(chrono::Local::now().format("%Y-%m-%d").to_string())
}

fn default_data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join("AppData").join("Local"));
        base.join("openbutler")
    }
    #[cfg(target_os = "macos")]
    {
        home_dir()
            .join("Library")
            .join("Application Support")
            .join("openbutler")
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join(".local").join("share"));
        base.join("openbutler")
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}
