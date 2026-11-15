//! Wire contract between OpenButler clients and the background host.
//!
//! This crate is the single source of truth for the daemon IPC protocol:
//! envelope rules, framing, and the data structures that travel on the wire.
//! It is extracted from the TypeScript implementation (openbutler-005,
//! `src/daemon-protocol.ts`, `src/daemon-frames.ts` and companions, as read
//! 2026-11-16) and pinned by the JSON vectors under `conformance/`.
//!
//! The crate is pure data plus the frame decoder: no I/O, no sockets, no
//! runtime. Both the Rust daemon and the Rust CLI depend on it, and the
//! TypeScript implementation is cross-checked against the same vectors.
//!
//! Documentation index: `docs/index.md`.

pub mod attach;
pub mod constants;
pub mod entry;
pub mod envelope;
pub mod error;
pub mod frame;
pub mod runtime;
pub mod session;
pub mod status;
pub mod util;

pub use attach::AttachResult;
pub use constants::{
    DAEMON_PROTOCOL_VERSION, MAX_DAEMON_FRAME_BYTES, PREVIOUS_DAEMON_PROTOCOL_VERSION,
};
pub use entry::{SessionEntry, SessionEntryMeta, SessionProject};
pub use envelope::{
    DaemonEvent, DaemonNotification, DaemonRequest, DaemonResponse, validate_request,
    validate_response,
};
pub use error::{FrameError, ValidationError};
pub use frame::FrameDecoder;
pub use runtime::{ApprovalMode, LocalClientKind, RuntimeViewData};
pub use session::{
    SessionEntriesPatch, SessionEntriesRange, SessionPatch, SessionSnapshotData, SessionSummary,
};
pub use status::{DaemonStatus, TelegramStatus};
