//! Coordination types shared by the team store.
//!
//! Source: legacy `team/coordination.ts` (read 2026-11-21) — the nine-state
//! work machine with its terminal/active sets and the peer reports. camelCase
//! everywhere they leave the process: the wire and the persisted `value`
//! column must round-trip `JSON.stringify(work)` exactly.

use serde::{Deserialize, Serialize};

/// A peer's self-description (`PeerProfile`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerProfile {
    pub description: String,
    pub revision: i64,
    pub updated_at: i64,
}

/// One MCP server inside a capability report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerReport {
    pub name: String,
    pub tool_count: i64,
}

/// Reported by the member's host, never inferred from the agent's biography.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerCapabilities {
    pub platform: String,
    pub mcp_servers: Vec<McpServerReport>,
    pub mcp_tools: Vec<String>,
    pub custodian_busy: bool,
    pub observed_at: i64,
}

/// The nine work states (`WorkState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkState {
    Proposed,
    Countered,
    Accepted,
    Working,
    Blocked,
    Completed,
    Failed,
    Rejected,
    Cancelled,
}

impl WorkState {
    /// The source's own strings — the `state` column and the value JSON
    /// agree on them (`as_str` feeds the column, serde the JSON).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Countered => "countered",
            Self::Accepted => "accepted",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Rejected => "rejected",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Work that has reached an end state — the `listPeerWork` filter.
pub const TERMINAL_WORK_STATES: [WorkState; 4] = [
    WorkState::Completed,
    WorkState::Rejected,
    WorkState::Cancelled,
    WorkState::Failed,
];

/// Work the member has accepted but not yet closed: the active load.
pub const ACTIVE_WORK_STATES: [WorkState; 3] =
    [WorkState::Accepted, WorkState::Working, WorkState::Blocked];

/// One unit of coordinated work (`PeerWork`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerWork {
    pub id: String,
    pub requester: String,
    pub assignee: String,
    pub original_terms: String,
    pub terms: String,
    /// Older records without this field contain the requester's original
    /// proposal — absent in JSON when unset, like the source's `?`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terms_author: Option<String>,
    pub state: WorkState,
    pub revision: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_actor: String,
    pub last_action: String,
    pub last_note: String,
    pub result: Option<String>,
}
