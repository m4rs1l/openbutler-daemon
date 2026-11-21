//! Wire and persistence DTOs — the source's `team/contracts.ts`
//! (read 2026-11-21). Adapters preserve these fields instead of
//! redefining them: camelCase on the wire, field order as declared.

/// The presence window a peer counts as online.
pub const TEAM_PRESENCE_WINDOW_MS: i64 = 15_000;
/// A capability report older than this no longer counts as current.
pub const TEAM_CAPABILITY_MAX_AGE_MS: i64 = 45_000;
/// When a member should renew its capability report.
pub const TEAM_CAPABILITY_RENEW_MS: i64 = 20_000;
/// The message id budget (`MAX_TEAM_MESSAGE_ID_CHARS`).
pub const MAX_TEAM_MESSAGE_ID_CHARS: usize = 256;
/// The handshake banner (`TEAM_PROTOCOL_VERSION`).
pub const TEAM_PROTOCOL_VERSION: &str = "openbutler-team/2";

/// One team message (`TeamMessage`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamMessage {
    pub seq: i64,
    pub msg_id: String,
    pub sender: String,
    pub recipient: String,
    pub kind: String,
    pub body: String,
    pub file_id: Option<String>,
    pub created_at: i64,
    pub reply_to: Option<String>,
}
