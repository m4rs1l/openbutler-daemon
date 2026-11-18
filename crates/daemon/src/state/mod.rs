//! Durable state the daemon owns: session records on disk.
//!
//! One authority per concern: `paths` validates ids and builds locations,
//! `record` locates and parses one saved record whole, `save` is the only
//! writer (recompute + header order + atomic rename), `header` reads display
//! fields without materialising transcripts, `list` runs the picker query,
//! `projects` owns `projects.json` and folder canonicalisation.
//! The on-disk contract itself is documented in `docs/state-formats.md`.

mod attachments;
mod header;
mod list;
mod paths;
mod projects;
pub(crate) use record::last_assistant_text;
mod record;
mod save;
pub mod schedules;

pub use attachments::{
    MAX_ATTACHMENT_BYTES, MAX_IMAGE_BYTES, decode_base64_image, image_type_from_bytes,
    store_attachment, verify_attachment_path,
};

pub use header::{SessionHeader, read_session_header};
pub use list::{WorkspaceFilter, list_sessions};
pub(crate) use paths::strip_windows_prefix;
pub use paths::{
    CUSTODIAN_SESSION_ID, is_session_id, same_path, session_directory, sessions_directory,
};
pub use projects::session_project;
/// The one UTF-16 preview slice — shared with the ledger's envelope.
pub(crate) use record::slice_utf16;
pub use record::{
    SessionRecord, parse_record, read_session_record, read_session_view, record_path,
};
pub use save::{SaveOutcome, save_session};

/// Errors of the state layer. Per file, listing stays tolerant: one damaged
/// record must never block the picker.
#[derive(Debug, thiserror::Error)]
/// Why a session-state read or write was refused.
pub enum StateError {
    #[error("Invalid session id.")]
    InvalidSessionId,
    #[error("Session directory escapes its storage root.")]
    Escape,
    /// The write-path gate (legacy `isSavedSession` throw).
    #[error("Cannot save an invalid session.")]
    InvalidRecord,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
