//! Envelope rules: requests, responses, and unsolicited events.
//!
//! Frames are UTF-8 JSON + `\n`. One socket carries both responses and events;
//! receivers distinguish them by the presence of `event` versus `id` + `ok`.
//! Every request carries a version, a correlation id, and an action; payloads
//! ride as flattened extra keys (the TypeScript index signature), which is how
//! `workspaceKey` and every action parameter travel.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::constants::{DAEMON_PROTOCOL_VERSION, MAX_ACTION_LENGTH, MAX_ID_LENGTH};
use crate::error::ValidationError;
use crate::runtime::RuntimeViewData;
use crate::session::{SessionPatch, SessionSnapshotData};

/// Counts JS string length: UTF-16 code units, exactly like
/// `String.prototype.length`. A byte count here would reject ids the
/// TypeScript gate accepts and split the two implementations.
pub fn js_length(value: &str) -> usize {
    value.chars().map(char::len_utf16).sum()
}

/// Request frame (client → host): `{version, id, action, ...payload}`.
///
/// `workspaceKey` is one of the flattened payload keys and is omitted on
/// `attach` — the host issues it, the client adopts it, and every later
/// request echoes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonRequest {
    pub version: u32,
    pub id: String,
    pub action: String,
    #[serde(flatten)]
    pub payload: Map<String, Value>,
}

impl DaemonRequest {
    /// Builds a request at the current protocol version.
    pub fn new(id: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            version: DAEMON_PROTOCOL_VERSION,
            id: id.into(),
            action: action.into(),
            payload: Map::new(),
        }
    }

    /// Attaches one payload key.
    pub fn with(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.payload.insert(key.into(), value.into());
        self
    }

    /// Reads a payload key.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.payload.get(key)
    }
}

/// Host-side request gate (`isDaemonRequest`): version match, `id` and
/// `action` each 1..=64 JS string length units.
pub fn validate_request(request: &DaemonRequest) -> Result<(), ValidationError> {
    if request.version != DAEMON_PROTOCOL_VERSION {
        return Err(ValidationError::WrongVersion {
            expected: DAEMON_PROTOCOL_VERSION,
            actual: request.version,
        });
    }
    let id = js_length(&request.id);
    if !(1..=MAX_ID_LENGTH).contains(&id) {
        return Err(ValidationError::InvalidIdLength { actual: id });
    }
    let action = js_length(&request.action);
    if !(1..=MAX_ACTION_LENGTH).contains(&action) {
        return Err(ValidationError::InvalidActionLength { actual: action });
    }
    Ok(())
}

/// Response frame: success carries `result`, failure carries `error`.
///
/// The two shapes are told apart by which key exists, never by `ok` alone —
/// exactly like the TypeScript union, and the tagged representation here makes
/// a half-and-half frame impossible to express.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DaemonResponse {
    Success {
        version: u32,
        id: String,
        ok: bool,
        result: Value,
    },
    Failure {
        version: u32,
        id: String,
        ok: bool,
        error: String,
    },
}

impl DaemonResponse {
    /// The correlation id this response settles.
    pub fn id(&self) -> &str {
        match self {
            DaemonResponse::Success { id, .. } | DaemonResponse::Failure { id, .. } => id,
        }
    }

    /// The protocol version this response carries.
    pub fn version(&self) -> u32 {
        match self {
            DaemonResponse::Success { version, .. } | DaemonResponse::Failure { version, .. } => {
                *version
            }
        }
    }
}

/// Client-side response gate (`isBasicResponse`): the response must echo the
/// requested version, and a success with `ok:false` (or a failure with
/// `ok:true`) is malformed.
pub fn validate_response(
    response: &DaemonResponse,
    expected_version: u32,
) -> Result<(), ValidationError> {
    if response.version() != expected_version {
        return Err(ValidationError::ResponseVersionMismatch {
            expected: expected_version,
            actual: response.version(),
        });
    }
    match response {
        DaemonResponse::Success { ok, .. } if !*ok => Err(ValidationError::MalformedResponse),
        DaemonResponse::Failure { ok, .. } if *ok => Err(ValidationError::MalformedResponse),
        _ => Ok(()),
    }
}

/// Unsolicited host → client frame. Distinguished from a response by the
/// `event` key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "lowercase")]
pub enum DaemonEvent {
    /// Full session state; also pushed right after `attach` with the same
    /// window as the response.
    Snapshot {
        version: u32,
        snapshot: SessionSnapshotData,
        runtime: RuntimeViewData,
    },
    /// Incremental session state.
    Patch { version: u32, patch: SessionPatch },
    /// Owner-directed message. An empty title with content is promoted into
    /// the transcript by clients; a titled one is a transient notice.
    Notification {
        version: u32,
        title: String,
        message: String,
    },
}

/// The notification frame as its own named shape (the TypeScript
/// `DaemonNotification` alias); clients use it for owner messages only, never
/// for model conversation patches.
pub type DaemonNotification = DaemonEvent;
