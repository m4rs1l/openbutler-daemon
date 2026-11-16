//! Session state on the wire: records, snapshots, patches, and paging.
//!
//! Split by authority so each file owns exactly one concern:
//! [`view`] holds the saved record and its list row, [`run`] the live turn
//! state, [`patch`] the incremental changes, [`entry`] the transcript union,
//! [`read`] the three disk-authoritative reads (payloads, page math, inline
//! attachment filter), and [`attach`] the handshake that delivers the first
//! snapshot.

pub mod attach;
pub mod entry;
pub mod patch;
pub mod read;
pub mod run;
pub mod view;

pub use attach::AttachResult;
pub use patch::{IndexedEntry, SessionEntriesPatch, SessionPatch, SessionSnapshotData, TextDelta};
pub use read::{EntriesRangeRequest, InlineAttachment};
pub use run::{
    PendingQuestion, QueuedMessage, QueuedMessageBrief, RunState, ToolCallRequest, ToolChange,
    ToolData, ToolEffect, ToolNetwork, ToolOutbound, ToolReversible,
};
pub use view::{SessionEntriesRange, SessionMeta, SessionSummary, SessionView};
