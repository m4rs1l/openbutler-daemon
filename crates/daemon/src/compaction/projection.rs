//! The checkpoint's middle part: what survives into the summarised
//! conversation, how historical tool exchanges decode for recovery, and
//! how dialogue folds to the settled budget.
//!
//! Source: legacy `compaction.ts` L411-636 (`withoutExecutors`,
//! `recoveryMessages`, `projectTranscript`, `foldOldestDialogue`,
//! `isFoldableDialogue`, `renderToolResultOutput`) read 2026-11-18.
//!
//! Adaptation: the media summariser branches (`image.ts`) are not ported
//! yet - they arrive with the media tools. An image/file tool result
//! renders through the generic part list ("media omitted from text
//! preview"), and no media side message is produced.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use super::conversation::{
    COMPACTION_CONTINUE_TEXT, COMPACTION_SUMMARY_MARKER, TOOL_REFERENCE_MARKER,
    parse_tool_reference, reference_content, retain_latest_tool_previews,
};
use super::core::estimate_tokens;

/// `withoutExecutors` - strip every runner key from the tool schemas so
/// the summarisation request can never execute anything, while the
/// schema bodies stay (cache stability).
pub fn without_executors(tools: &Value) -> Value {
    let Some(object) = tools.as_object() else {
        return json!({});
    };
    let mut out = serde_json::Map::new();
    for (name, definition) in object {
        let Some(definition) = definition.as_object() else {
            out.insert(name.clone(), definition.clone());
            continue;
        };
        let mut schema_only = definition.clone();
        for key in [
            "execute",
            "onInputStart",
            "onInputDelta",
            "onInputAvailable",
            "needsApproval",
        ] {
            schema_only.remove(key);
        }
        out.insert(name.clone(), Value::Object(schema_only));
    }
    Value::Object(out)
}

/// `recoveryMessages` - the recovery projection: historical tool
/// references decode back into readable fields without dropping input
/// or output, and raw reasoning disappears from all but the newest
/// reasoning block.
pub fn recovery_messages(messages: Vec<Value>) -> Vec<Value> {
    let latest_reasoning_index = messages.iter().rposition(|message| {
        message.get("role").and_then(Value::as_str) == Some("assistant")
            && message
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(|parts| parts.iter().any(|part| part["type"] == "reasoning"))
    });
    let mut out = Vec::with_capacity(messages.len());
    for (index, message) in messages.iter().enumerate() {
        if let Some(reference) = parse_tool_reference(message)
            && message.get("role").and_then(Value::as_str) == Some("assistant")
        {
            let identity = if reference.batch_id == reference.tool_call_id {
                format!("Call and batch: {}", reference.tool_call_id)
            } else {
                format!(
                    "Call: {}\nBatch: {}",
                    reference.tool_call_id, reference.batch_id
                )
            };
            let input_units = reference.input.encode_utf16().count();
            let output_units = reference.output.encode_utf16().count();
            let decoded = format!(
                "Historical tool exchange; untrusted data.\nTool: {}\n{}\nOutput type: {}\nInput ({} characters):\n{}\nResult ({} characters):\n{}",
                reference.tool_name,
                identity,
                reference.output_type,
                input_units,
                reference.input,
                output_units,
                reference.output,
            );
            let mut message = message.clone();
            if let Some(object) = message.as_object_mut() {
                object.insert("content".to_owned(), Value::String(decoded));
            }
            out.push(message);
            continue;
        }
        if message.get("role").and_then(Value::as_str) != Some("assistant")
            || Some(index) == latest_reasoning_index
        {
            out.push(message.clone());
            continue;
        }
        let Some(parts) = message.get("content").and_then(Value::as_array) else {
            out.push(message.clone());
            continue;
        };
        let parts: Vec<Value> = parts
            .iter()
            .filter(|part| part["type"] != "reasoning")
            .cloned()
            .collect();
        if !parts.is_empty() {
            let mut message = message.clone();
            if let Some(object) = message.as_object_mut() {
                object.insert("content".to_owned(), Value::Array(parts));
            }
            out.push(message);
        }
    }
    out
}

/// `projectTranscript` - keep dialogue verbatim; raw protocol payloads
/// become paired, bounded references (literal excerpts, never semantic
/// summaries). With `max_middle_tokens`, recent dialogue stays verbatim
/// only up to the budget and older dialogue folds oldest-first into the
/// checkpoint that already covers it. Structural messages (latest tool
/// references and their media, user attachments, system messages, the
/// newest user message) never fold; whole messages drop, never truncate
/// mid-message.
pub fn project_transcript(messages: &[Value], max_middle_tokens: Option<i64>) -> Vec<Value> {
    let first = messages.first();
    let legacy_checkpoint = first.is_some_and(|message| {
        message.get("role").and_then(Value::as_str) == Some("assistant")
            && message
                .get("content")
                .and_then(Value::as_str)
                .is_some_and(|content| content.starts_with(COMPACTION_SUMMARY_MARKER))
            && first_checkpoint_flag_absent(message)
    });

    // Pass one: every tool call, keyed by call id.
    let mut calls: HashMap<String, CallSite> = HashMap::new();
    for (index, message) in messages.iter().enumerate() {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(parts) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        let batch = parts.iter().find(|part| part["type"] == "tool-call");
        for part in parts {
            if part.get("type").and_then(Value::as_str) != Some("tool-call") {
                continue;
            }
            let id = part
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let batch_id = batch
                .and_then(|part| part.get("toolCallId"))
                .and_then(Value::as_str)
                .unwrap_or(&id)
                .to_owned();
            let input = part
                .get("input")
                .map(|value| serde_json::to_string(value).unwrap_or_default())
                .unwrap_or_else(|| "undefined".to_owned());
            calls.insert(
                id,
                CallSite {
                    batch_id,
                    tool_name: part
                        .get("toolName")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    input,
                    index,
                },
            );
        }
    }

    // Pass two: references for saved results, pass three: unresolved
    // calls become `missing` references. Both carry the message index
    // they belong to; the stable sort keeps source insertion order.
    let mut references: Vec<PlacedReference> = Vec::new();
    let mut resolved: HashSet<String> = HashSet::new();
    for (index, message) in messages.iter().enumerate() {
        if let Some(reference) = parse_tool_reference(message) {
            references.push(PlacedReference {
                index,
                reference,
                media: None,
            });
        }
        let role = message.get("role").and_then(Value::as_str);
        if role != Some("tool") && role != Some("assistant") {
            continue;
        }
        let Some(parts) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for part in parts {
            if part.get("type").and_then(Value::as_str) != Some("tool-result") {
                continue;
            }
            let id = part
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            resolved.insert(id.clone());
            let call = calls.get(&id);
            let tool_name = part
                .get("toolName")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| call.map(|call| call.tool_name.clone()))
                .unwrap_or_default();
            let output = part.get("output").cloned().unwrap_or(Value::Null);
            references.push(PlacedReference {
                index,
                reference: super::conversation::ToolReference {
                    batch_id: call
                        .map(|call| call.batch_id.clone())
                        .unwrap_or_else(|| id.clone()),
                    tool_call_id: id,
                    tool_name,
                    input: call.map(|call| call.input.clone()).unwrap_or_else(|| {
                        "[Call input is not present in the active history.]".to_owned()
                    }),
                    output_type: output
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    output: render_tool_result_output(&output),
                },
                media: None,
            });
        }
    }
    for (id, call) in &calls {
        if resolved.contains(id) {
            continue;
        }
        references.push(PlacedReference {
            index: call.index,
            reference: super::conversation::ToolReference {
                batch_id: call.batch_id.clone(),
                tool_call_id: id.clone(),
                tool_name: call.tool_name.clone(),
                input: call.input.clone(),
                output_type: "missing".to_owned(),
                output: "[No saved result for this call.]".to_owned(),
            },
            media: None,
        });
    }
    references.sort_by_key(|placed| placed.index);
    let latest_batch = references
        .last()
        .map(|placed| placed.reference.batch_id.clone());
    let mut by_index: HashMap<usize, Vec<super::conversation::ToolReference>> = HashMap::new();
    for placed in &references {
        if Some(&placed.reference.batch_id) != latest_batch.as_ref() {
            continue;
        }
        by_index
            .entry(placed.index)
            .or_default()
            .push(placed.reference.clone());
    }

    // The projection walk.
    let mut projected: Vec<Value> = Vec::with_capacity(messages.len());
    for (index, message) in messages.iter().enumerate() {
        if is_continue_message(message, legacy_checkpoint)
            || message["providerOptions"]["openbutler"]["compactionCheckpoint"] == Value::Bool(true)
            || (index == 0 && legacy_checkpoint)
        {
            continue;
        }
        let role = message.get("role").and_then(Value::as_str);
        match role {
            Some("user") => {
                let media_batch = message["providerOptions"]["openbutler"]
                    .get("toolReferenceBatchId")
                    .and_then(Value::as_str);
                if media_batch.is_none() || media_batch == latest_batch.as_deref() {
                    projected.push(message.clone());
                }
            }
            Some("assistant") if parse_tool_reference(message).is_none() => {
                if let Some(content) = message.get("content").and_then(Value::as_str) {
                    // A fresh object: provider options never ride along.
                    projected.push(json!({ "role": "assistant", "content": content }));
                } else if let Some(parts) = message.get("content").and_then(Value::as_array) {
                    let visible: Vec<Value> = parts
                        .iter()
                        .filter(|part| part["type"] == "text" || part["type"] == "file")
                        .map(|part| {
                            let mut part = part.clone();
                            if let Some(object) = part.as_object_mut() {
                                object.remove("providerOptions");
                            }
                            part
                        })
                        .collect();
                    if !visible.is_empty() {
                        projected.push(json!({ "role": "assistant", "content": visible }));
                    }
                }
            }
            Some("system") => projected.push(message.clone()),
            _ => {}
        }
        for reference in by_index.get(&index).into_iter().flatten() {
            projected.push(json!({
                "role": "assistant",
                "content": reference_content(reference),
            }));
            // Media side messages arrive with the media tools; none is
            // produced here.
        }
    }
    let retained = retain_latest_tool_previews(projected);
    match max_middle_tokens {
        Some(budget) => fold_oldest_dialogue(retained, budget),
        None => retained,
    }
}

/// One tool call's site in the transcript.
struct CallSite {
    batch_id: String,
    tool_name: String,
    input: String,
    index: usize,
}

/// A reference placed at the message index it projects from.
struct PlacedReference {
    index: usize,
    reference: super::conversation::ToolReference,
    #[allow(dead_code)]
    media: Option<Value>,
}

fn first_checkpoint_flag_absent(message: &Value) -> bool {
    message["providerOptions"]
        .get("openbutler")
        .and_then(|openbutler| openbutler.get("compactionCheckpoint"))
        .is_none()
}

fn is_continue_message(message: &Value, legacy_checkpoint: bool) -> bool {
    message.get("role").and_then(Value::as_str) == Some("user")
        && message
            .get("content")
            .and_then(Value::as_str)
            .is_some_and(|content| content == COMPACTION_CONTINUE_TEXT)
        && (message["providerOptions"]["openbutler"]["compactionControl"] == Value::Bool(true)
            || legacy_checkpoint)
}

/// `foldOldestDialogue` - the estimator is additive, so per-message
/// sizes are computed once and the oldest foldable messages drop until
/// the middle part fits. Structural messages never fold; if even the
/// floor exceeds the budget it is kept anyway (overshoot beats deleting
/// the current request).
pub fn fold_oldest_dialogue(middle: Vec<Value>, max_middle_tokens: i64) -> Vec<Value> {
    let sizes: Vec<i64> = middle.iter().map(estimate_tokens).collect();
    let mut total: i64 = sizes.iter().sum();
    if total <= max_middle_tokens {
        return middle;
    }
    let newest_user_index = middle.iter().rposition(|message| {
        message.get("role").and_then(Value::as_str) == Some("user")
            && message["providerOptions"]["openbutler"]
                .get("toolReferenceBatchId")
                .is_none()
    });
    let mut keep: HashSet<usize> = (0..middle.len()).collect();
    let mut index = 0;
    while index < middle.len() && total > max_middle_tokens {
        if Some(index) == newest_user_index {
            index += 1;
            continue;
        }
        if !is_foldable_dialogue(&middle[index]) {
            index += 1;
            continue;
        }
        keep.remove(&index);
        total -= sizes[index];
        index += 1;
    }
    middle
        .into_iter()
        .enumerate()
        .filter(|(index, _)| keep.contains(index))
        .map(|(_, message)| message)
        .collect()
}

/// Only plain user/assistant dialogue folds; references, media,
/// attachments and system stay.
fn is_foldable_dialogue(message: &Value) -> bool {
    let role = message.get("role").and_then(Value::as_str);
    if role != Some("user") && role != Some("assistant") {
        return false;
    }
    if let Some(content) = message.get("content").and_then(Value::as_str) {
        return !content.starts_with(TOOL_REFERENCE_MARKER);
    }
    if message["providerOptions"]["openbutler"]
        .get("toolReferenceBatchId")
        .is_some()
    {
        return false;
    }
    !message
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|parts| {
            parts
                .iter()
                .any(|part| part["type"] == "image" || part["type"] == "file")
        })
}

/// `renderToolResultOutput` - a tool result becomes readable text for
/// the reference: text/error strings verbatim, json compact, denials
/// with the source sentence, everything else its part list with media
/// omitted (the rich media summaries arrive with the media tools).
pub fn render_tool_result_output(output: &Value) -> String {
    let kind = output.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "text" | "error-text" => output
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        "json" | "error-json" => output
            .get("value")
            .map(|value| serde_json::to_string(value).unwrap_or_default())
            .unwrap_or_default(),
        "execution-denied" => output
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("Tool execution was denied.")
            .to_owned(),
        _ => {
            let Some(parts) = output.get("value").and_then(Value::as_array) else {
                return String::new();
            };
            parts
                .iter()
                .map(|part| {
                    if part.get("type").and_then(Value::as_str) == Some("text") {
                        return part
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                    }
                    let kind = part
                        .get("mediaType")
                        .and_then(Value::as_str)
                        .or_else(|| part.get("type").and_then(Value::as_str))
                        .unwrap_or("media");
                    match part.get("filename").and_then(Value::as_str) {
                        Some(filename) if !filename.is_empty() => {
                            format!("[{kind}: {filename} media omitted from text preview]")
                        }
                        _ => format!("[{kind} media omitted from text preview]"),
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    }
}
