//! `session.send` at the dispatch surface: the message contract, the
//! accept-immediately answer, and the turn handed to the executor seam.
//!
//! Source: legacy `daemon-host.ts` case `session.send` (L3501) and
//! `parseQueuedMessage` (L4104), read 2026-11-17. One documented
//! deviation: well-formed images are refused loudly instead of being
//! stored as attachments (that port is a later slice) — silent loss is
//! not an option.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};

use common::{temp_data_dir, uuid, write_session};
use openbutler_daemon::agent::{TurnExecutor, TurnRequest};
use openbutler_daemon::dispatch::{self, DispatchState, Host};
use openbutler_protocol::entry::UserEntrySource;
use openbutler_protocol::{DaemonRequest, DaemonResponse};
use serde_json::{Value, json};

/// The seam's test double: records what dispatch handed over.
#[derive(Clone, Default)]
struct RecordingExecutor {
    sent: Arc<Mutex<Vec<(String, TurnRequest)>>>,
}

impl TurnExecutor for RecordingExecutor {
    fn spawn_turn(self: Arc<Self>, session_id: String, request: TurnRequest) {
        self.sent.lock().unwrap().push((session_id, request));
    }
}

fn request(id: &str, action: &str, pairs: &[(&str, Value)]) -> DaemonRequest {
    let mut request = DaemonRequest::new(id, action);
    for (key, value) in pairs {
        request = request.with(*key, value.clone());
    }
    request
}

fn call(
    host: &Host,
    data_dir: &Path,
    state: &mut DispatchState,
    request: DaemonRequest,
) -> DaemonResponse {
    dispatch::handle(host, data_dir, state, &request)
}

fn success_result(response: DaemonResponse) -> Value {
    let DaemonResponse::Success { result, .. } = response else {
        panic!("expected a success envelope, got {response:?}");
    };
    result
}

fn failure_text(response: DaemonResponse) -> String {
    let DaemonResponse::Failure { error, .. } = response else {
        panic!("expected a failure envelope, got {response:?}");
    };
    error
}

fn send_request(message: Value) -> DaemonRequest {
    request("r1", "session.send", &[("message", message)])
}

fn message(text: &str, images: Value) -> Value {
    json!({ "text": text, "images": images })
}

/// A chat with a resolvable provider: `mock` in settings.yaml (url) and
/// config.yaml (secret key), model and workspace on the record.
fn seed_chat(dir: &Path) -> String {
    std::fs::write(
        dir.join("settings.yaml"),
        "providers:\n  mock:\n    enabled: true\n    url: http://127.0.0.1:9/v1\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("config.yaml"),
        "providers:\n  mock:\n    keys:\n      - sk-test-key\n",
    )
    .unwrap();
    let id = uuid(7);
    let body = format!(
        "{{\n  \"version\": 1,\n  \"id\": \"{id}\",\n  \"title\": \"send chat\",\n  \"createdAt\": \"2026-11-17T00:00:00.000Z\",\n  \"updatedAt\": \"2026-11-17T00:00:00.000Z\",\n  \"providerId\": \"mock\",\n  \"modelId\": \"test-model\",\n  \"workspaceRoot\": \"work/chat\",\n  \"entries\": [],\n  \"conversation\": []\n}}\n"
    );
    write_session(dir, &id, &body);
    id
}

fn host_with_executor() -> (Host, RecordingExecutor) {
    let host = Host::new();
    let recorder = RecordingExecutor::default();
    host.set_executor(Arc::new(recorder.clone()));
    (host, recorder)
}

#[test]
fn a_non_record_message_is_refused_with_the_source_wording() {
    let dir = temp_data_dir("send-nonrecord");
    let (host, _) = host_with_executor();
    let mut state = DispatchState::new();
    let response = call(&host, &dir, &mut state, send_request(json!("hello")));
    assert_eq!(
        failure_text(response),
        "message must contain text smaller than 8 MB."
    );
}

#[test]
fn text_over_8_mib_is_refused() {
    let dir = temp_data_dir("send-bigtext");
    let (host, _) = host_with_executor();
    let mut state = DispatchState::new();
    let big = "a".repeat(8 * 1024 * 1024 + 1);
    let response = call(
        &host,
        &dir,
        &mut state,
        send_request(message(&big, json!([]))),
    );
    assert_eq!(
        failure_text(response),
        "message must contain text smaller than 8 MB."
    );
}

#[test]
fn the_image_list_is_required_and_capped_at_eight() {
    let dir = temp_data_dir("send-images");
    let (host, _) = host_with_executor();
    let mut state = DispatchState::new();

    // Missing key: not an array under the source's combined check.
    let missing = json!({ "text": "hi" });
    assert_eq!(
        failure_text(call(&host, &dir, &mut state, send_request(missing))),
        "message may contain up to 8 images."
    );

    let nine: Vec<Value> = (0..9)
        .map(|_| json!({ "mediaType": "image/png", "base64": "aGVsbG8=" }))
        .collect();
    assert_eq!(
        failure_text(call(
            &host,
            &dir,
            &mut state,
            send_request(message("hi", Value::Array(nine)))
        )),
        "message may contain up to 8 images."
    );
}

#[test]
fn each_image_must_be_a_base64_png_record() {
    let dir = temp_data_dir("send-image-shape");
    let (host, _) = host_with_executor();
    let mut state = DispatchState::new();

    let wrong_type = json!({ "mediaType": "image/jpeg", "base64": "aGVsbG8=" });
    assert_eq!(
        failure_text(call(
            &host,
            &dir,
            &mut state,
            send_request(message("hi", json!([wrong_type])))
        )),
        "Only base64 PNG images are supported over this IPC version."
    );

    let not_a_record = json!("aGVsbG8=");
    assert_eq!(
        failure_text(call(
            &host,
            &dir,
            &mut state,
            send_request(message("hi", json!([not_a_record])))
        )),
        "Only base64 PNG images are supported over this IPC version."
    );
}

#[test]
fn well_formed_images_are_refused_loudly_instead_of_dropped() {
    let dir = temp_data_dir("send-image-drop");
    let (host, _) = host_with_executor();
    let mut state = DispatchState::new();
    let png = json!({ "mediaType": "image/png", "base64": "aGVsbG8=" });
    let response = call(
        &host,
        &dir,
        &mut state,
        send_request(message("hi", json!([png]))),
    );
    let text = failure_text(response);
    assert!(
        text.to_lowercase().contains("image"),
        "the refusal names the unsupported part: {text}"
    );
    assert!(
        !text.contains("dropped"),
        "never claim acceptance for work that did not happen"
    );
}

#[test]
fn sending_requires_a_loaded_chat() {
    let dir = temp_data_dir("send-nochat");
    let (host, _) = host_with_executor();
    let mut state = DispatchState::new();
    let response = call(
        &host,
        &dir,
        &mut state,
        send_request(message("hi", json!([]))),
    );
    assert_eq!(failure_text(response), "Chat not found.");
}

#[test]
fn an_accepted_send_hands_a_prepared_turn_to_the_executor() {
    let dir = temp_data_dir("send-accept");
    let session_id = seed_chat(&dir);
    let (host, recorder) = host_with_executor();
    host.reload_settings(&dir).expect("settings load");
    let mut state = DispatchState::new();
    let loaded = call(
        &host,
        &dir,
        &mut state,
        request("load", "session.load", &[("sessionId", json!(session_id))]),
    );
    success_result(loaded);

    let response = call(
        &host,
        &dir,
        &mut state,
        send_request(message("hello there", json!([]))),
    );
    assert_eq!(success_result(response), json!({ "accepted": true }));

    let sent = recorder.sent.lock().unwrap();
    assert_eq!(sent.len(), 1, "exactly one turn per accepted send");
    let (handed_over, turn) = &sent[0];
    assert_eq!(handed_over, &session_id);
    assert_eq!(turn.text, "hello there");
    assert_eq!(turn.source, UserEntrySource::Terminal);
    assert_eq!(turn.provider_key, "mock");
    assert_eq!(turn.provider_url.as_deref(), Some("http://127.0.0.1:9/v1"));
    assert_eq!(turn.provider_api_key.as_deref(), Some("sk-test-key"));
    assert_eq!(turn.model, "test-model");
    assert_eq!(turn.workspace_root.as_deref(), Some("work/chat"));
    let system = turn.system.as_ref().expect("the turn carries a system");
    assert!(
        system.contains(&dir.join("MEMORY").display().to_string()),
        "the assembled system names this machine's memory root"
    );
    assert!(
        system.contains("work/chat"),
        "the assembled system names the chat's workspace"
    );
}
