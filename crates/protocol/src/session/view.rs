//! Saved-record view: metadata, the full view, the list row, and the
//! older-entries page.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::entry::{SessionEntry, SessionProject};
use crate::runtime::{ReasoningLabel, ReasoningSupport};

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
