//! Runtime view: what the active runtime offers this attachment.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Permission posture of a workspace (TS `ApprovalMode`).
///
/// [`ApprovalMode::Ask`] is not selectable going forward: it is accepted only
/// when an older host reports it, and readers canonicalize it to
/// [`ApprovalMode::Workspace`] (the TS migration rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalMode {
    Workspace,
    AutoReview,
    FullAccess,
    Ask,
}

/// Which local surface owns a socket (TS `LocalClientKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LocalClientKind {
    Terminal,
    Web,
}

/// Whether the model's reasoning is available, unsupported, or simply
/// unmapped by the adapter (TS `ReasoningSupport`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReasoningSupport {
    Available,
    ModelUnsupported,
    AdapterUnmapped,
}

/// One selectable reasoning level with its provider payload
/// (TS `ReasoningVariant`: `{id, label, providerOptions}`).
///
/// `providerOptions` is a namespace-keyed object whose values are themselves
/// JSON objects (`Record<string, Record<string, JSONValue>>`); the inner
/// objects ride as [`Value`] to keep the wire bytes identical.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningVariant {
    pub id: String,
    pub label: String,
    pub provider_options: Map<String, Value>,
}

/// A display-only reasoning level reference on a saved session
/// (`{id, label}` — no provider payload).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningLabel {
    pub id: String,
    pub label: String,
}

/// View of the active runtime, as sent in `snapshot` events and attach
/// results (TS `RuntimeViewData`).
///
/// The TypeScript interface also carries *function* members
/// (`saveReasoningPreference`, `saveShowReasoningPreference`,
/// `saveApprovalModePreference`, `reportRetry`). `JSON.stringify` never emits
/// function values, so they are absent from the wire and omitted here; serde
/// ignores them if a peer ever sends anything under those names anyway.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeViewData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_session_attachments: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_projects: Option<bool>,
    pub workspace_root: String,
    pub model_name: String,
    /// Absent until the host supplies one; readers fall back to `provider_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_name: Option<String>,
    pub provider_id: String,
    pub model_id: String,
    #[serde(deserialize_with = "Option::<i64>::deserialize")]
    pub context_window_tokens: Option<i64>,
    /// The head's configured compaction budget; absent means the identity
    /// default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custodian_context_limit_tokens: Option<i64>,
    pub supports_vision: bool,
    pub reasoning_variants: Vec<ReasoningVariant>,
    #[serde(deserialize_with = "Option::<String>::deserialize")]
    pub reasoning_metadata_source: Option<String>,
    pub reasoning_support: ReasoningSupport,
    /// Resolved against the runtime's model — already usable as-is.
    pub reasoning_default: String,
    /// The machine's raw stored preference, unresolved; each chat resolves it
    /// against the levels its own model offers.
    pub reasoning_preference: String,
    pub show_reasoning_default: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_titles: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_mode: Option<ApprovalMode>,
}
