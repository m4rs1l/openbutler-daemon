//! The Node sidecar door: commands (host → sidecar) and events
//! (sidecar → host) over newline-delimited JSON on stdin/stdout.
//!
//! The Rust side owns the agent loop, retries, queueing and cancellation;
//! the sidecar is a stateless model transport — one process, many calls,
//! no conversation state. Unknown event types are ignored and logged by
//! the decoder (forward compatibility); a line that is not JSON, or that
//! exceeds the line cap, is a protocol violation and kills the process.
//! Source decision record: `docs/sidecar-door.md`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The door's own protocol version, negotiated at the handshake.
pub const SIDECAR_PROTOCOL_VERSION: u32 = 1;

/// One provider entry in a call. `key` must be on the sidecar's embedded
/// allowlist — an unknown key is refused, never used as a package name.
/// `api_key` travels only over stdin and is never logged or persisted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SidecarProvider {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

/// A tool handed to the SDK as a schema only: there is no `execute`
/// channel in the contract, so the SDK cannot run its own loop — the
/// host receives the call and decides.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SidecarTool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: Value,
}

/// One model call. `messages` is the AI SDK message array kept as opaque
/// JSON: the typed extraction lands with the agent-loop slice, and the
/// door's job is a lossless pass-through (providerOptions, reasoning
/// parts and cache marks included).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SidecarCall {
    pub id: String,
    pub provider: SidecarProvider,
    pub model: String,
    pub messages: Value,
    #[serde(default)]
    pub tools: Vec<SidecarTool>,
}

/// Host → sidecar commands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SidecarCommand {
    #[serde(rename = "call")]
    Call(SidecarCall),
    #[serde(rename = "cancel")]
    Cancel { id: String },
}

/// The sidecar's startup announcement. The host refuses to send calls
/// until this arrives and validates: protocol version match, and the
/// requested provider keys present in the allowlist report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SidecarReady {
    pub protocol_version: u32,
    pub ai_sdk_version: String,
    pub providers: Vec<String>,
}

/// Error classes the host makes policy decisions from. `Other` catches
/// unknown kinds so a newer sidecar never breaks the door.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SidecarErrorKind {
    Auth,
    RateLimit,
    ContextOverflow,
    Network,
    Aborted,
    InvalidRequest,
    #[serde(other)]
    Other,
}

/// Sidecar → host events. Field names are camelCase on the wire; each
/// carries the call id it belongs to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum SidecarEvent {
    #[serde(rename = "ready")]
    Ready(SidecarReady),
    #[serde(rename = "text-delta")]
    TextDelta { id: String, delta: String },
    #[serde(rename = "reasoning-delta")]
    ReasoningDelta {
        id: String,
        delta: String,
        /// The provider's thinking signature when it arrives on this part
        /// (Anthropic signature, OpenAI encrypted reasoning) — forwarded
        /// untouched so multi-step turns can echo it back.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    #[serde(rename = "tool-call")]
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
    },
    #[serde(rename = "usage")]
    Usage {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        completion_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total_tokens: Option<u64>,
    },
    #[serde(rename = "finish")]
    Finish {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finish_reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<Value>,
    },
    #[serde(rename = "error")]
    Error {
        id: String,
        kind: SidecarErrorKind,
        retryable: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}
