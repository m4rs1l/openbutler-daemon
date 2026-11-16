//! The three disk-authoritative session reads: payload rules, the
//! absolute-index page math, and the inline-attachment filter —
//! `session.load`, `session.entries-range`, and `session.find-attachment`.
//!
//! Source: `daemon-host.ts` dispatch cases L3594-L3640 (read 2026-11-16).
//! Each action's payload and answer shape live together here so one file owns
//! the action end to end; the reading itself (where the record is and how it
//! is parsed) is the daemon's `state` layer.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::entry::{FileAttachment, SessionEntry};
use crate::constants::{INLINE_IMAGE_MEDIA_TYPES, MAX_ENTRIES_RANGE_LIMIT};
use crate::error::ValidationError;
use crate::payload::{require_integer, require_string};

/// Page size when the client omits `limit` (TS `limit === undefined ? 50`).
pub const DEFAULT_ENTRIES_LIMIT: u64 = 50;

/// `before` accepts 0..=Number.MAX_SAFE_INTEGER (2^53-1).
const MAX_BEFORE: u64 = 9_007_199_254_740_991;
/// Byte caps from the legacy `requireString` calls.
const SESSION_ID_MAX_BYTES: usize = 64;
const ATTACHMENT_ID_MAX_BYTES: usize = 128;

/// `session.load` payload: the chat to open by id.
pub fn load_session_id(payload: &Map<String, Value>) -> Result<String, ValidationError> {
    require_string(payload.get("sessionId"), "sessionId", SESSION_ID_MAX_BYTES)
}

/// `session.find-attachment` payload: the attachment id to resolve.
pub fn find_attachment_id(payload: &Map<String, Value>) -> Result<String, ValidationError> {
    require_string(
        payload.get("attachmentId"),
        "attachmentId",
        ATTACHMENT_ID_MAX_BYTES,
    )
}

/// `session.entries-range` payload after validation: `before` is `None` when
/// the client asked for the tail (the answer substitutes `total`, as in the
/// legacy `Math.min`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntriesRangeRequest {
    pub before: Option<u64>,
    pub limit: u64,
}

/// Parses the range payload with the legacy rules and defaults. Extra keys
/// are ignored, like the TypeScript index signature.
pub fn entries_range(payload: &Map<String, Value>) -> Result<EntriesRangeRequest, ValidationError> {
    let before = match payload.get("before") {
        None => None,
        Some(value) => Some(require_integer(Some(value), "before", 0, MAX_BEFORE)?),
    };
    let limit = match payload.get("limit") {
        None => DEFAULT_ENTRIES_LIMIT,
        Some(value) => require_integer(Some(value), "limit", 1, MAX_ENTRIES_RANGE_LIMIT as u64)?,
    };
    Ok(EntriesRangeRequest { before, limit })
}

impl EntriesRangeRequest {
    /// The absolute window over a transcript of `total` entries: `before` is
    /// clamped to `total` (TS `Math.min(total, before)`), the start is floored
    /// at zero (TS `Math.max(0, before - limit)`), and an absent `before`
    /// means the end. Returns the half-open `(start, end)` slice bounds.
    pub fn resolve(&self, total: u64) -> (u64, u64) {
        let end = self.before.map_or(total, |before| before.min(total));
        let start = end.saturating_sub(self.limit);
        (start, end)
    }
}

/// `session.find-attachment` success payload (`{path, mediaType}`); a miss
/// answers an explicit `null`, not a failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InlineAttachment {
    pub path: String,
    pub media_type: String,
}

/// The host-side attachment lookup: user entries only, exact id, and only a
/// type the browser can render inline (the legacy flatMap + find over
/// `INLINE_IMAGE_MEDIA_TYPES` — a pdf never resolves whatever its id).
pub fn find_inline_attachment<'a>(
    entries: &'a [SessionEntry],
    id: &str,
) -> Option<&'a FileAttachment> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            SessionEntry::User {
                attachments: Some(files),
                ..
            } => Some(files.as_slice()),
            _ => None,
        })
        .flatten()
        .find(|file| file.id == id && INLINE_IMAGE_MEDIA_TYPES.contains(&file.media_type.as_str()))
}
