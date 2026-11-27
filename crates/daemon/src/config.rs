//! Data directory authority: where OpenButler keeps state.
//!
//! Everything the daemon writes goes through here — nothing is written inside
//! the checkout (a tool that scatters caches next to its source breaks on a
//! read-only install and pollutes the user's git status). The resolution has
//! exactly two rungs: the environment, then the platform convention. No other
//! location on the machine is ever consulted.

use std::path::{Path, PathBuf};

use crate::state::home_dir;

/// Resolves the data directory: `OPENBUTLER_DATA_DIR` when set, otherwise the
/// platform default.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("OPENBUTLER_DATA_DIR").filter(|dir| !dir.is_empty()) {
        return PathBuf::from(dir);
    }
    default_data_dir()
}

/// The dated scratch root (`getScratchRoot`, legacy `paths.ts` L83):
/// `<tmpdir>/openbutler-daemon/YYYY-MM-DD` — outside the data directory because
/// it is disposable, dated so a day of scratch can be discarded, and
/// reclaimed by the OS at any time (that is the point). The directory name
/// keeps this stack apart from the legacy Node install's scratch.
pub fn scratch_root() -> PathBuf {
    std::env::temp_dir()
        .join("openbutler-daemon")
        .join(chrono::Local::now().format("%Y-%m-%d").to_string())
}

fn default_data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join("AppData").join("Local"));
        base.join("openbutler-daemon")
    }
    #[cfg(target_os = "macos")]
    {
        home_dir()
            .join("Library")
            .join("Application Support")
            .join("openbutler-daemon")
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join(".local").join("share"));
        base.join("openbutler-daemon")
    }
}

/// The Node program the sidecar runs under, resolved in one order:
///
/// 1. `OPENBUTLER_NODE` — development, and the escape hatch on a machine that
///    already has its own Node,
/// 2. the runtime an install ships beside the binary (`<exe dir>/node/node[.exe]`),
/// 3. `node` on `PATH`.
///
/// The sibling wins over `PATH` on purpose: a packaged host must use the
/// runtime it shipped with, never whatever stray `node` the machine happens to
/// have first.
pub fn node_program() -> PathBuf {
    let exe = std::env::current_exe().ok();
    node_program_in(
        std::env::var_os("OPENBUTLER_NODE"),
        exe.as_deref().and_then(Path::parent),
    )
}

/// [`node_program`] with its inputs given, so the order is testable without
/// touching this process's environment or the filesystem around the test host.
fn node_program_in(override_path: Option<std::ffi::OsString>, exe_dir: Option<&Path>) -> PathBuf {
    if let Some(path) = override_path.filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    if let Some(dir) = exe_dir {
        let name = if cfg!(windows) { "node.exe" } else { "node" };
        let sibling = dir.join("node").join(name);
        if sibling.is_file() {
            return sibling;
        }
    }
    PathBuf::from("node")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::node_program_in;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ob-node-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn runtime_name() -> &'static str {
        if cfg!(windows) { "node.exe" } else { "node" }
    }

    #[test]
    fn the_environment_wins_over_everything() {
        let dir = scratch("env");
        std::fs::create_dir_all(dir.join("node")).unwrap();
        std::fs::write(dir.join("node").join(runtime_name()), b"").unwrap();

        let resolved = node_program_in(Some(OsString::from("/custom/node")), Some(&dir));
        assert_eq!(resolved, Path::new("/custom/node"));

        let empty = node_program_in(Some(OsString::new()), Some(&dir));
        assert_eq!(
            empty,
            dir.join("node").join(runtime_name()),
            "an empty override is not an override"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_shipped_runtime_beats_the_path() {
        let dir = scratch("ship");
        std::fs::create_dir_all(dir.join("node")).unwrap();
        let shipped = dir.join("node").join(runtime_name());
        std::fs::write(&shipped, b"").unwrap();

        assert_eq!(node_program_in(None, Some(&dir)), shipped);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn without_a_shipped_runtime_the_path_name_is_used() {
        let dir = scratch("none");
        assert_eq!(
            node_program_in(None, Some(&dir)),
            PathBuf::from("node"),
            "a source build has no runtime beside it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
