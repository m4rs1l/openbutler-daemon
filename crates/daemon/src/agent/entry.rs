//! Transcript entry construction: one owner message, one assistant block,
//! one tool row and one notice - how they are built, grown and settled.
use super::*;

/// The model-visible user message: host-context labels in front of the raw
/// text, parts array (`queuedUserMessageToModelMessage`). Labels never enter
/// the transcript — only this message carries them.
pub(super) fn user_model_message(request: &TurnRequest) -> Value {
    let source_label = match request.source {
        UserEntrySource::Telegram => "[Host context: owner message via Telegram]",
        UserEntrySource::Terminal => "[Host context: owner message via terminal]",
        UserEntrySource::Web => "[Host context: owner message via web]",
        UserEntrySource::Inbox
        | UserEntrySource::OwnerInbox
        | UserEntrySource::Team
        | UserEntrySource::Worker => "[Host context: inbox notification]",
        UserEntrySource::Schedule | UserEntrySource::Background => "",
    };
    // The label travels only to the model; newlines in a path would break
    // the line structure, so the source spaces them out.
    let workspace_label = match request.workspace_root.as_deref() {
        Some(root) => format!(
            "[Host context: active workspace {}]",
            root.replace(['\r', '\n'], " ")
        ),
        None => String::new(),
    };
    // A Telegram turn has to be answered in Telegram: text kept in this
    // session is invisible to the owner.
    let delivery_hint = match request.source {
        UserEntrySource::Telegram => {
            "[Host context: reply through send_message_to_owner; text written only in this session never reaches the owner]"
        }
        _ => "",
    };
    let text = [
        source_label,
        workspace_label.as_str(),
        delivery_hint,
        request.text.as_str(),
    ]
    .iter()
    .filter(|part| !part.is_empty())
    .copied()
    .collect::<Vec<_>>()
    .join("\n");
    let content = if text.is_empty() {
        Vec::new()
    } else {
        vec![json!({ "type": "text", "text": text })]
    };
    let mut content = content;
    if !request.attachments.is_empty() {
        content.push(json!({
            "type": "text",
            "text": attachment_references(&request.attachments)
        }));
    }
    json!({ "role": "user", "content": content })
}

/// `attachmentReferences`: `[image#N="path"]` / `[file#N="path"]`,
/// image and file counters kept apart, joined by newlines.
fn attachment_references(attachments: &[openbutler_protocol::entry::FileAttachment]) -> String {
    let mut images = 0usize;
    let mut files = 0usize;
    attachments
        .iter()
        .map(|attachment| {
            let label = if attachment.media_type.starts_with("image/") {
                images += 1;
                format!("image#{images}")
            } else {
                files += 1;
                format!("file#{files}")
            };
            let path = serde_json::to_string(&attachment.path).unwrap_or_else(|_| "\"\"".into());
            format!("[{label}={path}]")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `finishTurn`: stamp the turn's outcome onto the user entry that opened
/// it, in place — the row keeps its position and timing.
pub(super) fn finish_user_entry(record: &mut SessionRecord, turn_id: &str, outcome: TurnOutcome) {
    let ended_at = Utc::now().timestamp_millis();
    for entry in &mut record.entries {
        if let SessionEntry::User { meta, .. } = entry
            && meta.turn_id.as_deref() == Some(turn_id)
        {
            meta.turn_outcome = Some(outcome);
            meta.turn_ended_at = Some(ended_at);
        }
    }
}

pub(super) fn push_notice(
    state: &mut SessionState,
    turn_id: &str,
    text: &str,
    tone: Option<NoticeTone>,
) {
    state.record.entries.push(SessionEntry::Notice {
        meta: SessionEntryMeta {
            at: Some(Utc::now().timestamp_millis()),
            turn_id: Some(turn_id.to_owned()),
            ..SessionEntryMeta::default()
        },
        text: text.to_owned(),
        detail: None,
        tone,
        cause: None,
    });
    state.dirty = true;
}

/// The settled tool entry: written after execution with `running: false`,
/// the display text and the duration — `onToolResult` (L1576-1593). The
/// running-start row belongs to the streaming surface (c3); persistence
/// sees the settled state.
pub(super) fn settle_tool_entry(
    state: &mut SessionState,
    turn_id: &str,
    name: &str,
    arguments: &Value,
    text: &str,
    at: i64,
    duration_ms: i64,
) {
    state.record.entries.push(SessionEntry::Tool {
        meta: SessionEntryMeta {
            at: Some(at),
            turn_id: Some(turn_id.to_owned()),
            ..SessionEntryMeta::default()
        },
        command: None,
        input: arguments.as_object().cloned(),
        result: text.to_owned(),
        running: Some(false),
        preparing: None,
        tool: Some(name.to_owned()),
        tool_title: None,
        session: None,
        duration_ms: Some(duration_ms),
        outcome: None,
    });
    state.dirty = true;
}

/// One streamed reply grows **one** assistant entry (`appendStreamedText`).
pub(super) fn grow_assistant(
    state: &mut SessionState,
    assistant_index: &mut Option<usize>,
    delta: &str,
    turn_id: &str,
    reply: &mut String,
) {
    match *assistant_index {
        Some(index) => {
            if let SessionEntry::Assistant { text, .. } = &mut state.record.entries[index] {
                text.push_str(delta);
            }
        }
        None => {
            state.record.entries.push(SessionEntry::Assistant {
                meta: SessionEntryMeta {
                    at: Some(Utc::now().timestamp_millis()),
                    turn_id: Some(turn_id.to_owned()),
                    ..SessionEntryMeta::default()
                },
                text: delta.to_owned(),
            });
            *assistant_index = Some(state.record.entries.len() - 1);
        }
    }
    reply.push_str(delta);
    state.dirty = true;
}

/// Thinking grows one reasoning entry, spliced **before** the streamed
/// assistant entry when one exists (`appendStreamedReasoning`); otherwise it
/// appends at the end, and the assistant that follows lands after it.
pub(super) fn grow_reasoning(
    state: &mut SessionState,
    reasoning_index: &mut Option<usize>,
    reasoning_started_at: &mut Option<i64>,
    assistant_index: &mut Option<usize>,
    delta: &str,
    turn_id: &str,
) {
    if let Some(index) = *reasoning_index {
        if let SessionEntry::Reasoning { text, .. } = &mut state.record.entries[index] {
            text.push_str(delta);
        }
        state.dirty = true;
        return;
    }
    let entry = SessionEntry::Reasoning {
        meta: SessionEntryMeta {
            at: Some(Utc::now().timestamp_millis()),
            turn_id: Some(turn_id.to_owned()),
            ..SessionEntryMeta::default()
        },
        text: delta.to_owned(),
        duration_ms: None,
    };
    let position = match *assistant_index {
        Some(index) => index,
        None => state.record.entries.len(),
    };
    state.record.entries.insert(position, entry);
    *reasoning_index = Some(position);
    *reasoning_started_at = Some(Utc::now().timestamp_millis());
    if let Some(assistant) = assistant_index {
        // The splice shifted the assistant one row down.
        *assistant += 1;
    }
    state.dirty = true;
}

/// The block ends at the next assistant token, a tool call, or the stream's
/// end: stamp `durationMs` and clear the handle (`finishStreamedReasoning`).
pub(super) fn finish_reasoning_block(
    state: &mut SessionState,
    reasoning_index: &mut Option<usize>,
    reasoning_started_at: &mut Option<i64>,
) {
    let (Some(index), Some(started_at)) = (*reasoning_index, *reasoning_started_at) else {
        *reasoning_index = None;
        *reasoning_started_at = None;
        return;
    };
    if let SessionEntry::Reasoning { duration_ms, .. } = &mut state.record.entries[index]
        && duration_ms.is_none()
    {
        *duration_ms = Some((Utc::now().timestamp_millis() - started_at).max(0));
        state.dirty = true;
    }
    *reasoning_index = None;
    *reasoning_started_at = None;
}
