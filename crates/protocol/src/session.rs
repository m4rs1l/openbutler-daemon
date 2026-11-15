//! Session state on the wire: records, snapshots, patches, and paging.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::entry::{SessionEntry, SessionProject};
use crate::runtime::{ReasoningLabel, ReasoningSupport, RuntimeViewData};

/// Saved-record fields shared by a full view and a patch view: everything
/// `SavedSession` carries except `entries`, `conversation`, and
/// `conversationArchive` (the conversation arrays never leave the host).
///
/// Used directly as `patch.session`; [`SessionView`] flattens it together with
/// `entries` for snapshot sessions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<SessionProject>,
    /// Saved-record format version (literal `1` in TypeScript).
    pub version: u32,
    pub id: String,
    pub title: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    /// Content-activity stamp used for ordering; absent on records saved
    /// before the field existed (listing falls back to `updatedAt`).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "lastActivityAt"
    )]
    pub last_activity_at: Option<String>,
    pub provider_id: String,
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "showReasoning"
    )]
    pub show_reasoning: Option<bool>,
    /// The levels this session's own model offers; keeps the chat's picker
    /// honest while the shared runtime is bound to another model.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "reasoningVariants"
    )]
    pub reasoning_variants: Option<Vec<ReasoningLabel>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "supportsVision"
    )]
    pub supports_vision: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "reasoningSupport"
    )]
    pub reasoning_support: Option<ReasoningSupport>,
    /// Optional and nullable as in TypeScript; readers treat `null` and an
    /// absent key alike (see docs/ipc-protocol.md, open questions).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "reasoningMetadataSource"
    )]
    pub reasoning_metadata_source: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "reasoningTitles"
    )]
    pub reasoning_titles: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "workspaceRoot"
    )]
    pub workspace_root: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "contextTokens"
    )]
    pub context_tokens: Option<i64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "contextEstimateTokens"
    )]
    pub context_estimate_tokens: Option<i64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "contextWindowTokens"
    )]
    pub context_window_tokens: Option<i64>,
    /// Opaque provider/key-pool ids mapped to the last selected key position.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "keyPositions"
    )]
    pub key_positions: Option<Map<String, Value>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "hasUserMessage"
    )]
    pub has_user_message: Option<bool>,
    /// Owner stop/Esc policy survives host restarts.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "automaticWakeHeld"
    )]
    pub automatic_wake_held: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "firstUserMessage"
    )]
    pub first_user_message: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "lastAssistantMessage"
    )]
    pub last_assistant_message: Option<String>,
}

/// A session record with its transcript — `patch` and window math address the
/// entries by absolute host index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionView {
    #[serde(flatten)]
    pub meta: SessionMeta,
    pub entries: Vec<SessionEntry>,
}

/// A queued owner message awaiting its turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuedMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub text: String,
    pub image_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steering: Option<bool>,
}

/// The queued message as previewed in a snapshot: text plus image count.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuedMessageBrief {
    pub text: String,
    pub image_count: u32,
}

/// A question the host waits for (`session.answer`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingQuestion {
    pub id: String,
    pub question: String,
    /// Free-form options, or `null` for a plain-text question.
    #[serde(deserialize_with = "Option::<Vec<String>>::deserialize")]
    pub options: Option<Vec<String>>,
}

/// Declared effect of one tool call (TS `ToolEffect`), shown by approval UIs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ToolEffect {
    pub change: ToolChange,
    pub data: ToolData,
    pub network: ToolNetwork,
    pub reversible: ToolReversible,
    pub outbound: ToolOutbound,
}

/// What the call touches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolChange {
    None,
    Create,
    Modify,
    Delete,
    Execute,
    Unknown,
}

/// Sensitivity of the data involved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolData {
    Public,
    Local,
    Sensitive,
    Secret,
    Unknown,
}

/// Whether the call uses the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolNetwork {
    None,
    Possible,
    Used,
    Unknown,
}

/// Whether the effect can be undone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolReversible {
    Full,
    Compensating,
    None,
    Unknown,
}

/// Where results flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolOutbound {
    None,
    Model,
    ToolServer,
    ModelAndToolServer,
    Unknown,
}

/// A tool call waiting for a decision (`pendingApproval`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallRequest {
    pub request_id: String,
    pub provider_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub input: Map<String, Value>,
    pub effect: ToolEffect,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "timeoutSeconds"
    )]
    pub timeout_seconds: Option<u64>,
    /// Why the deterministic gates escalated this call to a human or reviewer.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "gateReason"
    )]
    pub gate_reason: Option<String>,
}

/// The run state both snapshots and patches carry, field for field identical
/// in the two TypeScript types.
///
/// `busy`, `queuedMessageCount`, and `queuedMessages` are plain required
/// fields; the `null`-able ones are required *and* nullable — the host always
/// writes them, `null` included, so absence marks a malformed frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunState {
    pub busy: bool,
    /// The turn the running work writes into; `null` before a run has placed
    /// its message. Clients must not guess the live turn from `busy` plus list
    /// position.
    #[serde(deserialize_with = "Option::<String>::deserialize")]
    pub running_turn_id: Option<String>,
    #[serde(deserialize_with = "Option::<ToolCallRequest>::deserialize")]
    pub pending_approval: Option<ToolCallRequest>,
    #[serde(deserialize_with = "Option::<String>::deserialize")]
    pub pending_approval_id: Option<String>,
    #[serde(deserialize_with = "Option::<PendingQuestion>::deserialize")]
    pub pending_question: Option<PendingQuestion>,
    pub queued_message_count: u32,
    #[serde(deserialize_with = "Option::<QueuedMessageBrief>::deserialize")]
    pub first_queued_message: Option<QueuedMessageBrief>,
    pub queued_messages: Vec<QueuedMessage>,
    #[serde(deserialize_with = "Option::<String>::deserialize")]
    pub session_save_error: Option<String>,
    /// Absolute index of the entry currently streaming, or `null`.
    #[serde(deserialize_with = "Option::<i64>::deserialize")]
    pub streaming_assistant_index: Option<i64>,
}

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

/// Saved-session list row (`SessionSummary`): denormalised so the picker
/// never parses `entries`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<SessionProject>,
    pub id: String,
    pub title: String,
    pub preview: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "lastActivityAt"
    )]
    pub last_activity_at: Option<String>,
    pub provider_id: String,
    pub model_id: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "workspaceRoot"
    )]
    pub workspace_root: Option<String>,
}

/// A page of older transcript addressed by absolute index
/// (`session.entries-range`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEntriesRange {
    pub from: i64,
    pub to: i64,
    pub total: i64,
    pub entries: Vec<SessionEntry>,
}
