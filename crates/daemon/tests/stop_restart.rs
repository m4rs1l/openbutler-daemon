//! `stop`'s restart flag: refusal wording, the host-wide notification every
//! other connection hears, and the shutdown request.
//!
//! Source: daemon-host.ts L3191-3214, read 2026-11-24. The one divergence is
//! documented in the arm: the live-session state keys the source records for
//! reattach (`host.restart.workspace:<key>`) belong to the attach runtime this
//! build does not have yet, so nothing records them.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};

use common::temp_data_dir;
use openbutler_daemon::dispatch::fanout::{EventSink, SharedSink};
use openbutler_daemon::dispatch::{self, DispatchState, Host};
use openbutler_protocol::envelope::DaemonEvent;
use openbutler_protocol::{DaemonRequest, DaemonResponse};
use serde_json::{Value, json};

#[derive(Default)]
struct RecordingSink {
    frames: Mutex<Vec<Vec<u8>>>,
}

impl EventSink for RecordingSink {
    fn try_send(&self, frame: Vec<u8>) -> bool {
        self.frames.lock().unwrap().push(frame);
        true
    }
}

fn sink() -> Arc<RecordingSink> {
    Arc::new(RecordingSink::default())
}

fn stop(host: &Host, dir: &Path, state: &mut DispatchState, payload: Value) -> DaemonResponse {
    let mut request = DaemonRequest::new("stop-1", "stop");
    if let Value::Object(fields) = payload {
        for (key, value) in fields {
            request.payload.insert(key, value);
        }
    }
    dispatch::handle(host, dir, state, &request)
}

fn refusal(host: &Host, dir: &Path, payload: Value) -> String {
    let mut state = DispatchState::new();
    match stop(host, dir, &mut state, payload) {
        DaemonResponse::Failure { error, .. } => error,
        DaemonResponse::Success { result, .. } => panic!("expected a refusal, got {result}"),
    }
}

fn notification_of(frame: &[u8]) -> (String, String) {
    let text = std::str::from_utf8(frame).unwrap();
    let event: DaemonEvent =
        serde_json::from_str(text.trim_end()).expect("the broadcast frame must be a DaemonEvent");
    let DaemonEvent::Notification { title, message, .. } = event else {
        panic!("the broadcast must be a notification event");
    };
    (title, message)
}

#[test]
fn restart_must_be_a_boolean() {
    let dir = temp_data_dir("stop-restart-shape");
    let host = Host::new();
    // The source's `typeof` check refuses every non-boolean value, null
    // included (`request.restart !== undefined && typeof !== 'boolean'`).
    assert_eq!(
        refusal(&host, &dir, json!({ "restart": "yes" })),
        "restart must be a boolean."
    );
    assert_eq!(
        refusal(&host, &dir, json!({ "restart": null })),
        "restart must be a boolean."
    );
}

#[test]
fn a_restart_stop_notifies_every_other_connection() {
    let dir = temp_data_dir("stop-restart-notify");
    let host = Host::new();
    let other = sink();
    host.clients().register(other.clone() as SharedSink);
    let requester = sink();
    let mut state = DispatchState::new();
    state.set_sink(requester.clone() as SharedSink);

    let response = stop(&host, &dir, &mut state, json!({ "restart": true }));
    let DaemonResponse::Success { result, .. } = response else {
        panic!("stop must succeed");
    };
    assert_eq!(result, json!({ "stopping": true }));

    let frames = other.frames.lock().unwrap().clone();
    assert_eq!(
        frames.len(),
        1,
        "one notification reaches the other connection"
    );
    assert_eq!(
        notification_of(&frames[0]),
        (
            "OpenButler host restarting".to_owned(),
            "Sessions will be saved. Reconnecting after the host restarts.".to_owned()
        )
    );
    assert!(
        requester.frames.lock().unwrap().is_empty(),
        "the requester hears the stop through its response, never as a notification"
    );
}

#[test]
fn a_plain_stop_uses_the_stopping_wording() {
    let dir = temp_data_dir("stop-plain-wording");
    let host = Host::new();
    let other = sink();
    host.clients().register(other.clone() as SharedSink);
    let mut state = DispatchState::new();

    let response = stop(&host, &dir, &mut state, json!({}));
    assert!(matches!(response, DaemonResponse::Success { .. }));

    let frames = other.frames.lock().unwrap().clone();
    assert_eq!(frames.len(), 1);
    assert_eq!(
        notification_of(&frames[0]),
        (
            "OpenButler host stopping".to_owned(),
            "The owner requested a shutdown.".to_owned()
        )
    );
}

#[test]
fn stop_defers_the_shutdown_to_the_connection() {
    let dir = temp_data_dir("stop-shutdown");
    let host = Host::new();
    let shutdown = host.subscribe_shutdown();
    assert!(!*shutdown.borrow());
    let mut state = DispatchState::new();
    let response = stop(&host, &dir, &mut state, json!({ "restart": true }));
    assert!(matches!(response, DaemonResponse::Success { .. }));
    assert!(
        !*shutdown.borrow(),
        "the arm must not end the serve loop before its ack is written"
    );
    assert!(
        state.take_shutdown_request(),
        "the connection gets the request and performs it after the response"
    );
    assert!(!state.take_shutdown_request(), "the request is taken once");
}
