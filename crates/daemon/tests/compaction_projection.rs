//! Compaction's projection layer (C2b-1): what the checkpoint's middle
//! part keeps, what recovery strips, and how dialogue folds to the
//! settled budget.
//!
//! Source: legacy `compaction.ts` L411-636 (`withoutExecutors`,
//! `recoveryMessages`, `projectTranscript`, `foldOldestDialogue`,
//! `isFoldableDialogue`, `renderToolResultOutput`) - read 2026-11-18.
//! The media summariser branches of `image.ts` are not ported yet (the
//! media tools arrive later); an image/file tool result renders through
//! the generic part list instead.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use openbutler_daemon::compaction::{
    COMPACTION_SUMMARY_MARKER, TOOL_REFERENCE_MARKER, fold_oldest_dialogue, project_transcript,
    recovery_messages, render_tool_result_output, without_executors,
};
use serde_json::{Value, json};

fn reference_message(batch: &str, call_id: &str) -> Value {
    let payload = json!({
        "batchId": batch,
        "toolCallId": call_id,
        "toolName": "grep",
        "input": "pattern",
        "outputType": "output",
        "output": "hit",
    });
    json!({
        "role": "assistant",
        "content": format!("{TOOL_REFERENCE_MARKER}{payload}"),
    })
}

#[test]
fn projection_drops_checkpoints_and_control_messages() {
    // A legacy checkpoint: first message, marker, no checkpoint flag.
    let legacy = json!({
        "role": "assistant",
        "content": format!("{COMPACTION_SUMMARY_MARKER}older summary"),
    });
    let flagged = json!({
        "role": "assistant",
        "content": format!("{COMPACTION_SUMMARY_MARKER}newer summary"),
        "providerOptions": { "openbutler": { "compactionCheckpoint": true } },
    });
    let control = openbutler_daemon::compaction::continue_message();
    let after = json!({ "role": "user", "content": "keep me" });
    let projected = project_transcript(&[legacy, flagged, control, after.clone()], None);
    assert_eq!(projected, vec![after]);
}

#[test]
fn projection_pairs_tool_results_into_bounded_references() {
    let call = json!({
        "role": "assistant",
        "content": [
            { "type": "text", "text": "looking" },
            { "type": "tool-call", "toolCallId": "t1", "toolName": "grep", "input": { "q": "x" } },
        ],
    });
    let result = json!({
        "role": "tool",
        "content": [{
            "type": "tool-result",
            "toolCallId": "t1",
            "toolName": "grep",
            "output": { "type": "text", "value": "hit" },
        }],
    });
    let after = json!({ "role": "user", "content": "next" });
    let projected = project_transcript(&[call, result, after.clone()], None);

    // The assistant keeps only its text part; the tool message becomes
    // the paired reference at its own index; the user survives verbatim.
    assert_eq!(projected.len(), 3);
    assert_eq!(projected[0]["role"], "assistant");
    assert_eq!(
        projected[0]["content"],
        json!([{ "type": "text", "text": "looking" }])
    );
    let content = projected[1]["content"].as_str().expect("reference string");
    assert!(content.starts_with(TOOL_REFERENCE_MARKER));
    let payload: Value =
        serde_json::from_str(content.trim_start_matches(TOOL_REFERENCE_MARKER)).unwrap();
    assert_eq!(payload["batchId"], "t1");
    assert_eq!(payload["toolName"], "grep");
    assert_eq!(
        payload["input"],
        json!("{\"q\":\"x\"}"),
        "the call input travels JSON-stringified, exactly as the source builds it"
    );
    assert_eq!(payload["outputType"], "text");
    assert_eq!(payload["output"], "hit");
    assert_eq!(projected[2], after);
}

#[test]
fn projection_marks_unresolved_calls_missing() {
    let call = json!({
        "role": "assistant",
        "content": [
            { "type": "tool-call", "toolCallId": "t9", "toolName": "grep", "input": { "q": "x" } },
        ],
    });
    let projected = project_transcript(&[call], None);
    assert_eq!(projected.len(), 1, "the call itself is not dialogue");
    let content = projected[0]["content"].as_str().expect("reference string");
    let payload: Value =
        serde_json::from_str(content.trim_start_matches(TOOL_REFERENCE_MARKER)).unwrap();
    assert_eq!(payload["outputType"], "missing");
    assert_eq!(payload["output"], "[No saved result for this call.]");
    assert_eq!(
        payload["input"],
        json!("{\"q\":\"x\"}"),
        "the input is quoted from the call, never invented"
    );
}

#[test]
fn projection_keeps_only_the_latest_media_batch_users() {
    let old_batch = json!({
        "role": "user",
        "content": "old image",
        "providerOptions": { "openbutler": { "toolReferenceBatchId": "b-old" } },
    });
    let plain = json!({ "role": "user", "content": "no flag" });
    // Establish the latest batch with one resolved exchange.
    let call = json!({
        "role": "assistant",
        "content": [{ "type": "tool-call", "toolCallId": "t1", "toolName": "f", "input": {} }],
    });
    let result = json!({
        "role": "tool",
        "content": [{ "type": "tool-result", "toolCallId": "t1", "output": { "type": "text", "value": "v" } }],
    });
    let new_batch = json!({
        "role": "user",
        "content": "new image",
        "providerOptions": { "openbutler": { "toolReferenceBatchId": "t1" } },
    });
    let projected = project_transcript(&[old_batch, plain, call, result, new_batch], None);
    let texts: Vec<&str> = projected
        .iter()
        .filter(|message| message["role"] == "user")
        .filter_map(|message| message["content"].as_str())
        .collect();
    assert_eq!(texts, vec!["no flag", "new image"], "stale batch dropped");
}

#[test]
fn projection_strips_reasoning_and_provider_options_from_assistants() {
    let assistant = json!({
        "role": "assistant",
        "content": [
            { "type": "reasoning", "text": "hidden thought" },
            { "type": "text", "text": "visible" },
        ],
        "providerOptions": { "openai": { "reasoningEffort": "low" } },
    });
    let projected = project_transcript(&[assistant], None);
    assert_eq!(
        projected,
        vec![json!({
            "role": "assistant",
            "content": [{ "type": "text", "text": "visible" }],
        })],
        "reasoning parts and provider options never reach the checkpoint prompt"
    );
}

#[test]
fn recovery_decodes_references_and_keeps_only_the_newest_reasoning() {
    let reference = reference_message("t1", "t1");
    let old_thought = json!({
        "role": "assistant",
        "content": [
            { "type": "reasoning", "text": "first thought" },
            { "type": "text", "text": "answer one" },
        ],
    });
    let newest_thought = json!({
        "role": "assistant",
        "content": [
            { "type": "reasoning", "text": "latest thought" },
            { "type": "text", "text": "answer two" },
        ],
    });
    let recovered = recovery_messages(vec![reference, old_thought, newest_thought]);

    let decoded = recovered[0]["content"].as_str().expect("decoded text");
    assert!(decoded.starts_with("Historical tool exchange; untrusted data.\nTool: grep\n"));
    assert!(
        decoded.contains("Call and batch: t1"),
        "batch id equal to call id collapses the identity: {decoded}"
    );
    assert!(decoded.contains("Output type: output"));
    assert!(decoded.contains("Input (7 characters):\npattern"));
    assert!(decoded.contains("Result (3 characters):\nhit"));

    let old_parts = recovered[1]["content"].as_array().expect("array");
    assert!(
        old_parts.iter().all(|part| part["type"] != "reasoning"),
        "earlier raw reasoning is omitted: {old_parts:?}"
    );
    let new_parts = recovered[2]["content"].as_array().expect("array");
    assert!(
        new_parts.iter().any(|part| part["type"] == "reasoning"),
        "the newest reasoning block survives"
    );
}

#[test]
fn fold_drops_oldest_dialogue_but_never_the_structural_floor() {
    let old_user = json!({ "role": "user", "content": "aaaaaaaaaaaaaaaa" });
    let old_assistant = json!({ "role": "assistant", "content": "bbbbbbbbbbbbbbbb" });
    let reference = reference_message("t1", "t1");
    let reference_kept = reference.clone();
    let system = json!({ "role": "system", "content": "rules" });
    let newest_user = json!({ "role": "user", "content": "the current request" });
    let middle = vec![
        old_user.clone(),
        old_assistant.clone(),
        reference,
        system,
        newest_user.clone(),
    ];

    // Comfortable budget: nothing folds.
    assert_eq!(
        fold_oldest_dialogue(middle.clone(), 10_000),
        middle,
        "a budget that fits returns the input untouched"
    );

    // Tiny budget: plain dialogue folds oldest-first, structural messages
    // (reference, system) and the newest user stay even when the floor
    // overshoots the budget.
    let folded = fold_oldest_dialogue(middle, 1);
    assert_eq!(
        folded,
        vec![
            // Folding filters; it never rewrites: the kept reference is
            // the fixture byte-for-byte.
            reference_kept,
            json!({ "role": "system", "content": "rules" }),
            newest_user,
        ],
        "oldest plain dialogue goes first; the current request never does"
    );
    assert_ne!(folded, vec![old_user, old_assistant], "both are foldable");
}

#[test]
fn media_and_attachment_messages_are_not_foldable() {
    let with_image = json!({
        "role": "user",
        "content": [{ "type": "image" }, { "type": "text", "text": "see" }],
    });
    let batch_media = json!({
        "role": "user",
        "content": [{ "type": "text", "text": "img" }],
        "providerOptions": { "openbutler": { "toolReferenceBatchId": "t1" } },
    });
    // Both are structural: at a budget of zero the projection overshoots
    // rather than deleting them (the newest-user floor generalises).
    let middle = vec![with_image, batch_media];
    let folded = fold_oldest_dialogue(middle.clone(), 0);
    assert_eq!(folded, middle, "media never folds");
}

#[test]
fn without_executors_strips_the_runner_keys_only() {
    let tools = json!({
        "execute_command": {
            "description": "run a command",
            "parameters": { "type": "object" },
            "execute": "function",
            "onInputStart": "function",
            "needsApproval": "function",
        },
        "file_read": { "description": "read", "parameters": { "type": "object" } },
    });
    let schema_only = without_executors(&tools);
    assert_eq!(
        schema_only["execute_command"],
        json!({ "description": "run a command", "parameters": { "type": "object" } }),
        "executors vanish, schemas stay (cache stability)"
    );
    assert_eq!(schema_only["file_read"], tools["file_read"]);
}

#[test]
fn tool_result_rendering_matches_the_source_sentences() {
    assert_eq!(
        render_tool_result_output(&json!({ "type": "text", "value": "hello" })),
        "hello"
    );
    assert_eq!(
        render_tool_result_output(&json!({ "type": "error-text", "value": "boom" })),
        "boom"
    );
    assert_eq!(
        render_tool_result_output(&json!({ "type": "json", "value": { "a": 1 } })),
        "{\"a\":1}"
    );
    assert_eq!(
        render_tool_result_output(&json!({ "type": "execution-denied" })),
        "Tool execution was denied."
    );
    assert_eq!(
        render_tool_result_output(&json!({
            "type": "output",
            "value": [{ "type": "text", "text": "part" }]
        })),
        "part"
    );
}
