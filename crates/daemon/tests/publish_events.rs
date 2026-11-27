//! `make_session_event` / `diff_entries` — the publish unit (legacy
//! `makeSessionEvent`, daemon-host.ts L3711, and `diffEntries` L3749,
//! read 2026-11-17): previous/next snapshots become one `DaemonEvent`.
//!
//! Contract source: `event-snapshot.json` and `patch-delta.json`
//! conformance vectors (protocol side already round-trips them).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use openbutler_daemon::agent::events::{PublishSnapshot, diff_entries, make_session_event};
use openbutler_protocol::DAEMON_PROTOCOL_VERSION;
use openbutler_protocol::envelope::DaemonEvent;
use openbutler_protocol::runtime::RuntimeViewData;
use openbutler_protocol::session::entry::SessionEntry;
use openbutler_protocol::session::{
    RunState, SessionEntriesPatch, SessionMeta, SessionView, TextDelta, TextDeltaKind,
};
use serde_json::{Value, json};

fn entry(value: Value) -> SessionEntry {
    serde_json::from_value(value).expect("entry fixture")
}

fn user(text: &str) -> SessionEntry {
    entry(json!({
        "kind": "user",
        "id": "e1",
        "createdAt": "2026-11-17T20:00:00.000Z",
        "text": text,
    }))
}

fn assistant(text: &str) -> SessionEntry {
    entry(json!({
        "kind": "assistant",
        "id": "e2",
        "createdAt": "2026-11-17T20:00:01.000Z",
        "text": text,
    }))
}

fn reasoning(text: &str, duration_ms: Option<i64>) -> SessionEntry {
    let mut value = json!({
        "kind": "reasoning",
        "id": "e3",
        "createdAt": "2026-11-17T20:00:02.000Z",
        "text": text,
    });
    if let Some(duration) = duration_ms {
        value["durationMs"] = json!(duration);
    }
    entry(value)
}

fn run_state(busy: bool) -> RunState {
    serde_json::from_value(json!({
        "busy": busy,
        "runningTurnId": null,
        "pendingApproval": null,
        "pendingApprovalId": null,
        "pendingQuestion": null,
        "queuedMessageCount": 0,
        "firstQueuedMessage": null,
        "queuedMessages": [],
        "sessionSaveError": null,
        "streamingAssistantIndex": null,
    }))
    .expect("run state fixture")
}

fn meta(id: &str) -> SessionMeta {
    serde_json::from_value(json!({
        "version": 1,
        "id": id,
        "title": "Chat",
        "preview": "",
        "createdAt": "2026-11-17T20:00:00.000Z",
        "updatedAt": "2026-11-17T20:00:00.000Z",
        "providerId": "mock",
        "modelId": "test-model",
    }))
    .expect("meta fixture")
}

fn snapshot(id: &str, entries: Vec<SessionEntry>) -> PublishSnapshot {
    PublishSnapshot {
        session: SessionView {
            meta: meta(id),
            entries,
        },
        run: run_state(false),
    }
}

fn runtime() -> RuntimeViewData {
    // The conformance fixture itself (runtime-view.json) — the wire shape
    // the protocol suite already round-trips.
    serde_json::from_value(json!({
        "supportsSessionAttachments": true,
        "supportsProjects": true,
        "workspaceRoot": "C:\\work\\demo",
        "modelName": "MiMo V2.6 Flash",
        "providerName": "Vercel Gateway",
        "providerId": "vercel",
        "modelId": "xiaomi/mimo-v2.6-flash",
        "contextWindowTokens": 524288,
        "supportsVision": false,
        "reasoningVariants": [
            {
                "id": "high",
                "label": "High",
                "providerOptions": { "reasoningEffort": { "effort": "high" } }
            },
            { "id": "none", "label": "None", "providerOptions": {} }
        ],
        "reasoningMetadataSource": null,
        "reasoningSupport": "available",
        "reasoningDefault": "high",
        "reasoningPreference": "high",
        "showReasoningDefault": false,
        "reasoningTitles": false,
        "approvalMode": "full-access",
    }))
    .expect("runtime fixture")
}

#[test]
fn a_session_id_change_publishes_a_full_snapshot() {
    let previous = snapshot("aaaa", vec![user("hi")]);
    let next = snapshot("bbbb", vec![user("hi")]);
    match make_session_event(&previous, &next, &runtime(), DAEMON_PROTOCOL_VERSION) {
        DaemonEvent::Snapshot {
            version, snapshot, ..
        } => {
            assert_eq!(version, DAEMON_PROTOCOL_VERSION);
            assert_eq!(snapshot.session.meta.id, "bbbb");
            assert_eq!(snapshot.session.entries.len(), 1);
        }
        other => panic!("expected a snapshot event, got {other:?}"),
    }
}

#[test]
fn unchanged_entries_publish_a_patch_without_an_entries_patch() {
    let previous = snapshot("aaaa", vec![user("hi"), assistant("yo")]);
    let next = snapshot("aaaa", vec![user("hi"), assistant("yo")]);
    match make_session_event(&previous, &next, &runtime(), DAEMON_PROTOCOL_VERSION) {
        DaemonEvent::Patch { patch, .. } => {
            assert_eq!(patch.session.id, "aaaa");
            assert!(
                patch.entries_patch.is_none(),
                "identical entries carry no entriesPatch key"
            );
        }
        other => panic!("expected a patch event, got {other:?}"),
    }
}

#[test]
fn appended_entries_become_a_delta_with_no_rewrites() {
    let previous = snapshot("aaaa", vec![user("hi")]);
    let next = snapshot("aaaa", vec![user("hi"), assistant("yo")]);
    assert_eq!(
        diff_entries(&previous.session.entries, &next.session.entries),
        Some(SessionEntriesPatch::Delta {
            append: vec![assistant("yo")],
            replace: vec![],
            append_text: vec![],
        })
    );
}

#[test]
fn a_shrinking_transcript_replaces_everything() {
    let previous = snapshot("aaaa", vec![user("one"), assistant("two")]);
    let next = snapshot("aaaa", vec![user("one")]);
    assert_eq!(
        diff_entries(&previous.session.entries, &next.session.entries),
        Some(SessionEntriesPatch::ReplaceAll {
            entries: vec![user("one")]
        })
    );
}

#[test]
fn a_pure_text_append_rides_an_append_text_delta() {
    let previous = snapshot("aaaa", vec![assistant("Hel")]);
    let next = snapshot("aaaa", vec![assistant("Hello world")]);
    assert_eq!(
        diff_entries(&previous.session.entries, &next.session.entries),
        Some(SessionEntriesPatch::Delta {
            append: vec![],
            replace: vec![],
            append_text: vec![TextDelta {
                index: 0,
                delta: "lo world".to_owned(),
                kind: TextDeltaKind::Text,
            }],
        }),
        "only the text suffix crosses the wire"
    );
}

#[test]
fn a_field_change_replaces_the_entry_instead_of_appending_text() {
    // The stamped thinking duration case (source comment L3768): a delta
    // patch cannot carry a non-text field, or the client keeps a stale copy.
    let previous = snapshot("aaaa", vec![reasoning("Hello", Some(12))]);
    let next = snapshot("aaaa", vec![reasoning("Hello!", Some(40))]);
    assert_eq!(
        diff_entries(&previous.session.entries, &next.session.entries),
        Some(SessionEntriesPatch::Delta {
            append: vec![],
            replace: vec![openbutler_protocol::session::IndexedEntry {
                index: 0,
                entry: reasoning("Hello!", Some(40)),
            }],
            append_text: vec![],
        })
    );
}

#[test]
fn identical_snapshots_publish_no_event_difference_at_all() {
    let previous = snapshot("aaaa", vec![user("same")]);
    let next = snapshot("aaaa", vec![user("same")]);
    assert_eq!(
        diff_entries(&previous.session.entries, &next.session.entries),
        None
    );
}

#[test]
fn the_patch_event_round_trips_through_the_wire() {
    let previous = snapshot("aaaa", vec![user("hi")]);
    let next = snapshot("aaaa", vec![user("hi"), assistant("yo")]);
    let event = make_session_event(&previous, &next, &runtime(), DAEMON_PROTOCOL_VERSION);
    let text = serde_json::to_string(&event).expect("serialize");
    let decoded: DaemonEvent = serde_json::from_str(&text).expect("decode");
    assert_eq!(event, decoded, "the published frame survives the wire");
}
