//! Endpoint derivation — the single authority for how clients find us.
//!
//! Endpoint discovery: on Windows the pipe name is
//! `\\.\pipe\openbutler-<sha256 prefix>` of the lowercased resolved data
//! directory (so every client on this machine derives the same name); on
//! POSIX it is `<data>/daemon.sock`. The data directory travels with the
//! endpoint so the server can never dispatch against a different install
//! than the one it listens for.

use std::path::{Path, PathBuf};

/// Where the daemon listens.
#[derive(Debug, Clone, PartialEq, Eq)]
/// The listen endpoint a flag or setting names.
pub enum Endpoint {
    /// Windows named pipe: the full `\\.\pipe\...` name.
    NamedPipe(String),
    /// POSIX unix socket file.
    UnixSocket(PathBuf),
}

/// A resolved endpoint: target plus the data directory it belongs to.
#[derive(Debug, Clone)]
/// The endpoint after parsing (address plus port).
pub struct ResolvedEndpoint {
    data_dir: PathBuf,
    target: Endpoint,
}

impl ResolvedEndpoint {
    /// The data directory this endpoint serves (dispatch reads state here).
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// The transport target (for tests and diagnostics).
    pub fn target(&self) -> &Endpoint {
        &self.target
    }

    /// The connect target as an owned string (lossy only for non-UTF-8 POSIX
    /// paths, which the product never creates).
    pub fn socket_name(&self) -> String {
        match &self.target {
            Endpoint::NamedPipe(name) => name.clone(),
            Endpoint::UnixSocket(path) => path.to_string_lossy().into_owned(),
        }
    }
}

/// Derives the endpoint for a data directory.
pub fn endpoint_for(data_dir: &Path) -> ResolvedEndpoint {
    #[cfg(windows)]
    let target = {
        use sha2::{Digest, Sha256};
        let resolved = data_dir.to_string_lossy().to_lowercase();
        let digest = Sha256::digest(resolved.as_bytes());
        let suffix: String = digest[..12]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Endpoint::NamedPipe(format!(r"\\.\pipe\openbutler-{suffix}"))
    };
    #[cfg(not(windows))]
    let target = Endpoint::UnixSocket(data_dir.join("daemon.sock"));

    ResolvedEndpoint {
        data_dir: data_dir.to_path_buf(),
        target,
    }
}
