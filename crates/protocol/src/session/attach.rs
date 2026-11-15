//! The `attach` answer: what a client adopts when it joins a session.

use serde::{Deserialize, Serialize};

use crate::runtime::{LocalClientKind, RuntimeViewData};
use crate::session::{SessionSnapshotData, SessionSummary};
use crate::util::{de_trio_option, trio_absent};

/// Result of the `attach` request (TS `AttachResult`).
///
/// Capability fields double as version negotiation: a newer client reads a
/// missing `sessionAttachments` or an unexpected `clientKind` echo as "this
/// host is older" and reports a restart instruction instead of silently
/// degrading (the TS capability-order rule).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachResult {
    /// The host's entry script — source tree or built bundle — for the
    /// vintage check against the client's own entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_entry: Option<String>,
    /// Immutable compiled host identity: `null` for a source run, absent on
    /// an older host (the distinction is preserved).
    #[serde(
        default,
        deserialize_with = "de_trio_option",
        skip_serializing_if = "trio_absent"
    )]
    pub host_build_id: Option<Option<String>>,
    /// The host can attach this conversation without replacing another
    /// controller; `true` whenever present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_attachments: Option<bool>,
    /// Host-issued binding key; every later request echoes it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "workspaceKey"
    )]
    pub workspace_key: Option<String>,
    /// The client identity the host accepted for this socket.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "clientKind"
    )]
    pub client_kind: Option<LocalClientKind>,
    pub snapshot: SessionSnapshotData,
    pub runtime: RuntimeViewData,
    pub saved_sessions: Vec<SessionSummary>,
}
