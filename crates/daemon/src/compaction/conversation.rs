//! Conversation markers and the projection helpers that bound a compacted
//! conversation: tool references, preview retention, archive pointers and
//! the queued-message order repair.
//!
//! Source: legacy `conversation.ts` (read 2026-11-18). Messages are plain
//! JSON values here - the same `ModelMessage` shape the door carries -
//! and every helper is pure: no disk, no model, no state.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

/// Prefix of every checkpoint message the engine commits.
pub const COMPACTION_SUMMARY_MARKER: &str = "OpenButler conversation summary (context only):\n";
/// Prefix of every tool-reference placeholder a projection writes.
pub const TOOL_REFERENCE_MARKER: &str = "OpenButler tool exchange (raw context preview):\n";
/// The synthetic user message that resumes the task after a checkpoint.
/// Distinguished from identical real text by its provider options.
pub const COMPACTION_CONTINUE_TEXT: &str = "Continue the unfinished user request from the compacted history. Use the summary to recover the current goal, constraints, completed work, and next action.";

/// Per-field preview bound inside the newest tool batch, and the batch's
/// total budget shared across fields.
const TOOL_PREVIEW_FIELD_CHARS: i64 = 8_192;
const TOOL_PREVIEW_BATCH_CHARS: i64 = 32_768;
/// Head/tail truncation marker, byte-exact from the source.
const TRUNCATION_MARKER: &str = "\n… [middle truncated] …\n";

/// The synthetic continue message (`COMPACTION_CONTINUE_MESSAGE`).
pub fn continue_message() -> Value {
    json!({
        "role": "user",
        "content": COMPACTION_CONTINUE_TEXT,
        "providerOptions": { "openbutler": { "compactionControl": true } },
    })
}

/// `appendTranscriptArchiveReference` - the completed checkpoint carries
/// the session-file pointer exactly once.
pub fn append_transcript_archive_reference(content: &str, archive_path: &str) -> String {
    let reference = format!(
        "\n\nOpenButler transcript archive (original messages and tool results): {archive_path}"
    );
    if content.contains(&reference) {
        content.to_owned()
    } else {
        format!("{content}{reference}")
    }
}

/// A decoded tool-reference placeholder.
#[derive(Debug, Clone, PartialEq, Eq)]
/// A tool call kept in the condensed view (id plus result).
pub struct ToolReference {
    pub batch_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub input: String,
    pub output_type: String,
    pub output: String,
}

/// `parseToolReference` - only an assistant string message that carries
/// the marker and a complete six-string record decodes; everything else
/// (wrong role, no marker, invalid JSON, non-object, non-string field)
/// is not a reference.
pub fn parse_tool_reference(message: &Value) -> Option<ToolReference> {
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let content = message.get("content")?.as_str()?;
    let payload = content.strip_prefix(TOOL_REFERENCE_MARKER)?;
    let value: Value = serde_json::from_str(payload).ok()?;
    let object = value.as_object()?;
    let field = |name: &str| object.get(name).and_then(Value::as_str).map(str::to_owned);
    Some(ToolReference {
        batch_id: field("batchId")?,
        tool_call_id: field("toolCallId")?,
        tool_name: field("toolName")?,
        input: field("input")?,
        output_type: field("outputType")?,
        output: field("output")?,
    })
}

/// The source writes the record in declaration order (`JSON.stringify` of
/// the spread), so the rebuilt content is assembled field by field - a
/// map's key order would not match the original bytes.
pub(super) fn reference_content(reference: &ToolReference) -> String {
    let quote = |value: &str| {
        serde_json::to_string(&Value::String(value.to_owned()))
            .unwrap_or_else(|_| String::from("\"\""))
    };
    format!(
        "{TOOL_REFERENCE_MARKER}{{\"batchId\":{},\"toolCallId\":{},\"toolName\":{},\"input\":{},\"outputType\":{},\"output\":{}}}",
        quote(&reference.batch_id),
        quote(&reference.tool_call_id),
        quote(&reference.tool_name),
        quote(&reference.input),
        quote(&reference.output_type),
        quote(&reference.output),
    )
}

/// `retainLatestToolPreviews` (called only after a completed checkpoint
/// covers these exchanges): references outside the newest batch are
/// dropped; the newest batch's fields share `min(8_192, batch / (n * 2))`
/// and are truncated head-and-tail when they exceed it. Returns
/// `(messages, changed)` so callers can keep source object identity.
pub fn retain_latest_tool_previews_changed(messages: Vec<Value>) -> (Vec<Value>, bool) {
    let references: Vec<Option<ToolReference>> =
        messages.iter().map(parse_tool_reference).collect();
    let Some(latest) = references
        .iter()
        .flatten()
        .last()
        .map(|r| r.batch_id.clone())
    else {
        return (messages, false);
    };
    let count = references
        .iter()
        .flatten()
        .filter(|r| r.batch_id == latest)
        .count();
    // `count >= 1` by construction (the newest reference is in the batch).
    let field_limit = std::cmp::min(
        TOOL_PREVIEW_FIELD_CHARS,
        TOOL_PREVIEW_BATCH_CHARS / (count as i64 * 2),
    );
    let mut changed = false;
    let mut retained = Vec::with_capacity(messages.len());
    for (index, message) in messages.into_iter().enumerate() {
        let Some(reference) = references[index].clone() else {
            retained.push(message);
            continue;
        };
        if reference.batch_id != latest {
            changed = true;
            continue;
        }
        let input = truncate_head_and_tail(&reference.input, field_limit);
        let output = truncate_head_and_tail(&reference.output, field_limit);
        if input == reference.input && output == reference.output {
            retained.push(message);
            continue;
        }
        changed = true;
        let mut rebuilt = reference;
        rebuilt.input = input;
        rebuilt.output = output;
        let mut message = message;
        if let Some(object) = message.as_object_mut() {
            object.insert(
                "content".to_owned(),
                Value::String(reference_content(&rebuilt)),
            );
        }
        retained.push(message);
    }
    (retained, changed)
}

/// The plain form of the projection above.
pub fn retain_latest_tool_previews(messages: Vec<Value>) -> Vec<Value> {
    retain_latest_tool_previews_changed(messages).0
}

/// `truncateHeadAndTail` - both ends survive inside the limit. Lengths
/// are UTF-16 units (the source's `String.length`); a cut that would
/// split a surrogate pair replaces it, because a Rust string cannot hold
/// the lone half the source would keep.
pub fn truncate_head_and_tail(value: &str, maximum_chars: i64) -> String {
    if maximum_chars <= 0 {
        return String::new();
    }
    let units: Vec<u16> = value.encode_utf16().collect();
    if units.len() as i64 <= maximum_chars {
        return value.to_owned();
    }
    let marker: Vec<u16> = TRUNCATION_MARKER.encode_utf16().collect();
    if maximum_chars as usize <= marker.len() {
        return String::from_utf16_lossy(&marker[..maximum_chars as usize]);
    }
    let remaining = maximum_chars as usize - marker.len();
    // Head takes three quarters, tail the rest (JS `Math.ceil`).
    let head_length = (remaining as f64 * 0.75).ceil() as usize;
    let tail_length = remaining - head_length;
    let mut out = String::from_utf16_lossy(&units[..head_length]);
    out.push_str(TRUNCATION_MARKER);
    out.push_str(&String::from_utf16_lossy(
        &units[units.len() - tail_length..],
    ));
    out
}

/// `retireCoveredToolPreviews` - finish the projection of legacy saved
/// checkpoints without summarising fresh messages: the boundary is the
/// last continue message, and only a prefix that actually holds a
/// checkpoint is retired.
pub fn retire_covered_tool_previews(messages: Vec<Value>) -> Vec<Value> {
    let boundary = messages.iter().rposition(|message| {
        message.get("role").and_then(Value::as_str) == Some("user")
            && message.get("content") == Some(&Value::String(COMPACTION_CONTINUE_TEXT.to_owned()))
            && message["providerOptions"]["openbutler"]["compactionControl"] == Value::Bool(true)
    });
    let Some(boundary) = boundary else {
        return messages;
    };
    let covered = messages[..boundary]
        .iter()
        .enumerate()
        .any(|(index, message)| {
            message.get("role").and_then(Value::as_str) == Some("assistant")
                && message
                    .get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|content| content.starts_with(COMPACTION_SUMMARY_MARKER))
                && (message["providerOptions"]["openbutler"]["compactionCheckpoint"]
                    == Value::Bool(true)
                    || index == 0)
        });
    if !covered {
        return messages;
    }
    let (retained, changed) = retain_latest_tool_previews_changed(messages[..=boundary].to_vec());
    if !changed {
        return messages;
    }
    let mut out = retained;
    out.extend_from_slice(&messages[boundary + 1..]);
    out
}

/// `repairQueuedMessageOrder` - repair the historical queue-index bug
/// using existing results: a queued user message that landed inside an
/// unresolved tool batch moves to that batch's boundary. Nothing is ever
/// invented or discarded.
pub fn repair_queued_message_order(messages: Vec<Value>) -> Vec<Value> {
    // Where each tool result lives.
    let mut result_positions: HashMap<String, usize> = HashMap::new();
    for (index, message) in messages.iter().enumerate() {
        if message.get("role").and_then(Value::as_str) != Some("tool") {
            continue;
        }
        let Some(parts) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for part in parts {
            if part.get("type").and_then(Value::as_str) == Some("tool-result")
                && let Some(id) = part.get("toolCallId").and_then(Value::as_str)
            {
                result_positions.insert(id.to_owned(), index);
            }
        }
    }
    // Walk: collect queued users that sit inside an open batch.
    let mut pending: HashSet<String> = HashSet::new();
    let mut boundary: Option<usize> = None;
    let mut moves: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut moved: HashSet<usize> = HashSet::new();
    for (index, message) in messages.iter().enumerate() {
        let role = message.get("role").and_then(Value::as_str);
        let parts = message.get("content").and_then(Value::as_array);
        if role == Some("assistant")
            && let Some(parts) = parts
        {
            for part in parts {
                if part.get("type").and_then(Value::as_str) != Some("tool-call") {
                    continue;
                }
                // Provider-executed calls never wait for a host result.
                if part.get("providerExecuted") == Some(&Value::Bool(true)) {
                    continue;
                }
                let id = part
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let result = result_positions.get(id).copied();
                let unresolved = match result {
                    Some(position) => position > index,
                    None => false,
                };
                if !unresolved {
                    continue;
                }
                if pending.is_empty() {
                    boundary = Some(index);
                }
                pending.insert(id.to_owned());
            }
        }
        if role == Some("tool")
            && let Some(parts) = parts
        {
            for part in parts {
                if part.get("type").and_then(Value::as_str) == Some("tool-result")
                    && let Some(id) = part.get("toolCallId").and_then(Value::as_str)
                {
                    pending.remove(id);
                }
            }
        }
        if role == Some("user")
            && !pending.is_empty()
            && message["providerOptions"]["openbutler"]["queuedMessageId"].is_string()
            && let Some(boundary) = boundary
        {
            moves.entry(boundary).or_default().push(index);
            moved.insert(index);
        }
    }
    if moves.is_empty() {
        return messages;
    }
    // At the boundary: the queued users first, then the message that
    // opened the batch; at their original positions: nothing.
    let mut out = Vec::with_capacity(messages.len());
    for (index, message) in messages.iter().enumerate() {
        if let Some(group) = moves.get(&index) {
            for &position in group {
                out.push(messages[position].clone());
            }
        }
        if !moved.contains(&index) {
            out.push(message.clone());
        }
    }
    out
}
