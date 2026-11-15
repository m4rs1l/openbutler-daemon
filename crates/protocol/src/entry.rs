//! Transcript entries: the tagged union a renderer consumes.
//!
//! An entry is a body (`kind`-tagged) flattened with shared metadata
//! (`at`, `turnId`, `internal`, `turnOutcome`, `turnEndedAt`) — one flat JSON
//! object on the wire, exactly like `SessionEntry = SessionEntryBody &
//! SessionEntryMeta` in TypeScript.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// File-backed payload carried by user entries and inbox messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileAttachment {
    pub id: String,
    pub path: String,
    pub name: String,
    pub media_type: String,
    pub size_bytes: i64,
    pub sha256: String,
}

/// Project binding a session belongs to (`SessionProject`). `id` and `name`
/// are nullable: a locked folder set can outlive its project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionProject {
    #[serde(deserialize_with = "Option::<String>::deserialize")]
    pub id: Option<String>,
    #[serde(deserialize_with = "Option::<String>::deserialize")]
    pub name: Option<String>,
    pub folders: Vec<String>,
    pub workspace_root: String,
    pub locked: bool,
}

/// Severity of a notice entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoticeTone {
    Info,
    Warn,
    Error,
}

/// Where a user entry came from (TS `TURN_SOURCES` — the single authority
/// shared by the type and the save validator).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UserEntrySource {
    Terminal,
    Web,
    Telegram,
    Inbox,
    OwnerInbox,
    Schedule,
    Background,
    Team,
    Worker,
}

/// Machine-readable internal marker; a compaction checkpoint is stored so the
/// record stays complete, but readers show the compaction row instead of the
/// raw summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryInternal {
    Compaction,
}

/// Outcome recorded when a turn ends (`TurnCompletion`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnOutcome {
    Completed,
    Cancelled,
    Failed,
}

/// Progress of a compaction row in the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompactionStatus {
    Running,
    Done,
    Failed,
}

/// Metadata every entry carries so a client can group and time the work.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionEntryMeta {
    /// Epoch milliseconds when the entry was first written.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "at")]
    pub at: Option<i64>,
    /// Id of the user message that opened the turn this entry belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "turnId")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub internal: Option<EntryInternal>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "turnOutcome"
    )]
    pub turn_outcome: Option<TurnOutcome>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "turnEndedAt"
    )]
    pub turn_ended_at: Option<i64>,
}

/// One transcript entry on the wire (TS `SessionEntry`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SessionEntry {
    /// Owner text (or an inbox message), with optional attachments.
    User {
        #[serde(flatten)]
        meta: SessionEntryMeta,
        text: String,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "workspaceRoot"
        )]
        workspace_root: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "imageCount"
        )]
        image_count: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attachments: Option<Vec<FileAttachment>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<UserEntrySource>,
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "eventId")]
        event_id: Option<String>,
    },
    /// The model's reply.
    Assistant {
        #[serde(flatten)]
        meta: SessionEntryMeta,
        text: String,
    },
    /// A statement from the machinery (durable, survives reconnects when it
    /// arrives as a promoted notification).
    Notice {
        #[serde(flatten)]
        meta: SessionEntryMeta,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tone: Option<NoticeTone>,
        /// Machine-readable reason this notice exists (e.g.
        /// `compaction:exceeds-window`); absent on ordinary statements.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cause: Option<String>,
    },
    /// Turn separator.
    Divider {
        #[serde(flatten)]
        meta: SessionEntryMeta,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    /// A reasoning block the owner chose to show.
    Reasoning {
        #[serde(flatten)]
        meta: SessionEntryMeta,
        text: String,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "durationMs"
        )]
        duration_ms: Option<i64>,
    },
    /// Compaction is a visible event: it changes how much context the model
    /// can see, and a reader deserves to see when and how far.
    Compaction {
        #[serde(flatten)]
        meta: SessionEntryMeta,
        status: CompactionStatus,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "beforeTokens"
        )]
        before_tokens: Option<i64>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "afterTokens"
        )]
        after_tokens: Option<i64>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "durationMs"
        )]
        duration_ms: Option<i64>,
    },
    /// One tool invocation: complete redacted input for the expanded view.
    Tool {
        #[serde(flatten)]
        meta: SessionEntryMeta,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<Map<String, Value>>,
        result: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        running: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preparing: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "toolTitle")]
        tool_title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "durationMs"
        )]
        duration_ms: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        outcome: Option<String>,
    },
}

impl SessionEntry {
    /// Shared metadata of this entry, whatever its body.
    pub fn meta(&self) -> &SessionEntryMeta {
        match self {
            SessionEntry::User { meta, .. }
            | SessionEntry::Assistant { meta, .. }
            | SessionEntry::Notice { meta, .. }
            | SessionEntry::Divider { meta, .. }
            | SessionEntry::Reasoning { meta, .. }
            | SessionEntry::Compaction { meta, .. }
            | SessionEntry::Tool { meta, .. } => meta,
        }
    }
}
