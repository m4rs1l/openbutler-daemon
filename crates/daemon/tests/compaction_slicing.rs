//! Compaction's slice chunker (C2b-2a): greedy transcript chunking that
//! never splits a tool call from its result.
//!
//! Source: legacy `compaction.ts` L798-840 (`splitIntoSlices`,
//! `messageToolCallIds`, `messageToolResultIds`) - read 2026-11-18. The
//! AI SDK rejects a request whose assistant tool call has no tool-result
//! in the same request; that killed every naive live cut on 2026-11-03,
//! so a slice closes only while no call inside it is unresolved.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use openbutler_daemon::compaction::{estimate_tokens, split_into_slices};
use serde_json::{Value, json};

fn user(text: &str) -> Value {
    json!({ "role": "user", "content": text })
}

fn call(id: &str) -> Value {
    json!({
        "role": "assistant",
        "content": [{ "type": "tool-call", "toolCallId": id, "toolName": "grep", "input": {} }],
    })
}

fn result(id: &str) -> Value {
    json!({
        "role": "tool",
        "content": [{ "type": "tool-result", "toolCallId": id }],
    })
}

#[test]
fn slices_greedily_until_the_budget_holds_no_more() {
    let messages = vec![user("aaaa"), user("bbbb"), user("cccc"), user("dddd")];
    let one = estimate_tokens(&messages[0]);
    let slices = split_into_slices(&messages, one * 2, one * 100).expect("fits");
    assert_eq!(
        slices.iter().map(Vec::len).collect::<Vec<_>>(),
        vec![2, 2],
        "two messages per slice at a two-message budget"
    );
}

#[test]
fn a_call_never_leaves_its_result_behind() {
    let plain = user("aaaa");
    let opening = call("t1");
    let closing = result("t1");
    let after = user("zzzz");
    let messages = vec![plain, opening, closing, after];

    // A budget that fits the plain message plus the call: the boundary
    // check runs before the result arrives, the open call keeps the
    // pair together, and only the next plain message closes the slice.
    let one = estimate_tokens(&messages[0]);
    let opening_size = estimate_tokens(&messages[1]);
    let slices = split_into_slices(&messages, one + opening_size, one * 100).expect("fits");
    assert_eq!(slices.len(), 2, "slices: {slices:?}");
    assert_eq!(slices[0].len(), 3, "pair travels together: {:?}", slices[0]);
    assert_eq!(slices[1], vec![messages[3].clone()]);
}

#[test]
fn an_oversized_but_fitting_message_opens_its_own_slice() {
    let small = user("aa");
    let huge = user(&"x".repeat(400));
    let small_two = user("bb");
    let messages = vec![small.clone(), huge.clone(), small_two];
    let small_size = estimate_tokens(&messages[0]);
    let huge_size = estimate_tokens(&messages[1]);
    // Budget fits only small messages; the huge one is still under the
    // hard cap, so it closes the current slice and starts a fresh one.
    let slices = split_into_slices(&messages, small_size * 2, huge_size).expect("fits");
    assert_eq!(
        slices.iter().map(Vec::len).collect::<Vec<_>>(),
        vec![1, 1, 1],
        "the huge message closes the first slice, then closes the next too: {slices:?}"
    );
    assert_eq!(slices[0], vec![small]);
    assert_eq!(slices[1], vec![huge]);
}

#[test]
fn a_single_message_over_the_hard_cap_refuses_the_chain() {
    let messages = vec![user(&"x".repeat(400))];
    let size = estimate_tokens(&messages[0]);
    assert!(
        split_into_slices(&messages, size - 1, size - 1).is_none(),
        "one message that cannot fit any slice stops the chain as data"
    );
}

#[test]
fn an_unresolved_call_trails_into_the_final_slice() {
    // A call whose result never arrives keeps every later boundary shut:
    // the open set never empties, so everything after it is one slice.
    let messages = vec![call("t-lost"), user("bbbb"), user("cccc")];
    let one = estimate_tokens(&json!({ "role": "user", "content": "aaaa" }));
    let slices = split_into_slices(&messages, one * 2, one * 100).expect("fits");
    assert_eq!(slices.len(), 1, "open call pins the tail: {slices:?}");
    assert_eq!(slices[0].len(), 3);
}
