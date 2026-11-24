//! The host-wide connection roster: who a host notification reaches.
//!
//! Source: legacy `sockets` set + the `stop` action's notification loop
//! (daemon-host.ts L3200-3210, read 2026-11-24). The roster is the set of
//! admitted connections; a broadcast skips the requester, and a sink that
//! refuses its frame is dropped the way a fan-out subscriber is.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use openbutler_daemon::dispatch::clients::Clients;
use openbutler_daemon::dispatch::fanout::EventSink;
use openbutler_protocol::envelope::DaemonEvent;
use serde_json::Value;

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

fn notification(frame: &[u8]) -> Value {
    let text = std::str::from_utf8(frame).unwrap();
    serde_json::from_str(text.trim_end()).expect("a broadcast frame must be JSON")
}

#[test]
fn a_broadcast_reaches_every_connection_except_the_requester() {
    let roster = Clients::new();
    let first = sink();
    let second = sink();
    let requester = sink();
    roster.register(first.clone());
    roster.register(second.clone());
    roster.register(requester.clone());

    let delivered = roster.broadcast_notification(
        "OpenButler host restarting",
        "Sessions will be saved. Reconnecting after the host restarts.",
        Some(&(requester.clone() as Arc<dyn EventSink>)),
    );

    assert_eq!(
        delivered, 2,
        "both other connections must receive the frame"
    );
    assert_eq!(first.frames.lock().unwrap().len(), 1);
    assert_eq!(second.frames.lock().unwrap().len(), 1);
    assert!(
        requester.frames.lock().unwrap().is_empty(),
        "the requester hears its own stop through the response, not a notification"
    );

    let event: DaemonEvent = serde_json::from_value(notification(&first.frames.lock().unwrap()[0]))
        .expect("the frame must be a DaemonEvent");
    let DaemonEvent::Notification { title, message, .. } = event else {
        panic!("the broadcast must be a notification event");
    };
    assert_eq!(title, "OpenButler host restarting");
    assert_eq!(
        message,
        "Sessions will be saved. Reconnecting after the host restarts."
    );
}

#[test]
fn an_unregistered_connection_is_not_reached() {
    let roster = Clients::new();
    let gone = sink();
    let id = roster.register(gone.clone());
    roster.unregister(id);

    let delivered = roster.broadcast_notification(
        "OpenButler host stopping",
        "The owner requested a shutdown.",
        None,
    );

    assert_eq!(delivered, 0);
    assert!(gone.frames.lock().unwrap().is_empty());
}

#[test]
fn a_refusing_sink_is_dropped_from_the_roster() {
    let roster = Clients::new();
    let refusing = Arc::new(RecordingSink {
        frames: Mutex::new(Vec::new()),
        refuse: true,
    });
    let alive = sink();
    roster.register(refusing.clone());
    roster.register(alive.clone());

    let first = roster.broadcast_notification("t", "m", None);
    assert_eq!(first, 1, "only the healthy sink takes the frame");
    // The refusing sink left the roster: the second broadcast has nothing
    // to ask it again.
    let second = roster.broadcast_notification("t", "m", None);
    assert_eq!(second, 1);
    assert_eq!(alive.frames.lock().unwrap().len(), 2);
}
