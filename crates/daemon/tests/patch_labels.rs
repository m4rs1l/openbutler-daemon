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
