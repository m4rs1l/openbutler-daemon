//! Greedy transcript chunking for the sliced fallback - the piece that
//! keeps a tool call and its result inside one slice.
//!
//! Source: legacy `compaction.ts` L798-840 (`splitIntoSlices`,
//! `messageToolCallIds`, `messageToolResultIds`) read 2026-11-18. The
//! AI SDK rejects a request whose assistant tool call has no tool-result
//! in the same request (`AI_MissingToolResultsError`) before anything
//! reaches the provider - that is what killed every naive live cut on
//! 2026-11-03 when the boundary landed between call and result. A slice
//! therefore closes only while no call inside it is still unresolved, so
//! the pair always travels together even when that pushes the slice past
//! the budget hint.
//!
//! Returns `None` when a single message cannot fit a slice at all - the
//! caller reports it as data, never as an exception.

use serde_json::Value;

use super::core::estimate_tokens;

/// `splitIntoSlices` - greedy chunking that never splits a tool call
/// from its result.
pub fn split_into_slices(
    messages: &[Value],
    slice_budget: i64,
    hard_cap: i64,
) -> Option<Vec<Vec<Value>>> {
    let mut slices: Vec<Vec<Value>> = Vec::new();
    let mut current: Vec<Value> = Vec::new();
    let mut size = 0_i64;
    let mut open: Vec<String> = Vec::new();
    for message in messages {
        let message_size = estimate_tokens(message);
        let oversized = message_size > slice_budget;
        if oversized && message_size > hard_cap {
            return None;
        }
        if !current.is_empty()
            && open.is_empty()
            && (oversized || size + message_size > slice_budget)
        {
            slices.push(std::mem::take(&mut current));
            size = 0;
        }
        current.push(message.clone());
        size += message_size;
        for call_id in message_tool_call_ids(message) {
            if !open.contains(&call_id) {
                open.push(call_id);
            }
        }
        for result_id in message_tool_result_ids(message) {
            open.retain(|id| id != &result_id);
        }
    }
    if !current.is_empty() {
        slices.push(current);
    }
    Some(slices)
}

/// Tool-call ids an assistant message is still waiting on results for.
fn message_tool_call_ids(message: &Value) -> Vec<String> {
    let Some(parts) = message.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    parts
        .iter()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("tool-call"))
        .filter_map(|part| part.get("toolCallId").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

/// Tool-result ids a tool message delivers for previously issued calls.
fn message_tool_result_ids(message: &Value) -> Vec<String> {
    if message.get("role").and_then(Value::as_str) != Some("tool") {
        return Vec::new();
    }
    let Some(parts) = message.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    parts
        .iter()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("tool-result"))
        .filter_map(|part| part.get("toolCallId").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}
