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
//! Layout: shared leaves live at the root (`auth`, `constants`, `util`,
//! `error`, `frame`, `envelope`, `payload`); domains that carry several
//! authorities live in their own folder — [`session`] (record, run state,
//! patches, transcript entries, reads, attach) and [`runtime`] (status,
//! runtime view).
//!
//! Documentation index: `docs/index.md`.

pub mod auth;
pub mod constants;
pub mod envelope;
pub mod error;
pub mod frame;
pub mod ledger;
pub mod payload;
pub mod runtime;
pub mod session;
pub mod sidecar;
pub mod util;

// Historical module paths stay valid: these now live inside their domain
// folder, re-exported here so no caller changes.
pub use runtime::status;
pub use session::{attach, entry};

pub use auth::{AUTH_FAILED, PAIR_FAILED, REMOTE_FIRST_FRAME_REFUSED};
pub use constants::{DAEMON_PROTOCOL_VERSION, INLINE_IMAGE_MEDIA_TYPES, MAX_DAEMON_FRAME_BYTES};
pub use entry::{SessionEntry, SessionEntryMeta, SessionProject};
pub use envelope::{
    DaemonEvent, DaemonNotification, DaemonRequest, DaemonResponse, validate_request,
    validate_response,
};
pub use error::{FrameError, ValidationError};
pub use frame::FrameDecoder;
pub use runtime::{ApprovalMode, DaemonStatus, LocalClientKind, RuntimeViewData, TelegramStatus};
pub use session::{
    AttachResult, EntriesRangeRequest, InlineAttachment, SessionEntriesPatch, SessionEntriesRange,
    SessionPatch, SessionSnapshotData, SessionSummary, SessionView,
};
