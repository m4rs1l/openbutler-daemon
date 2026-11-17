//! Fan-out behaviour: one published state reaches exactly the sockets
//! subscribed to that session, and a refusing sink is dropped.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use openbutler_daemon::agent::events::PublishSnapshot;
use openbutler_daemon::dispatch::fanout::{EventSink, Fanout};
use openbutler_protocol::envelope::DaemonEvent;
use openbutler_protocol::runtime::RuntimeViewData;
use openbutler_protocol::session::entry::SessionEntry;
use openbutler_protocol::session::{RunState, SessionMeta, SessionView};
use serde_json::json;

/// A sink that records every frame, or refuses every frame on demand.
#[derive(Default)]
struct RecordingSink {
    frames: Mutex<Vec<Vec<u8>>>,
    refuse: bool,
}

impl EventSink for RecordingSink {
    fn try_send(&self, frame: Vec<u8>) -> bool {
        if self.refuse {
            return false;
        }
        self.frames.lock().unwrap().push(frame);
        true
    }
}

fn sink() -> Arc<RecordingSink> {
    Arc::new(RecordingSink::default())
}

fn user(text: &str) -> SessionEntry {
    serde_json::from_value(json!({
        "kind": "user",
        "id": "e1",
        "createdAt": "2026-11-17T20:00:00.000Z",
        "text": text,
    }))
    .expect("entry fixture")
}

fn meta(id: &str) -> SessionMeta {
    serde_json::from_value(json!({
        "version": 1,
        "id": id,
        "title": "Chat",
        "createdAt": "2026-11-17T20:00:00.000Z",
        "updatedAt": "2026-11-17T20:00:00.000Z",
        "providerId": "mock",
        "modelId": "test-model",
    }))
    .expect("meta fixture")
}

fn run_state() -> RunState {
    serde_json::from_value(json!({
        "busy": false,
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

fn snapshot(id: &str, entries: Vec<SessionEntry>) -> PublishSnapshot {
    PublishSnapshot {
        session: SessionView {
            meta: meta(id),
            entries,
        },
        run: run_state(),
    }
}

fn runtime() -> RuntimeViewData {
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
        "reasoningVariants": [],
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
fn a_published_state_reaches_every_subscriber_of_that_session() {
    let fanout = Fanout::new();
    let first = sink();
    let second = sink();
    fanout.subscribe("sess-1", first.clone());
    fanout.subscribe("sess-1", second.clone());

    let previous = snapshot("sess-1", vec![user("hi")]);
    let next = snapshot("sess-1", vec![user("hi"), user("there")]);
    let delivered = fanout.publish("sess-1", &previous, &next, &runtime());
    assert_eq!(delivered, 2);

    for sink in [first, second] {
        let frames = sink.frames.lock().unwrap();
        assert_eq!(frames.len(), 1, "one frame per published state");
        let event: DaemonEvent =
            serde_json::from_slice(frames[0].strip_suffix(b"\n").unwrap_or(&frames[0]))
                .expect("the frame decodes as an event");
        assert!(
            matches!(event, DaemonEvent::Patch { .. }),
            "an append publishes a patch"
        );
    }
}

#[test]
fn other_sessions_do_not_receive_the_frame() {
    let fanout = Fanout::new();
    let stranger = sink();
    fanout.subscribe("sess-2", stranger.clone());

    let previous = snapshot("sess-1", vec![user("hi")]);
    let next = snapshot("sess-1", vec![user("ho")]);
    assert_eq!(fanout.publish("sess-1", &previous, &next, &runtime()), 0);
    assert!(stranger.frames.lock().unwrap().is_empty());
}

#[test]
fn an_unsubscribed_sink_stops_receiving() {
    let fanout = Fanout::new();
    let sink = sink();
    let id = fanout.subscribe("sess-1", sink.clone());
    let previous = snapshot("sess-1", vec![user("a")]);
    let next = snapshot("sess-1", vec![user("b")]);
    assert_eq!(fanout.publish("sess-1", &previous, &next, &runtime()), 1);

    fanout.unsubscribe(id);
    assert_eq!(fanout.publish("sess-1", &previous, &next, &runtime()), 0);
    assert_eq!(sink.frames.lock().unwrap().len(), 1, "only the first frame");
}

#[test]
fn a_refusing_sink_is_dropped_from_the_roster() {
    let fanout = Fanout::new();
    let refusing = Arc::new(RecordingSink {
        frames: Mutex::new(Vec::new()),
        refuse: true,
    });
    let healthy = sink();
    fanout.subscribe("sess-1", refusing.clone());
    fanout.subscribe("sess-1", healthy.clone());

    let previous = snapshot("sess-1", vec![user("a")]);
    let next = snapshot("sess-1", vec![user("b")]);
    assert_eq!(
        fanout.publish("sess-1", &previous, &next, &runtime()),
        1,
        "the refusing sink does not count as delivered"
    );

    // The refusing subscription is gone: a second publish reaches only
    // the healthy sink and never consults the refused one again.
    assert_eq!(fanout.publish("sess-1", &previous, &next, &runtime()), 1);
    assert_eq!(healthy.frames.lock().unwrap().len(), 2);
    assert!(refusing.frames.lock().unwrap().is_empty());
}
