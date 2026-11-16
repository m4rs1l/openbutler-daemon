#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Conformance suite: every JSON vector under `conformance/` must decode into
//! its contract type and serialize back to the identical wire value, and the
//! decoder/validation semantics must match the TypeScript implementation.

use openbutler_protocol::{
    AttachResult, DAEMON_PROTOCOL_VERSION, DaemonEvent, DaemonRequest, DaemonResponse,
    DaemonStatus, FrameDecoder, MAX_DAEMON_FRAME_BYTES, RuntimeViewData, SessionEntriesPatch,
    SessionEntriesRange, SessionEntry, SessionPatch, SessionSnapshotData, ValidationError,
    validate_request, validate_response,
};
use serde::de::DeserializeOwned;
use serde_json::Value;

/// Decodes a fixture into `T`, then proves the wire value survives a
/// decode → encode round trip unchanged.
fn roundtrip<T: DeserializeOwned + serde::Serialize>(fixture: &str) -> T {
    let original: Value = serde_json::from_str(fixture).expect("fixture must be valid JSON");
    let typed: T = serde_json::from_value(original.clone()).expect("fixture must decode");
    let back = serde_json::to_value(&typed).expect("typed value must serialize");
    assert_eq!(back, original, "round trip must preserve the wire value");
    typed
}

fn fixture(name: &str) -> &'static str {
    match name {
        "response-status" => include_str!("../conformance/response-status.json"),
        "request-attach" => include_str!("../conformance/request-attach.json"),
        "response-error" => include_str!("../conformance/response-error.json"),
        "event-notification" => include_str!("../conformance/event-notification.json"),
        "event-snapshot" => include_str!("../conformance/event-snapshot.json"),
        "daemon-status" => include_str!("../conformance/daemon-status.json"),
        "runtime-view" => include_str!("../conformance/runtime-view.json"),
        "snapshot-window" => include_str!("../conformance/snapshot-window.json"),
        "patch-delta" => include_str!("../conformance/patch-delta.json"),
        "entries-patch-replace-all" => {
            include_str!("../conformance/entries-patch-replace-all.json")
        }
        "entries-all-kinds" => include_str!("../conformance/entries-all-kinds.json"),
        "attach-result" => include_str!("../conformance/attach-result.json"),
        "entries-range" => include_str!("../conformance/entries-range.json"),
        "ledger-event" => include_str!("../conformance/ledger-event.json"),
        "ledger-event-nulls" => include_str!("../conformance/ledger-event-nulls.json"),
        "ledger-action" => include_str!("../conformance/ledger-action.json"),
        "ledger-action-fallbacks" => {
            include_str!("../conformance/ledger-action-fallbacks.json")
        }
        "sidecar-command-call" => {
            include_str!("../conformance/sidecar-command-call.json")
        }
        "sidecar-command-cancel" => {
            include_str!("../conformance/sidecar-command-cancel.json")
        }
        "sidecar-handshake" => include_str!("../conformance/sidecar-handshake.json"),
        "sidecar-event-text-delta" => {
            include_str!("../conformance/sidecar-event-text-delta.json")
        }
        "sidecar-event-reasoning-delta" => {
            include_str!("../conformance/sidecar-event-reasoning-delta.json")
        }
        "sidecar-event-tool-call" => {
            include_str!("../conformance/sidecar-event-tool-call.json")
        }
        "sidecar-event-usage" => include_str!("../conformance/sidecar-event-usage.json"),
        "sidecar-event-finish" => include_str!("../conformance/sidecar-event-finish.json"),
        "sidecar-event-error" => include_str!("../conformance/sidecar-event-error.json"),
        "auth-first-frame" => include_str!("../conformance/auth-first-frame.json"),
        other => panic!("unknown fixture {other}"),
    }
}

// --- Envelopes ---------------------------------------------------------------

#[test]
fn real_status_response_round_trip() {
    let response: DaemonResponse = roundtrip(fixture("response-status"));
    assert_eq!(response.id(), "probe-1");
    assert_eq!(response.version(), DAEMON_PROTOCOL_VERSION);
    validate_response(&response, DAEMON_PROTOCOL_VERSION).expect("valid response");

    let DaemonResponse::Success { result, .. } = response else {
        panic!("captured frame is a success");
    };
    let status: DaemonStatus =
        serde_json::from_value(result).expect("captured result must decode as DaemonStatus");
    assert_eq!(status.pid, 21104);
    assert_eq!(status.workspace_count, 3);
    assert_eq!(status.connected_clients, 2);
    // Explicit nulls survive as present-but-null (required nullable fields).
    let telegram = status.telegram.expect("telegram status present");
    assert_eq!(telegram.username, None);
    assert!(!telegram.paired);
}

#[test]
fn error_response_round_trip() {
    let response: DaemonResponse = roundtrip(fixture("response-error"));
    let DaemonResponse::Failure { error, ok, .. } = &response else {
        panic!("expected a failure frame");
    };
    assert!(!ok);
    assert!(error.starts_with("Authentication failed"));
    validate_response(&response, DAEMON_PROTOCOL_VERSION).expect("valid failure");
    assert!(matches!(
        validate_response(&response, DAEMON_PROTOCOL_VERSION + 1),
        Err(ValidationError::ResponseVersionMismatch { .. })
    ));
}

#[test]
fn attach_request_round_trip() {
    let request: DaemonRequest = roundtrip(fixture("request-attach"));
    assert_eq!(request.action, "attach");
    validate_request(&request).expect("valid request");
    assert_eq!(
        request.get("clientKind"),
        Some(&Value::String("terminal".into()))
    );
    assert!(
        request.get("workspaceKey").is_none(),
        "attach carries no host key"
    );
}

#[test]
fn request_validation_matrix() {
    let valid: DaemonRequest = serde_json::from_str(fixture("request-attach")).unwrap();
    validate_request(&valid).expect("baseline valid");

    let wrong_version = DaemonRequest {
        version: DAEMON_PROTOCOL_VERSION - 1,
        ..valid.clone()
    };
    assert!(matches!(
        validate_request(&wrong_version),
        Err(ValidationError::WrongVersion { .. })
    ));

    let empty_id = DaemonRequest {
        id: String::new(),
        ..valid.clone()
    };
    assert!(matches!(
        validate_request(&empty_id),
        Err(ValidationError::InvalidIdLength { actual: 0 })
    ));

    // JS `String.length` counts UTF-16 units: 64 astral characters are 128
    // units in JS even though they are 256 bytes here.
    let astral_64 = "\u{1F600}".repeat(64);
    let too_many_units = DaemonRequest {
        id: astral_64,
        ..valid.clone()
    };
    assert!(matches!(
        validate_request(&too_many_units),
        Err(ValidationError::InvalidIdLength { actual: 128 })
    ));
    let astral_32 = DaemonRequest {
        id: "\u{1F600}".repeat(32),
        ..valid.clone()
    };
    validate_request(&astral_32).expect("64 UTF-16 units are exactly the limit");

    let long_action = DaemonRequest {
        action: "a".repeat(65),
        ..valid
    };
    assert!(matches!(
        validate_request(&long_action),
        Err(ValidationError::InvalidActionLength { actual: 65 })
    ));
}

// --- Events ------------------------------------------------------------------

#[test]
fn notification_event_round_trip() {
    let event: DaemonEvent = roundtrip(fixture("event-notification"));
    let DaemonEvent::Notification {
        title,
        message,
        version,
    } = event
    else {
        panic!("expected notification");
    };
    assert_eq!(version, DAEMON_PROTOCOL_VERSION);
    assert_eq!(title, "OpenButler host stopping");
    assert!(message.contains("openbutler"));
}

#[test]
fn snapshot_event_round_trip() {
    let event: DaemonEvent = roundtrip(fixture("event-snapshot"));
    let DaemonEvent::Snapshot {
        snapshot, runtime, ..
    } = event
    else {
        panic!("expected snapshot");
    };
    assert!(snapshot.session.entries.is_empty());
    assert!(!snapshot.run.busy);
    assert_eq!(
        runtime.reasoning_support,
        openbutler_protocol::runtime::ReasoningSupport::ModelUnsupported
    );
}

#[test]
fn patch_event_variant_serializes_with_tag() {
    let patch: SessionPatch = serde_json::from_str(fixture("patch-delta")).unwrap();
    let event = DaemonEvent::Patch {
        version: DAEMON_PROTOCOL_VERSION,
        patch,
    };
    let value = serde_json::to_value(&event).unwrap();
    assert_eq!(value.get("event"), Some(&Value::String("patch".into())));
    assert_eq!(value.get("version"), Some(&Value::from(2)));
}

// --- Status and runtime ------------------------------------------------------

#[test]
fn daemon_status_round_trip() {
    let status: DaemonStatus = roundtrip(fixture("daemon-status"));
    assert_eq!(status.pid, 21104);
    let telegram = status.telegram.expect("telegram present");
    assert_eq!(format!("{:?}", telegram.state), "Polling");
    assert!(telegram.paired);
    assert_eq!(telegram.username.as_deref(), Some("m4rs1l"));
}

#[test]
fn runtime_view_round_trip() {
    let runtime: RuntimeViewData = roundtrip(fixture("runtime-view"));
    assert_eq!(runtime.context_window_tokens, Some(524_288));
    assert_eq!(runtime.reasoning_variants.len(), 2);
    assert_eq!(
        runtime.approval_mode,
        Some(openbutler_protocol::runtime::ApprovalMode::FullAccess)
    );
    assert_eq!(
        runtime.reasoning_variants[0].provider_options["reasoningEffort"],
        serde_json::json!({"effort": "high"})
    );
}

// --- Session state -----------------------------------------------------------

#[test]
fn windowed_snapshot_round_trip() {
    let snapshot: SessionSnapshotData = roundtrip(fixture("snapshot-window"));
    assert_eq!(snapshot.entries_from, Some(41));
    assert_eq!(snapshot.total_entries, Some(43));
    assert_eq!(snapshot.session.entries.len(), 2);
    assert!(snapshot.run.busy);
    assert_eq!(snapshot.run.streaming_assistant_index, Some(2));
    assert_eq!(snapshot.run.queued_message_count, 1);
    assert_eq!(snapshot.run.running_turn_id.as_deref(), Some("turn-9"));
    // Metadata sits flat next to the body.
    let tool = &snapshot.session.entries[0];
    assert_eq!(tool.meta().turn_id.as_deref(), Some("turn-9"));
    assert_eq!(
        tool.meta().turn_outcome,
        Some(openbutler_protocol::entry::TurnOutcome::Completed)
    );
}

#[test]
fn delta_patch_round_trip() {
    let patch: SessionPatch = roundtrip(fixture("patch-delta"));
    let Some(SessionEntriesPatch::Delta {
        append,
        replace,
        append_text,
    }) = &patch.entries_patch
    else {
        panic!("expected a delta patch");
    };
    assert_eq!(append.len(), 1);
    assert!(replace.is_empty());
    assert_eq!(append_text[0].index, 42);
    assert!(patch.run.pending_question.is_some());
    // `SessionMeta` has no `entries` member — a patch session is typed
    // without a transcript, so the field cannot ride along.
    assert_eq!(patch.session.title, "Wire conformance");
}

#[test]
fn replace_all_entries_patch_round_trip() {
    let entries: SessionEntriesPatch = roundtrip(fixture("entries-patch-replace-all"));
    let SessionEntriesPatch::ReplaceAll { entries } = entries else {
        panic!("expected replace-all");
    };
    assert_eq!(entries.len(), 3);
}

#[test]
fn every_entry_kind_round_trip() {
    let entries: Vec<SessionEntry> = roundtrip(fixture("entries-all-kinds"));
    assert_eq!(entries.len(), 8);
    let kinds: Vec<&str> = entries
        .iter()
        .map(|entry| match entry {
            SessionEntry::User { .. } => "user",
            SessionEntry::Assistant { .. } => "assistant",
            SessionEntry::Notice { .. } => "notice",
            SessionEntry::Divider { .. } => "divider",
            SessionEntry::Reasoning { .. } => "reasoning",
            SessionEntry::Compaction { .. } => "compaction",
            SessionEntry::Tool { .. } => "tool",
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "user",
            "user",
            "assistant",
            "notice",
            "divider",
            "reasoning",
            "compaction",
            "tool"
        ]
    );
    // The compaction checkpoint marks the raw summary entry.
    let SessionEntry::User { meta, .. } = &entries[1] else {
        panic!("second entry is the stored summary");
    };
    assert_eq!(
        meta.internal,
        Some(openbutler_protocol::entry::EntryInternal::Compaction)
    );
}

#[test]
fn attach_result_round_trip() {
    let attach: AttachResult = roundtrip(fixture("attach-result"));
    assert_eq!(attach.session_attachments, Some(true));
    assert_eq!(attach.workspace_key.as_deref(), Some("wsk_9b2f1c44"));
    assert_eq!(
        attach.client_kind,
        Some(openbutler_protocol::runtime::LocalClientKind::Terminal)
    );
    // Trio semantics: explicit `null` means "source run", not "older host".
    assert_eq!(attach.host_build_id, Some(None));
    assert_eq!(attach.saved_sessions.len(), 1);
    assert_eq!(attach.saved_sessions[0].running, Some(false));
}

#[test]
fn absent_build_id_stays_absent() {
    let mut value: Value = serde_json::from_str(fixture("attach-result")).unwrap();
    value.as_object_mut().unwrap().remove("hostBuildId");
    let attach: AttachResult = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(attach.host_build_id, None, "absent and null must differ");
    let back = serde_json::to_value(&attach).unwrap();
    assert!(
        back.get("hostBuildId").is_none(),
        "an absent key must not be written back"
    );
    // …while an explicit null is written back as null.
    let with_null: AttachResult = serde_json::from_str(fixture("attach-result")).unwrap();
    let back = serde_json::to_value(&with_null).unwrap();
    assert_eq!(back.get("hostBuildId"), Some(&Value::Null));
}

#[test]
fn entries_range_round_trip() {
    let range: SessionEntriesRange = roundtrip(fixture("entries-range"));
    assert_eq!((range.from, range.to, range.total), (0, 2, 40));
}

// --- Required-key strictness --------------------------------------------------

#[test]
fn patch_without_required_run_state_is_rejected() {
    let mut value: Value = serde_json::from_str(fixture("patch-delta")).unwrap();
    let object = value.as_object_mut().unwrap();
    object.remove("runningTurnId");
    object.remove("busy");
    let decoded: Result<SessionPatch, _> = serde_json::from_value(value);
    assert!(
        decoded.is_err(),
        "required nullable keys must be required on the wire, even through flatten"
    );
}

#[test]
fn snapshot_without_session_id_is_rejected() {
    let mut value: Value = serde_json::from_str(fixture("event-snapshot")).unwrap();
    value["snapshot"]["session"]
        .as_object_mut()
        .unwrap()
        .remove("id");
    let decoded: Result<DaemonEvent, _> = serde_json::from_value(value);
    assert!(decoded.is_err(), "a session without an id is malformed");
}

// --- Framing -----------------------------------------------------------------

#[test]
fn decoder_collects_multiple_frames_from_one_chunk() {
    let mut decoder = FrameDecoder::new(MAX_DAEMON_FRAME_BYTES);
    let frames = decoder.append(b"{\"a\":1}\n{\"b\":2}\n").unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0], b"{\"a\":1}");
    assert_eq!(frames[1], b"{\"b\":2}");
    assert_eq!(decoder.pending_bytes(), 0);
}

#[test]
fn decoder_holds_an_unfinished_tail_across_chunks() {
    let mut decoder = FrameDecoder::new(MAX_DAEMON_FRAME_BYTES);
    assert!(decoder.append(b"{\"id\":\"par").unwrap().is_empty());
    assert_eq!(decoder.pending_bytes(), 10);
    let frames = decoder.append(b"tial\"}\n").unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0], b"{\"id\":\"partial\"}");
    assert_eq!(decoder.pending_bytes(), 0, "tail must be released");
}

#[test]
fn decoder_enforces_the_cap_across_chunks() {
    let mut decoder = FrameDecoder::new(8);
    assert!(decoder.append(b"12345").is_ok());
    let error = decoder.append(b"67890").unwrap_err();
    assert_eq!(error.to_string(), "IPC frame exceeded the 32 MB limit.");
}

#[test]
fn decoder_accepts_a_frame_exactly_at_the_cap() {
    let mut decoder = FrameDecoder::new(4);
    let frames = decoder.append(b"1234\n").unwrap();
    assert_eq!(frames, vec![b"1234".to_vec()]);
    assert_eq!(decoder.pending_bytes(), 0);
}

#[test]
fn decoder_reports_an_overlong_frame_before_its_newline() {
    let mut decoder = FrameDecoder::new(4);
    let error = decoder.append(b"12345").unwrap_err();
    assert_eq!(error.to_string(), "IPC frame exceeded the 32 MB limit.");
}

#[test]
fn decoder_keeps_empty_frames_between_newlines() {
    let mut decoder = FrameDecoder::new(MAX_DAEMON_FRAME_BYTES);
    let frames = decoder.append(b"a\n\nb\n").unwrap();
    assert_eq!(frames.len(), 3);
    assert_eq!(frames[1], b"");
}

// --- Connection admission plane ----------------------------------------------

/// Every case the remote first-frame gate must classify exactly like the
/// TypeScript host, plus the fixed refusal/consent wordings it emits.
#[test]
fn auth_first_frame_classification() {
    use openbutler_protocol::auth::{self, FirstFrame};

    let vector: Value =
        serde_json::from_str(fixture("auth-first-frame")).expect("vector must be valid JSON");
    let cases = vector["cases"]
        .as_array()
        .expect("vector needs a cases array");
    assert!(!cases.is_empty(), "vector must carry cases");

    for case in cases {
        let name = case["name"].as_str().expect("case needs a name");
        let frame = case["frame"].clone();
        let classified = auth::classify_first_frame(&frame);
        let (kind, id) = match &classified {
            FirstFrame::Auth { id, token } => {
                assert!(!token.is_empty(), "case {name}: auth token must be kept");
                ("auth", *id)
            }
            FirstFrame::Pair { id, code } => {
                assert!(!code.is_empty(), "case {name}: pair code must be kept");
                ("pair", *id)
            }
            FirstFrame::Refused { id } => ("refused", *id),
        };
        assert_eq!(kind, case["expect"]["kind"], "case {name}");
        assert_eq!(id, case["expect"]["id"], "case {name}");
    }

    let messages = &vector["messages"];
    assert_eq!(
        auth::REMOTE_FIRST_FRAME_REFUSED,
        messages["firstFrameRefused"]
    );
    assert_eq!(auth::AUTH_FAILED, messages["authFailed"]);
    assert_eq!(auth::PAIR_FAILED, messages["pairFailed"]);

    let success = &vector["success"];
    let auth_result = auth::AuthSuccess {
        authenticated: true,
    };
    assert_eq!(
        serde_json::to_value(auth_result).expect("auth success serializes"),
        success["auth"]
    );
    let pair_result = auth::PairSuccess {
        token: "obt_example_minted_token".to_owned(),
    };
    assert_eq!(
        serde_json::to_value(pair_result).expect("pair success serializes"),
        success["pair"]
    );
}

// --- Activity ledger ----------------------------------------------------------

#[test]
fn ledger_feed_rows_round_trip() {
    use openbutler_protocol::ledger::{LedgerAction, LedgerEvent};

    let event: LedgerEvent = roundtrip(fixture("ledger-event"));
    assert_eq!(event.id, "evt-01");
    assert_eq!(event.dedupe_key, "schedule:nightly:manual:aaaabbbb");
    assert_eq!(event.attempts, 2);
    assert_eq!(event.error.as_deref(), Some("boom"));

    let sparse: LedgerEvent = roundtrip(fixture("ledger-event-nulls"));
    assert_eq!(sparse.workspace_root, None);
    assert_eq!(sparse.error, None);
    assert_eq!(sparse.payload, serde_json::json!({}));

    let action: LedgerAction = roundtrip(fixture("ledger-action"));
    assert_eq!(action.request_id, "req-01");
    assert_eq!(action.name, "shell");
    assert_eq!(action.event_id, None);
    assert_eq!(action.effect["change"], "execute");

    // The source casts parse-failure fallbacks straight through; the wire
    // must keep them as empty values rather than refusing the row.
    let fallbacks: LedgerAction = roundtrip(fixture("ledger-action-fallbacks"));
    assert_eq!(fallbacks.effect, serde_json::json!({}));
    assert_eq!(fallbacks.input, serde_json::json!({}));
    assert_eq!(fallbacks.result, None);
    assert_eq!(fallbacks.event_id.as_deref(), Some("evt-01"));
}

// --- Sidecar door -------------------------------------------------------------

#[test]
fn sidecar_commands_round_trip() {
    use openbutler_protocol::sidecar::{SidecarCall, SidecarCommand};

    let SidecarCommand::Call(call) = roundtrip(fixture("sidecar-command-call")) else {
        panic!("the call vector decodes as a call command");
    };
    let SidecarCall {
        id,
        provider,
        model,
        tools,
        ..
    } = call;
    assert_eq!(id, "call-01");
    assert_eq!(provider.key, "openai-compatible");
    assert_eq!(provider.url.as_deref(), Some("https://gateway.example/v1"));
    assert_eq!(provider.api_key.as_deref(), Some("sk-live-not-logged"));
    assert_eq!(model, "mimo-v2.6-flash");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "shell");
    // No execute channel exists in the contract: the schema is all the
    // SDK ever receives, and the vector's key set proves it.
    let value = serde_json::to_value(&tools[0]).expect("tool serializes");
    let object = value.as_object().expect("tool is an object");
    assert_eq!(object.len(), 3, "name, description, parameters only");
    assert!(!object.contains_key("execute"), "no execute channel");

    let SidecarCommand::Cancel { id } = roundtrip(fixture("sidecar-command-cancel")) else {
        panic!("the cancel vector decodes as a cancel command");
    };
    assert_eq!(id, "call-01");
}

#[test]
fn sidecar_handshake_round_trips() {
    use openbutler_protocol::sidecar::{SIDECAR_PROTOCOL_VERSION, SidecarReady};

    let ready: SidecarReady = roundtrip(fixture("sidecar-handshake"));
    assert_eq!(ready.protocol_version, SIDECAR_PROTOCOL_VERSION);
    assert_eq!(ready.ai_sdk_version, "6.0.332");
    assert_eq!(
        ready.providers,
        vec!["openai", "openai-compatible", "anthropic"]
    );
}

#[test]
fn sidecar_events_round_trip() {
    use openbutler_protocol::sidecar::{SidecarErrorKind, SidecarEvent};

    let SidecarEvent::TextDelta { id, delta } = roundtrip(fixture("sidecar-event-text-delta"))
    else {
        panic!("text vector decodes as text-delta");
    };
    assert_eq!((id.as_str(), delta.as_str()), ("call-01", "Hel"));

    let SidecarEvent::ReasoningDelta {
        id,
        delta,
        signature,
    } = roundtrip(fixture("sidecar-event-reasoning-delta"))
    else {
        panic!("reasoning vector decodes as reasoning-delta");
    };
    assert_eq!(id, "call-01");
    assert_eq!(delta, "The user wants");
    let signature = signature.expect("the provider signature must survive");
    assert!(
        signature.starts_with("EqQB"),
        "signatures pass through untouched"
    );

    let SidecarEvent::ToolCall {
        id,
        name,
        call_id,
        arguments,
    } = roundtrip(fixture("sidecar-event-tool-call"))
    else {
        panic!("tool vector decodes as tool-call");
    };
    assert_eq!((id.as_str(), name.as_str()), ("call-01", "shell"));
    assert_eq!(call_id.as_deref(), Some("toolu_01"));
    assert_eq!(arguments["command"], "ls -la");

    let SidecarEvent::Usage {
        id,
        prompt_tokens,
        reasoning_tokens,
        total_tokens,
        ..
    } = roundtrip(fixture("sidecar-event-usage"))
    else {
        panic!("usage vector decodes as usage");
    };
    assert_eq!(id, "call-01");
    assert_eq!(prompt_tokens, Some(1204));
    assert_eq!(reasoning_tokens, Some(40));
    assert_eq!(total_tokens, Some(1292));

    let SidecarEvent::Finish {
        id,
        finish_reason,
        provider_metadata,
    } = roundtrip(fixture("sidecar-event-finish"))
    else {
        panic!("finish vector decodes as finish");
    };
    assert_eq!(id, "call-01");
    assert_eq!(finish_reason.as_deref(), Some("tool-calls"));
    let metadata = provider_metadata.expect("provider metadata must survive");
    assert_eq!(metadata["openai"]["cachedPromptTokens"], 1024);

    let SidecarEvent::Error {
        id,
        kind,
        retryable,
        status,
        retry_after_ms,
        message,
    } = roundtrip(fixture("sidecar-event-error"))
    else {
        panic!("error vector decodes as error");
    };
    assert_eq!(id, "call-01");
    assert_eq!(kind, SidecarErrorKind::RateLimit);
    assert!(retryable);
    assert_eq!(status, Some(429));
    assert_eq!(retry_after_ms, Some(2000));
    assert_eq!(message.as_deref(), Some("slow down"));
}
