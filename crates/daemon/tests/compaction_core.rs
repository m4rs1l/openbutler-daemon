//! Compaction's pure core (C1): prompt bytes, trigger arithmetic, the
//! conservative estimators, and the conversation-projection helpers.
//!
//! Source: legacy `compaction.ts` (L19-183 constants/prompt/trigger,
//! L636-683 estimators) and `conversation.ts` (read 2026-11-18). No model
//! call exists in this module - the streaming engine arrives with the
//! sidecar wiring. Prompt files were extracted mechanically from the
//! TypeScript template literals; their sha256 pins byte-fidelity.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use openbutler_daemon::compaction::{
    COMPACTION_PROMPT, COMPACTION_PROMPT_TOKENS, COMPACTION_SUMMARY_MARKER,
    COMPACTION_SUMMARY_MAX_TOKENS, SLICE_MERGE_INSTRUCTION, SLICE_SYSTEM_PROMPT,
    TOOL_REFERENCE_MARKER, append_transcript_archive_reference, auto_compaction_trigger_tokens,
    continue_message, estimate_context_tokens, estimate_text_tokens, estimate_tokens,
    parse_tool_reference, repair_queued_message_order, retain_latest_tool_previews,
    retire_covered_tool_previews, truncate_head_and_tail,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn sha256_hex(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

// ---------------------------------------------------------------- prompts

#[test]
fn prompt_files_are_byte_exact_extractions() {
    // Fingerprints come from the extraction script that evaluated the
    // TypeScript literals as JavaScript would - these pins fail if the
    // files drift from the source or CRLF ever touches them.
    assert_eq!(COMPACTION_PROMPT.len(), 6315);
    assert_eq!(
        sha256_hex(COMPACTION_PROMPT),
        "b22f42cb68a55245ac016d81332ee91b415d58b3c7c22b52491e2ef4bdccd9b1"
    );
    assert!(COMPACTION_PROMPT.starts_with(
        "Create a detailed, self-contained continuation checkpoint for another instance of the same agent."
    ));
    assert!(COMPACTION_PROMPT.ends_with("Return only the continuation checkpoint."));
    assert!(!COMPACTION_PROMPT.contains('\r'));

    assert_eq!(SLICE_SYSTEM_PROMPT.len(), 721);
    assert_eq!(
        sha256_hex(SLICE_SYSTEM_PROMPT),
        "045c321ca122b85a8016931d0b5276ff4a99cc42ab14ad2b54a591829b91c856"
    );
    assert!(
        SLICE_SYSTEM_PROMPT
            .starts_with("You are condensing ONE slice of an agent transcript into durable notes")
    );
    assert!(!SLICE_SYSTEM_PROMPT.contains('\r'));

    assert_eq!(SLICE_MERGE_INSTRUCTION.len(), 235);
    assert_eq!(
        sha256_hex(SLICE_MERGE_INSTRUCTION),
        "5b2402a7e66c7feefd01033c4f09825e843946fa662c599bcf15fbe7e262e614"
    );
    assert!(SLICE_MERGE_INSTRUCTION.ends_with("Notes only."));
    assert!(!SLICE_MERGE_INSTRUCTION.contains('\r'));

    // The default prompt token count is derived from the prompt itself:
    // 6,315 ASCII characters / 4, rounded up.
    assert_eq!(COMPACTION_PROMPT_TOKENS, 1579);
}

#[test]
fn markers_and_continue_message_are_byte_exact() {
    assert_eq!(
        COMPACTION_SUMMARY_MARKER,
        "OpenButler conversation summary (context only):\n"
    );
    assert_eq!(
        TOOL_REFERENCE_MARKER,
        "OpenButler tool exchange (raw context preview):\n"
    );
    let message = continue_message();
    assert_eq!(message["role"], "user");
    assert_eq!(
        message["content"],
        "Continue the unfinished user request from the compacted history. Use the summary to recover the current goal, constraints, completed work, and next action."
    );
    assert_eq!(
        message["providerOptions"]["openbutler"]["compactionControl"],
        true
    );
}

// --------------------------------------------------------------- trigger

#[test]
fn trigger_is_arithmetic_not_a_share_of_the_window() {
    // reserve = prompt + summary ceiling + margin + measured growth p99
    // (1_000 + 26_024 + 1_024 + 11_583 = 39_631), floor never binds here.
    assert_eq!(auto_compaction_trigger_tokens(200_000, 1_000), 160_369);

    // With the real prompt: reserve = 40_210, so the 0.5 floor binds
    // below 80_420 tokens (the derivation the source comment records).
    assert_eq!(
        auto_compaction_trigger_tokens(80_418, COMPACTION_PROMPT_TOKENS),
        40_209
    );
    assert_eq!(
        auto_compaction_trigger_tokens(80_420, COMPACTION_PROMPT_TOKENS),
        40_210
    );
    assert_eq!(
        auto_compaction_trigger_tokens(80_422, COMPACTION_PROMPT_TOKENS),
        40_212
    );

    // Tiny window: the floor wins outright, never a negative trigger.
    assert_eq!(
        auto_compaction_trigger_tokens(10_000, COMPACTION_PROMPT_TOKENS),
        5_000
    );

    // No upper clamp: a large window stays proportional to its reserve.
    assert_eq!(
        auto_compaction_trigger_tokens(1_000_000, COMPACTION_PROMPT_TOKENS),
        959_790
    );

    // The reserve has no output-limit term.
    assert_eq!(COMPACTION_SUMMARY_MAX_TOKENS, 26_024);
}

// ------------------------------------------------------------- estimators

#[test]
fn text_estimator_counts_every_non_ascii_code_point_as_one_token() {
    assert_eq!(estimate_text_tokens(""), 0);
    assert_eq!(estimate_text_tokens("hello"), 2); // ceil(5/4)
    assert_eq!(estimate_text_tokens("abcd"), 1);
    assert_eq!(estimate_text_tokens("abcde"), 2);
    assert_eq!(estimate_text_tokens("日本語"), 3); // wide only
    assert_eq!(estimate_text_tokens("aé"), 2); // ceil(1/4) + 1 wide
    assert_eq!(estimate_text_tokens("🎉"), 1); // one code point
}

#[test]
fn token_estimator_walks_json_values_conservatively() {
    assert_eq!(estimate_tokens(&json!("hello")), 2);
    assert_eq!(estimate_tokens(&Value::Null), 1);
    assert_eq!(estimate_tokens(&json!(true)), 1);
    assert_eq!(estimate_tokens(&json!(42)), 1);
    assert_eq!(estimate_tokens(&json!([])), 0); // arrays sum, empty = zero
    assert_eq!(estimate_tokens(&json!(["ab", "cd"])), 2);
    assert_eq!(estimate_tokens(&json!({ "a": "x" })), 5); // object base 4 + 1
    // Media parts cost a flat block before their fields are walked.
    assert_eq!(
        estimate_tokens(&json!({ "type": "image", "data": "anything-huge" })),
        1024
    );
    assert_eq!(estimate_tokens(&json!({ "type": "file" })), 1024);
    assert_eq!(
        estimate_tokens(&json!({ "type": "image-url", "url": "http://x/y" })),
        1024
    );
}

#[test]
fn context_estimate_sums_system_messages_and_tools() {
    // system "abcd" = 1, one message "abcd" = 1, empty tools object = 4.
    assert_eq!(
        estimate_context_tokens("abcd", &[json!("abcd")], &json!({})),
        6
    );
}

// ------------------------------------------------- conversation helpers

#[test]
fn archive_reference_appends_exactly_once() {
    let content = "checkpoint summary";
    let once = append_transcript_archive_reference(content, "/data/sessions/a/a.json");
    assert_eq!(
        once,
        "checkpoint summary\n\nOpenButler transcript archive (original messages and tool results): /data/sessions/a/a.json"
    );
    let twice = append_transcript_archive_reference(&once, "/data/sessions/a/a.json");
    assert_eq!(twice, once);
}

fn tool_reference_message(batch: &str, call_id: &str, input: &str, output: &str) -> Value {
    let payload = json!({
        "batchId": batch,
        "toolCallId": call_id,
        "toolName": "execute_command",
        "input": input,
        "outputType": "output",
        "output": output,
    });
    json!({
        "role": "assistant",
        "content": format!("{TOOL_REFERENCE_MARKER}{payload}"),
    })
}

#[test]
fn tool_reference_parses_only_complete_string_records() {
    let message = tool_reference_message("b1", "c1", "echo hi", "hi\n");
    let parsed = parse_tool_reference(&message).expect("valid reference");
    assert_eq!(parsed.batch_id, "b1");
    assert_eq!(parsed.tool_call_id, "c1");
    assert_eq!(parsed.tool_name, "execute_command");
    assert_eq!(parsed.input, "echo hi");
    assert_eq!(parsed.output_type, "output");
    assert_eq!(parsed.output, "hi\n");

    // Wrong role, missing marker, invalid JSON, non-object payload and a
    // non-string field all refuse.
    let mut wrong_role = message.clone();
    wrong_role["role"] = json!("user");
    assert!(parse_tool_reference(&wrong_role).is_none());
    assert!(
        parse_tool_reference(&json!({
            "role": "assistant",
            "content": "plain text"
        }))
        .is_none()
    );
    assert!(
        parse_tool_reference(&json!({
            "role": "assistant",
            "content": format!("{TOOL_REFERENCE_MARKER}not json")
        }))
        .is_none()
    );
    assert!(
        parse_tool_reference(&json!({
            "role": "assistant",
            "content": format!("{TOOL_REFERENCE_MARKER}[]")
        }))
        .is_none()
    );
    let mut bad_field = tool_reference_message("b1", "c1", "a", "b");
    let stripped =
        bad_field["content"]
            .as_str()
            .unwrap()
            .replacen("\"batchId\":\"b1\"", "\"batchId\":7", 1);
    bad_field["content"] = json!(stripped);
    assert!(parse_tool_reference(&bad_field).is_none());
}

#[test]
fn retain_latest_tool_previews_bounds_the_newest_batch_only() {
    let old = tool_reference_message("old", "c0", "a", "b");
    let new_a = tool_reference_message("new", "c1", "a", "b");
    let new_b = tool_reference_message("new", "c2", "a", "b");
    let messages = vec![old.clone(), new_a.clone(), new_b.clone()];
    let retained = retain_latest_tool_previews(messages.clone());
    // The old batch is dropped, the newest stays (identity here: its
    // fields fit inside the per-field limit).
    assert_eq!(retained.len(), 2);
    assert_eq!(retained, vec![new_a, new_b]);

    // No references at all: the conversation is returned untouched.
    let plain = vec![json!({ "role": "user", "content": "hi" })];
    assert_eq!(retain_latest_tool_previews(plain.clone()), plain);

    // Five references in the newest batch share the batch budget:
    // floor(32_768 / (5 * 2)) = 3_276 per field, below the 8_192 cap.
    let big = "x".repeat(4_000);
    let five: Vec<Value> = (0..5)
        .map(|index| tool_reference_message("b", &format!("c{index}"), &big, &big))
        .collect();
    let bounded = retain_latest_tool_previews(five);
    for message in &bounded {
        let parsed = parse_tool_reference(message).expect("reference survives");
        assert_eq!(parsed.input.chars().count(), 3_276);
        assert_eq!(parsed.output.chars().count(), 3_276);
        assert!(parsed.input.contains("[middle truncated]"));
    }
}

#[test]
fn truncate_head_and_tail_keeps_both_ends_within_the_limit() {
    assert_eq!(truncate_head_and_tail("anything", 0), "");
    assert_eq!(truncate_head_and_tail("short", 10), "short");
    let long: String = (0..100)
        .map(|index| char::from(b'0' + index % 10))
        .collect();
    let cut = truncate_head_and_tail(&long, 40);
    // 24-unit marker, 16 remaining, head = ceil(16 * 0.75) = 12, tail = 4.
    assert_eq!(cut.chars().count(), 40);
    assert!(cut.contains("\n… [middle truncated] …\n"));
    assert!(cut.starts_with("012345678901"));
    assert!(cut.ends_with("6789"));
    // A limit at or below the marker keeps the marker's head only.
    let tiny = truncate_head_and_tail(&long, 10);
    assert_eq!(tiny, "\n… [middle");
}

#[test]
fn retire_covered_previews_finishes_only_committed_checkpoints() {
    let checkpoint = json!({
        "role": "assistant",
        "content": format!("{COMPACTION_SUMMARY_MARKER}old summary"),
        "providerOptions": { "openbutler": { "compactionCheckpoint": true } },
    });
    let reference = tool_reference_message("b", "c1", "a", "b");
    let control = continue_message();
    let after = json!({ "role": "user", "content": "next" });
    let messages = vec![checkpoint, reference, control, after.clone()];

    // Nothing oversized to bound: the projection is already finished and
    // the original list comes back untouched.
    assert_eq!(retire_covered_tool_previews(messages.clone()), messages);

    // No continue boundary: nothing is retired.
    let mut no_boundary = messages.clone();
    no_boundary.remove(2); // the control message itself
    assert_eq!(
        retire_covered_tool_previews(no_boundary.clone()),
        no_boundary
    );
}

#[test]
fn repair_moves_queued_users_to_the_tool_batch_boundary() {
    let call = json!({
        "role": "assistant",
        "content": [{ "type": "tool-call", "toolCallId": "t1", "toolName": "grep", "input": {} }],
    });
    let queued = json!({
        "role": "user",
        "content": "steer",
        "providerOptions": { "openbutler": { "queuedMessageId": "m9" } },
    });
    let result = json!({
        "role": "tool",
        "content": [{ "type": "tool-result", "toolCallId": "t1" }],
    });
    let messages = vec![call, queued, result];
    let repaired = repair_queued_message_order(messages);
    // The queued user jumps in front of the assistant that opened the
    // unresolved batch; nothing is invented or dropped.
    assert_eq!(
        repaired
            .iter()
            .map(|message| message["role"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["user", "assistant", "tool"]
    );
    assert_eq!(repaired[0]["content"], "steer");

    // A user without the queue marker, or a call the provider executed
    // itself, leaves the order alone.
    let plain_user = json!({ "role": "user", "content": "hi" });
    let messages = vec![messages_clone_call(), plain_user, messages_clone_result()];
    assert_eq!(repair_queued_message_order(messages.clone()), messages);
    let provider_call = json!({
        "role": "assistant",
        "content": [{ "type": "tool-call", "toolCallId": "t1", "providerExecuted": true }],
    });
    let queued = json!({
        "role": "user",
        "content": "steer",
        "providerOptions": { "openbutler": { "queuedMessageId": "m9" } },
    });
    let messages = vec![provider_call, queued, messages_clone_result()];
    assert_eq!(repair_queued_message_order(messages.clone()), messages);
}

fn messages_clone_call() -> Value {
    json!({
        "role": "assistant",
        "content": [{ "type": "tool-call", "toolCallId": "t1", "toolName": "grep", "input": {} }],
    })
}

fn messages_clone_result() -> Value {
    json!({
        "role": "tool",
        "content": [{ "type": "tool-result", "toolCallId": "t1" }],
    })
}
