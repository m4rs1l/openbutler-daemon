//! Compaction's engine (C2b-2b): budget arithmetic, the summarisation
//! call through the real door, the guard sentences as errors, refusals
//! as data, and the sliced fallback chain - all against the fake
//! sidecar (behavior keyed by the call-id tag).
//!
//! Source: legacy `compaction.ts` L183-410 (`compactConversation`) and
//! L684-916 (`compactBySlicing`, `streamNotesRequest`), read 2026-11-18.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::time::Duration;

use openbutler_daemon::compaction::{
    COMPACTION_SUMMARY_MARKER, CompactOutcome, CompactionRequest, EngineHooks, ImpossibleReason,
    SkipReason, compact_conversation, continue_message, estimate_context_tokens,
};
use openbutler_daemon::sidecar::{DoorConfig, SidecarDoor};
use openbutler_protocol::sidecar::SidecarProvider;
use serde_json::json;

fn script_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-sidecar.mjs")
}

fn config(log_file: Option<PathBuf>) -> DoorConfig {
    DoorConfig {
        node_program: PathBuf::from("node"),
        node_args: Vec::new(),
        script: script_path(),
        mode: String::new(),
        providers: Some(vec!["ok".to_owned()]),
        log_file,
        env: Vec::new(),
        spawn_timeout: Duration::from_millis(4000),
        idle_kill: Duration::from_secs(3600),
        drain_max: Duration::from_millis(900),
        queue_max_wait: Duration::from_millis(2500),
        first_event_timeout: Duration::from_millis(700),
        inter_event_timeout: Duration::from_millis(700),
        concurrency: 4,
        line_cap: 64 * 1024,
    }
}

fn provider() -> SidecarProvider {
    SidecarProvider {
        key: "ok".to_owned(),
        url: None,
        api_key: None,
    }
}

fn request(tag: &str) -> CompactionRequest {
    CompactionRequest {
        call_tag: tag.to_owned(),
        provider: provider(),
        model: "test-model".to_owned(),
        provider_options: None,
        system: String::new(),
        messages: vec![json!({ "role": "user", "content": "hello" })],
        transcript: None,
        tools: json!({}),
        archive_path: None,
        context_window: Some(200_000),
        output_limit: None,
        previous_context: None,
        previous_context_estimate: None,
        force: false,
        recovery: false,
    }
}

fn temp_log(tag: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("ob-engine-{tag}-{}-calls.log", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

fn call_count(log: &PathBuf) -> usize {
    std::fs::read_to_string(log)
        .map(|text| text.lines().filter(|line| !line.is_empty()).count())
        .unwrap_or(0)
}

#[tokio::test]
async fn on_start_fires_once_for_a_forced_attempt_and_never_for_a_skip() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let log = temp_log("onstart");
    let (door, _) = SidecarDoor::spawn(config(Some(log.clone()))).await.unwrap();

    // Below the trigger with no force: the guard returns before the
    // hook — zero fires, zero calls.
    let fired = AtomicUsize::new(0);
    let skipped = request("basic");
    let outcome = compact_conversation(
        &door,
        skipped.clone(),
        EngineHooks {
            on_start: Some(&|| {
                fired.fetch_add(1, Ordering::SeqCst);
            }),
            ..EngineHooks::default()
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        outcome,
        CompactOutcome::Skipped {
            reason: SkipReason::BelowThreshold
        }
    ));
    assert_eq!(
        fired.load(Ordering::SeqCst),
        0,
        "a skipped attempt never opens the row"
    );

    // Forced: the hook fires once before the summarisation call.
    let fired = AtomicUsize::new(0);
    let mut forced = request("basic");
    forced.force = true;
    let outcome = compact_conversation(
        &door,
        forced,
        EngineHooks {
            on_start: Some(&|| {
                fired.fetch_add(1, Ordering::SeqCst);
            }),
            ..EngineHooks::default()
        },
    )
    .await
    .unwrap();
    assert!(matches!(outcome, CompactOutcome::Compacted { .. }));
    assert_eq!(
        fired.load(Ordering::SeqCst),
        1,
        "exactly one row per attempt"
    );
    assert_eq!(call_count(&log), 1, "one summarisation call");
}

#[tokio::test]
async fn the_hooks_fire_around_and_during_the_main_call() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let log = temp_log("hooks");
    let (door, _) = SidecarDoor::spawn(config(Some(log.clone()))).await.unwrap();
    let starts = AtomicUsize::new(0);
    let texts = AtomicUsize::new(0);
    let mut forced = request("basic");
    forced.force = true;
    let hooks = EngineHooks {
        on_start: Some(&|| {
            starts.fetch_add(1, Ordering::SeqCst);
        }),
        on_text: Some(&|_delta: &str| {
            texts.fetch_add(1, Ordering::SeqCst);
        }),
        on_reasoning: None,
    };
    let outcome = compact_conversation(&door, forced, hooks).await.unwrap();
    assert!(matches!(outcome, CompactOutcome::Compacted { .. }));
    assert_eq!(starts.load(Ordering::SeqCst), 1, "the row hook fires once");
    assert_eq!(
        texts.load(Ordering::SeqCst),
        2,
        "both text deltas of the basic fake stream through"
    );
}

#[tokio::test]
async fn without_a_window_the_trigger_never_fires() {
    let log = temp_log("nowindow");
    let (door, _) = SidecarDoor::spawn(config(Some(log.clone()))).await.unwrap();
    let mut request = request("basic");
    request.context_window = None;
    let outcome = compact_conversation(&door, request, EngineHooks::default())
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        CompactOutcome::Skipped {
            reason: SkipReason::NoBudget
        }
    ));
    assert_eq!(call_count(&log), 0, "no window, no call");
    door.shutdown().await;
}

#[tokio::test]
async fn below_the_trigger_is_a_skip_not_a_call() {
    let log = temp_log("below");
    let (door, _) = SidecarDoor::spawn(config(Some(log.clone()))).await.unwrap();
    let outcome = compact_conversation(&door, request("basic"), EngineHooks::default())
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        CompactOutcome::Skipped {
            reason: SkipReason::BelowThreshold
        }
    ));
    assert_eq!(call_count(&log), 0, "below threshold, no call");
    door.shutdown().await;
}

#[tokio::test]
async fn force_compacts_and_builds_the_checkpoint() {
    let (door, _) = SidecarDoor::spawn(config(None)).await.unwrap();
    let mut request = request("basic");
    request.force = true;
    let outcome = compact_conversation(&door, request, EngineHooks::default())
        .await
        .unwrap();
    let CompactOutcome::Compacted { result } = outcome else {
        panic!("expected compaction, got {outcome:?}");
    };
    assert_eq!(result.summary, "Hello world");
    let first = &result.messages[0];
    let content = first["content"].as_str().expect("checkpoint string");
    assert!(
        content.starts_with(COMPACTION_SUMMARY_MARKER),
        "checkpoint first: {content}"
    );
    assert_eq!(
        first["providerOptions"]["openbutler"]["compactionCheckpoint"],
        json!(true)
    );
    assert_eq!(
        result.messages.last().unwrap(),
        &continue_message(),
        "the continue message closes the projection"
    );
    assert_eq!(result.input_tokens, Some(10));
    assert_eq!(result.output_tokens, Some(5));
    assert!(result.before_tokens > 0);
    door.shutdown().await;
}

#[tokio::test]
async fn a_length_finish_is_a_named_refusal() {
    let (door, _) = SidecarDoor::spawn(config(None)).await.unwrap();
    let mut request = request("length");
    request.force = true;
    request.output_limit = Some(16_384);
    let error = compact_conversation(&door, request, EngineHooks::default())
        .await
        .expect_err("length must refuse");
    assert_eq!(
        error,
        "The compaction summary was cut off by the model output limit (16384 tokens requested; 5 tokens used); the original context was kept."
    );
    door.shutdown().await;
}

#[tokio::test]
async fn a_tool_call_finish_is_a_text_only_refusal() {
    let (door, _) = SidecarDoor::spawn(config(None)).await.unwrap();
    let mut request = request("tool");
    request.force = true;
    let error = compact_conversation(&door, request, EngineHooks::default())
        .await
        .expect_err("tool calls must refuse");
    assert_eq!(
        error,
        "The model did not finish a text-only compaction summary; the original context was kept."
    );
    door.shutdown().await;
}

#[tokio::test]
async fn an_empty_summary_is_a_refusal() {
    let (door, _) = SidecarDoor::spawn(config(None)).await.unwrap();
    let mut request = request("empty");
    request.force = true;
    let error = compact_conversation(&door, request, EngineHooks::default())
        .await
        .expect_err("empty must refuse");
    assert_eq!(
        error,
        "The model returned an empty compaction summary; the original context was kept."
    );
    door.shutdown().await;
}

#[tokio::test]
async fn a_stream_error_surfaces_its_message() {
    let (door, _) = SidecarDoor::spawn(config(None)).await.unwrap();
    let mut request = request("error");
    request.force = true;
    let error = compact_conversation(&door, request, EngineHooks::default())
        .await
        .expect_err("stream errors throw");
    assert!(error.contains("slow down"), "error text travels: {error}");
    door.shutdown().await;
}

#[tokio::test]
async fn an_oversized_attempt_is_data_not_an_exception() {
    let (door, _) = SidecarDoor::spawn(config(None)).await.unwrap();
    let mut request = request("basic");
    request.context_window = Some(6_000);
    request.previous_context = Some(50_000);
    let context_estimate = estimate_context_tokens("", &request.messages, &json!({}));
    request.previous_context_estimate = Some(context_estimate);
    let outcome = compact_conversation(&door, request, EngineHooks::default())
        .await
        .unwrap();
    let CompactOutcome::Impossible {
        reason,
        window,
        estimated_tokens,
        shortfall_tokens,
        cause,
    } = outcome
    else {
        panic!("expected impossible, got {outcome:?}");
    };
    assert_eq!(reason, ImpossibleReason::ExceedsWindow);
    assert_eq!(window, Some(6_000));
    assert_eq!(estimated_tokens, 51_579, "active 50_000 + prompt 1_579");
    assert_eq!(shortfall_tokens, 46_603, "1_024 - (6_000 - 51_579)");
    assert!(cause.is_none(), "no slice chain was possible: {cause:?}");
    door.shutdown().await;
}

#[tokio::test]
async fn a_summary_that_does_not_shrink_is_not_applied() {
    let (door, _) = SidecarDoor::spawn(config(None)).await.unwrap();
    let mut request = request("basic");
    // Just past the trigger: the tiny conversation's estimate is the
    // floor, so the checkpoint plus continue message cannot shrink it.
    let context_estimate = estimate_context_tokens("", &request.messages, &json!({}));
    request.previous_context = Some(160_000);
    request.previous_context_estimate = Some(context_estimate);
    let outcome = compact_conversation(&door, request, EngineHooks::default())
        .await
        .unwrap();
    let CompactOutcome::Impossible {
        reason,
        estimated_tokens,
        shortfall_tokens,
        ..
    } = outcome
    else {
        panic!("expected impossible, got {outcome:?}");
    };
    assert_eq!(reason, ImpossibleReason::NoReduction);
    assert!(estimated_tokens >= 1, "the after-estimate is reported");
    assert_eq!(shortfall_tokens, 0, "nothing was short; it simply grew");
    door.shutdown().await;
}

#[tokio::test]
async fn the_sliced_chain_summarises_then_checkpoints() {
    let log = temp_log("chain");
    let (door, _) = SidecarDoor::spawn(config(Some(log.clone()))).await.unwrap();
    let big = "x".repeat(4_000);
    let messages: Vec<serde_json::Value> = (0..6)
        .map(|index| json!({ "role": "user", "content": format!("message {index}: {big}") }))
        .collect();
    let context_estimate = estimate_context_tokens("", &messages, &json!({}));
    let mut request = request("compact");
    request.messages = messages;
    request.context_window = Some(8_000);
    request.previous_context = Some(7_000);
    request.previous_context_estimate = Some(context_estimate);

    let outcome = compact_conversation(&door, request, EngineHooks::default())
        .await
        .unwrap();
    let CompactOutcome::Compacted { result } = outcome else {
        panic!("expected the chain to compact, got {outcome:?}");
    };
    assert_eq!(result.summary, "Hello world", "the forced final call");
    assert_eq!(result.before_tokens, 8_579, "active 7_000 + prompt 1_579");
    assert_eq!(
        call_count(&log),
        3,
        "two notes plus the forced final checkpoint"
    );
    door.shutdown().await;
}

#[tokio::test]
async fn recovery_appends_its_documented_suffix() {
    let (door, _) = SidecarDoor::spawn(config(None)).await.unwrap();
    let mut request = request("echo");
    request.force = true;
    request.recovery = true;
    let outcome = compact_conversation(&door, request, EngineHooks::default())
        .await
        .unwrap();
    let CompactOutcome::Compacted { result } = outcome else {
        panic!("expected compaction, got {outcome:?}");
    };
    assert_eq!(
        result.summary, "recovery-suffix-present",
        "the fake probe saw the recovery suffix on the final user message"
    );
    door.shutdown().await;
}
