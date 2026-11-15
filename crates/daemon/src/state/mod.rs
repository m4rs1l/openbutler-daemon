//! Durable state the daemon owns: session records on disk.
//!
//! One authority per concern: `paths` validates ids and builds locations,
//! `header` reads display fields without materialising transcripts, `list`
//! runs the picker query. The on-disk contract itself is documented in
//! `docs/state-formats.md`.

mod header;
mod list;
mod paths;

pub use header::{SessionHeader, read_session_header};
pub use list::{WorkspaceFilter, list_sessions};
pub use paths::{
    CUSTODIAN_SESSION_ID, is_session_id, same_path, session_directory, sessions_directory,
};

/// Errors of the state layer. Per file, listing stays tolerant: one damaged
/// record must never block the picker.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("Invalid session id.")]
    InvalidSessionId,
    #[error("Session directory escapes its storage root.")]
    Escape,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
