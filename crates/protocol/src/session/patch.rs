//! Incremental change frames: the transcript patch, the state snapshot, and
//! the patch envelope that carries them.

use serde::{Deserialize, Serialize};

use super::entry::SessionEntry;
use super::run::RunState;
use super::view::{SessionMeta, SessionView};
use crate::runtime::RuntimeViewData;

/// One queued message reference inside a delta patch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexedEntry {
    pub index: i64,
    pub entry: SessionEntry,
}

/// One text continuation inside a delta patch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDelta {
    pub index: i64,
    pub delta: String,
}

/// Incremental transcript change (TS `SessionEntriesPatch`).
///
/// All indices are absolute host indices; the client translates them into its
/// window, drops anything below it, and trims its front when the window
/// doubles.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SessionEntriesPatch {
    /// Append new entries and/or rewrite existing ones in place.
    Delta {
        append: Vec<SessionEntry>,
        replace: Vec<IndexedEntry>,
        #[serde(rename = "appendText")]
        append_text: Vec<TextDelta>,
    },
    /// Replace the whole transcript (session switch, compaction).
    #[serde(rename = "replace-all")]
    ReplaceAll { entries: Vec<SessionEntry> },
}

/// Wire snapshot of a session (TS `SessionSnapshotData`): the record with
/// entries, the run state, and the window markers.
///
/// `entriesFrom`/`totalEntries` are present only when the host sent a tail
/// window; their absence means *full transcript*, which is what an older
/// client that never asked for a window always received.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSnapshotData {
    pub session: SessionView,
    #[serde(flatten)]
    pub run: RunState,
    /// Absolute host index of `session.entries[0]` for a windowed frame.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "entriesFrom"
    )]
    pub entries_from: Option<i64>,
    /// Full transcript length when `entries_from` is present.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "totalEntries"
    )]
    pub total_entries: Option<i64>,
}

/// Incremental session state (TS `SessionPatchData`): the record *without*
/// entries, an optional entries patch, the run state, and the runtime view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPatch {
    pub session: SessionMeta,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "entriesPatch"
    )]
    pub entries_patch: Option<SessionEntriesPatch>,
    #[serde(flatten)]
    pub run: RunState,
    pub runtime: RuntimeViewData,
}
