//! The patch builder labels a continuation with the stream it grows, so the
//! client applies a label instead of inferring one from the index.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use openbutler_daemon::agent::events::diff_entries;
use openbutler_protocol::session::{SessionEntriesPatch, TextDeltaKind};
use serde_json::json;

fn entries(entry: serde_json::Value) -> Vec<openbutler_protocol::SessionEntry> {
    vec![serde_json::from_value(entry).unwrap()]
}

fn only_delta_kind(
    previous: &[openbutler_protocol::SessionEntry],
    next: &[openbutler_protocol::SessionEntry],
) -> TextDeltaKind {
    let Some(SessionEntriesPatch::Delta { append_text, .. }) = diff_entries(previous, next) else {
        panic!("a pure text append must ride the delta");
    };
    assert_eq!(append_text.len(), 1, "{append_text:?}");
    append_text[0].kind
}

#[test]
fn a_reasoning_continuation_is_labelled_reasoning() {
    let before = entries(json!({
        "kind": "reasoning", "at": 1, "turnId": "t", "text": "thin"
    }));
    let after = entries(json!({
        "kind": "reasoning", "at": 1, "turnId": "t", "text": "thinking"
    }));
    assert_eq!(only_delta_kind(&before, &after), TextDeltaKind::Reasoning);
}

#[test]
fn an_assistant_continuation_stays_text() {
    let before = entries(json!({
        "kind": "assistant", "at": 1, "turnId": "t", "text": "Hel"
    }));
    let after = entries(json!({
        "kind": "assistant", "at": 1, "turnId": "t", "text": "Hello"
    }));
    assert_eq!(only_delta_kind(&before, &after), TextDeltaKind::Text);
}

/// The assistant's opening chunk lands in the same publish that stamps the
/// reasoning block's duration. The rewritten reasoning row must not swallow
/// the entry appended after it: a client that never receives the append holds
/// no row for the answer and prints only the second half of the first word.
#[test]
fn an_entry_appended_after_a_rewritten_one_still_rides_the_patch() {
    let before: Vec<openbutler_protocol::SessionEntry> = vec![
        serde_json::from_value(json!({
            "kind": "user", "at": 1, "text": "tek kelimeyle merhaba de"
        }))
        .unwrap(),
        serde_json::from_value(json!({
            "kind": "reasoning", "at": 2, "turnId": "t", "text": "The user says hello"
        }))
        .unwrap(),
    ];
    let after: Vec<openbutler_protocol::SessionEntry> = vec![
        serde_json::from_value(json!({
            "kind": "user", "at": 1, "text": "tek kelimeyle merhaba de"
        }))
        .unwrap(),
        serde_json::from_value(json!({
            "kind": "reasoning", "at": 2, "turnId": "t", "text": "The user says hello",
            "durationMs": 42
        }))
        .unwrap(),
        serde_json::from_value(json!({
            "kind": "assistant", "at": 3, "turnId": "t", "text": "Mer"
        }))
        .unwrap(),
    ];
    let Some(SessionEntriesPatch::Delta {
        append,
        replace,
        append_text,
    }) = diff_entries(&before, &after)
    else {
        panic!("the publish must ride the delta");
    };
    assert_eq!(replace.len(), 1, "the stamped reasoning row is rewritten");
    assert_eq!(
        append.len(),
        1,
        "the new answer row must be appended: {append:?}"
    );
    match &append[0] {
        openbutler_protocol::SessionEntry::Assistant { text, .. } => assert_eq!(text, "Mer"),
        other => panic!("expected the assistant row, got {other:?}"),
    }
    assert!(append_text.is_empty(), "{append_text:?}");
}
