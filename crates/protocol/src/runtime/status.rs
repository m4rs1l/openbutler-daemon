//! Control-plane status shapes.

use serde::{Deserialize, Serialize};

use crate::util::{de_trio_option, trio_absent};

/// Telegram channel state as reported by `status`
/// (TS `TelegramStatus`, `telegram.ts`).
///
/// `username` and `error` are required and nullable: the host always writes
/// them, `null` included, so a client can tell "no username" from a host
/// that predates the field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelegramStatus {
    pub state: TelegramState,
    pub paired: bool,
    #[serde(deserialize_with = "Option::<String>::deserialize")]
    pub username: Option<String>,
    #[serde(deserialize_with = "Option::<String>::deserialize")]
    pub error: Option<String>,
}

/// The five states a Telegram poller reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TelegramState {
    Disabled,
    Connecting,
    Polling,
    Error,
    Stopped,
}

/// Answer to a `status` request on a current host (TS `DaemonStatus`).
///
/// Field semantics worth keeping:
/// - `unresponsive`: the pid is alive but IPC is dead — counts are unknown,
///   they are *not* zero.
/// - `entry`: the host's entry script (source or bundle) for vintage checks.
/// - `build_id`: `null` for a source run, *absent* on an older host — the
///   distinction is kept (see [`crate::util::de_trio_option`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonStatus {
    pub pid: u32,
    pub started_at: String,
    pub workspace_count: u32,
    pub connected_clients: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedules: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telegram: Option<TelegramStatus>,
    /// Present only as `true`; counts are unknown in this state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unresponsive: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    #[serde(
        default,
        deserialize_with = "de_trio_option",
        skip_serializing_if = "trio_absent"
    )]
    pub build_id: Option<Option<String>>,
}
