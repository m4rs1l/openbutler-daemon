//! Live run state: the queue, the pending question, the tool approval, and
//! the busy flags both snapshots and patches carry.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

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
