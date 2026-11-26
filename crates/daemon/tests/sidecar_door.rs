//! The sidecar door: handshake, framing discipline, cancellation,
//! queue lanes, timeouts and the supervisor state machine — all against
//! the fake sidecar. The real `sidecar.mjs` behaviors (allowlist, stdout
//! hygiene, the SDK execute-lock) are M2b acceptance criteria.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::time::Duration;

use openbutler_daemon::sidecar::{
    DoorConfig, DoorError, DoorState, Lane, SidecarDoor, sidecar_call,
};
use openbutler_protocol::sidecar::{SidecarErrorKind, SidecarEvent};

fn script_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-sidecar.mjs")
}

fn config(mode: &str) -> DoorConfig {
    DoorConfig {
        node_program: PathBuf::from("node"),
        node_args: Vec::new(),
        script: script_path(),
        mode: mode.to_owned(),
        providers: Some(vec!["ok".to_owned()]),
        log_file: None,
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

async fn spawn(
    mode: &str,
) -> Result<(SidecarDoor, openbutler_protocol::sidecar::SidecarReady), DoorError> {
    SidecarDoor::spawn(config(mode)).await
}

/// Collects until the stream ends; the last item, if any, carries the
/// terminal error.
async fn drain(
    stream: &mut openbutler_daemon::sidecar::CallStream,
) -> (Vec<SidecarEvent>, Option<DoorError>) {
    let mut events = Vec::new();
    let mut terminal = None;
    while let Some(item) = stream.next().await {
        match item {
            Ok(event) => events.push(event),
            Err(error) => {
                terminal = Some(error);
                break;
            }
        }
    }
    (events, terminal)
}

/// Wait for a door state instead of guessing a fixed instant: the
/// supervisor is wall-clock based, and under parallel load admission and
/// ticks drift. The contract is the state, not the second.
async fn wait_for_state(door: &SidecarDoor, target: DoorState, cap: Duration) -> DoorState {
    let deadline = std::time::Instant::now() + cap;
    loop {
        let current = door.state();
        if current == target || std::time::Instant::now() >= deadline {
            return current;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn handshake_reports_version_and_validates_the_provider_allowlist() {
    let (door, ready) = spawn("ok").await.expect("healthy handshake");
    assert_eq!(ready.protocol_version, 1);
    assert_eq!(ready.ai_sdk_version, "7.0.173");
    assert_eq!(ready.providers, vec!["ok"]);
    assert_eq!(door.state(), DoorState::Ready);

    // The host refuses a provider the handshake did not report — the
    // allowlist check has two sides.
    let error = door
        .call(Lane::Interactive, sidecar_call("ok:x", "mystery-provider"))
        .await
        .expect_err("unknown provider key must not be sent");
    assert!(matches!(error, DoorError::ProviderNotAllowed(_)), "{error}");
    door.shutdown().await;
}

#[tokio::test]
async fn a_wrong_protocol_version_refuses_the_spawn() {
    let error = spawn("wrong-version")
        .await
        .expect_err("version 99 must be refused");
    assert!(
        matches!(error, DoorError::ProtocolVersionMismatch { .. }),
        "{error}"
    );
}

#[tokio::test]
async fn no_ready_handshake_times_out() {
    let error = spawn("no-ready").await.expect_err("silent child");
    assert!(matches!(error, DoorError::HandshakeTimeout(_)), "{error}");
}

#[tokio::test]
async fn a_healthy_call_streams_deltas_usage_and_finish() {
    let (door, _) = spawn("ok").await.unwrap();
    let mut call = door
        .call(Lane::Interactive, sidecar_call("ok:1", "ok"))
        .await
        .unwrap();
    let (events, terminal) = drain(&mut call).await;

    assert!(terminal.is_none(), "clean finish, not {terminal:?}");
    assert_eq!(events.len(), 4);
    assert!(matches!(&events[0], SidecarEvent::TextDelta { delta, .. } if delta == "Hello"));
    assert!(matches!(&events[1], SidecarEvent::TextDelta { delta, .. } if delta == " world"));
    assert!(matches!(
        &events[2],
        SidecarEvent::Usage {
            prompt_tokens: Some(10),
            ..
        }
    ));
    assert!(
        matches!(&events[3], SidecarEvent::Finish { finish_reason, .. } if finish_reason.as_deref() == Some("stop"))
    );
    door.shutdown().await;
}

#[tokio::test]
async fn a_structured_provider_error_passes_through() {
    let (door, _) = spawn("ok").await.unwrap();
    let mut call = door
        .call(Lane::Background, sidecar_call("error:1", "ok"))
        .await
        .unwrap();
    let (events, terminal) = drain(&mut call).await;

    assert!(
        terminal.is_none(),
        "an error event ends the stream without a door error"
    );
    let Some(SidecarEvent::Error {
        kind,
        retryable,
        status,
        retry_after_ms,
        message,
        ..
    }) = events.into_iter().next()
    else {
        panic!("the structured error arrives as an event");
    };
    assert_eq!(kind, SidecarErrorKind::RateLimit);
    assert!(retryable);
    assert_eq!(status, Some(429));
    assert_eq!(retry_after_ms, Some(1500));
    assert_eq!(message.as_deref(), Some("slow down"));
    door.shutdown().await;
}

#[tokio::test]
async fn a_non_json_line_is_a_protocol_violation_that_kills_the_process() {
    let (door, _) = spawn("ok").await.unwrap();
    let mut call = door
        .call(Lane::Interactive, sidecar_call("bad-line:1", "ok"))
        .await
        .unwrap();
    let (events, terminal) = drain(&mut call).await;

    assert!(events.is_empty(), "no events survive the violation");
    assert!(
        matches!(terminal, Some(DoorError::ProtocolViolation(_))),
        "garbage is a violation, got {terminal:?}"
    );
    // The child is killed, not left running.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(door.state(), DoorState::Dead, "violation kills the process");
    door.shutdown().await;
}

#[tokio::test]
async fn an_oversized_line_is_a_protocol_violation_that_kills_the_process() {
    let (door, _) = spawn("ok").await.unwrap();
    let mut call = door
        .call(Lane::Interactive, sidecar_call("big-line:1", "ok"))
        .await
        .unwrap();
    let (_, terminal) = drain(&mut call).await;

    assert!(
        matches!(terminal, Some(DoorError::ProtocolViolation(_))),
        "over the line cap is a violation, got {terminal:?}"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(door.state(), DoorState::Dead);
    door.shutdown().await;
}

#[tokio::test]
async fn an_unknown_event_type_is_ignored_and_the_call_completes() {
    let (door, _) = spawn("ok").await.unwrap();
    let mut call = door
        .call(Lane::Interactive, sidecar_call("unknown:1", "ok"))
        .await
        .unwrap();
    let (events, terminal) = drain(&mut call).await;

    // The forward-compatible rule: skip + log, never kill.
    assert!(terminal.is_none(), "{terminal:?}");
    let deltas: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            SidecarEvent::TextDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, vec!["after-unknown"], "only known events surface");
    door.shutdown().await;
}

#[tokio::test]
async fn cancel_in_flight_drops_late_events_and_ends_the_call() {
    let (door, _) = spawn("ok").await.unwrap();
    let mut call = door
        .call(Lane::Interactive, sidecar_call("abort:1", "ok"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await; // sent
    call.cancel().await;
    let (events, terminal) = drain(&mut call).await;

    assert!(events.is_empty(), "late events are dropped");
    assert!(
        matches!(terminal, Some(DoorError::Cancelled)),
        "{terminal:?}"
    );
    door.shutdown().await;
}

#[tokio::test]
async fn a_queued_cancel_is_never_sent_to_the_sidecar() {
    let mut cfg = config("ok");
    cfg.concurrency = 1;
    let (door, _) = SidecarDoor::spawn(cfg).await.unwrap();
    let log = std::env::temp_dir().join(format!("ob-door-log-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&log);
    // Rebuild with the receipt log for this door.
    door.shutdown().await;
    let mut cfg = config("ok");
    cfg.concurrency = 1;
    cfg.log_file = Some(log.clone());
    let (door, _) = SidecarDoor::spawn(cfg).await.unwrap();

    let mut occupy = door
        .call(Lane::Interactive, sidecar_call("hang:hold", "ok"))
        .await
        .unwrap();
    let mut queued = door
        .call(Lane::Interactive, sidecar_call("ok:queued", "ok"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await; // queued is behind the holder
    queued.cancel().await;
    let (_, terminal) = drain(&mut queued).await;
    assert!(
        matches!(terminal, Some(DoorError::Cancelled)),
        "{terminal:?}"
    );

    // Release the holder; whatever arrives after must not be for the
    // cancelled call — and the log proves the sidecar never saw it.
    occupy.cancel().await;
    let _ = drain(&mut occupy).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let log_text = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        !log_text.contains("ok:queued"),
        "queued cancel must never be sent: {log_text}"
    );
    door.shutdown().await;
}

#[tokio::test]
async fn interactive_calls_jump_the_queue() {
    let mut cfg = config("ok");
    cfg.concurrency = 1;
    let (door, _) = SidecarDoor::spawn(cfg).await.unwrap();

    let mut hold = door
        .call(Lane::Interactive, sidecar_call("hang:hold", "ok"))
        .await
        .unwrap();
    let mut background = door
        .call(Lane::Background, sidecar_call("ok:bg", "ok"))
        .await
        .unwrap();
    let mut interactive = door
        .call(Lane::Interactive, sidecar_call("ok:person", "ok"))
        .await
        .unwrap();

    hold.cancel().await;
    let _ = drain(&mut hold).await;

    // The interactive waiter is admitted before the earlier background one.
    let (events, terminal) = drain(&mut interactive).await;
    assert!(terminal.is_none() && !events.is_empty(), "person ran first");
    let (_, bg_terminal) = drain(&mut background).await;
    assert!(bg_terminal.is_none(), "background ran after");
    door.shutdown().await;
}

#[tokio::test]
async fn a_queued_call_expires_with_an_explicit_error() {
    let mut cfg = config("ok");
    cfg.concurrency = 1;
    cfg.queue_max_wait = Duration::from_millis(250);
    let (door, _) = SidecarDoor::spawn(cfg).await.unwrap();

    let mut hold = door
        .call(Lane::Interactive, sidecar_call("hang:hold", "ok"))
        .await
        .unwrap();
    let mut queued = door
        .call(Lane::Interactive, sidecar_call("ok:never", "ok"))
        .await
        .unwrap();
    // The queue deadline is a stream outcome: the call returns a handle
    // immediately so it can be cancelled while queued.
    let (_, terminal) = drain(&mut queued).await;
    assert!(
        matches!(terminal, Some(DoorError::QueueTimeout)),
        "the queue deadline arrives on the stream, got {terminal:?}"
    );

    hold.cancel().await;
    let _ = drain(&mut hold).await;
    door.shutdown().await;
}

#[tokio::test]
async fn the_first_event_timeout_starts_at_send_not_at_queue_entry() {
    let mut cfg = config("ok");
    cfg.concurrency = 1;
    cfg.queue_max_wait = Duration::from_millis(4000);
    cfg.first_event_timeout = Duration::from_millis(300);
    let (door, _) = SidecarDoor::spawn(cfg).await.unwrap();

    let mut hold = door
        .call(Lane::Interactive, sidecar_call("chatty:hold", "ok"))
        .await
        .unwrap();
    // Queued at once; the holder keeps the slot for ~350ms of queue time.
    let mut queued = door
        .call(Lane::Interactive, sidecar_call("hang:late", "ok"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(350)).await;
    hold.cancel().await;
    let _ = drain(&mut hold).await;

    // Now the queued call is sent; its clock starts here and the silent
    // sidecar misses 300ms — a first-event timeout, not a queue timeout.
    let (_, terminal) = drain(&mut queued).await;
    assert!(
        matches!(terminal, Some(DoorError::FirstEventTimeout)),
        "clock starts at send, got {terminal:?}"
    );
    door.shutdown().await;
}

#[tokio::test]
async fn the_sidecar_idle_kills_only_with_zero_inflight_and_resets_on_a_new_call() {
    let mut cfg = config("ok");
    cfg.idle_kill = Duration::from_millis(600);
    let (door, _) = SidecarDoor::spawn(cfg).await.unwrap();

    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(door.state(), DoorState::Ready, "timer not expired yet");

    let mut call = door
        .call(Lane::Interactive, sidecar_call("ok:late", "ok"))
        .await
        .unwrap();
    let _ = drain(&mut call).await; // activity resets the timer

    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(
        door.state(),
        DoorState::Ready,
        "the call reset the idle timer"
    );

    tokio::time::sleep(Duration::from_millis(500)).await;
    let state = wait_for_state(&door, DoorState::Dead, Duration::from_millis(1500)).await;
    assert_eq!(state, DoorState::Dead, "idle kills");
    door.shutdown().await;
}

#[tokio::test]
async fn draining_force_kills_after_its_maximum() {
    let mut cfg = config("ok");
    cfg.idle_kill = Duration::from_millis(200);
    cfg.drain_max = Duration::from_millis(350);
    let (door, _) = SidecarDoor::spawn(cfg).await.unwrap();

    // A call that never ends keeps inflight above zero past the idle mark.
    let mut stuck = door
        .call(Lane::Interactive, sidecar_call("chatty:forever", "ok"))
        .await
        .unwrap();
    let state = wait_for_state(&door, DoorState::Dead, Duration::from_millis(1500)).await;

    assert_eq!(state, DoorState::Dead, "draining gives up and force-kills");
    let (_, terminal) = drain(&mut stuck).await;
    assert!(
        matches!(terminal, Some(DoorError::ProcessExited)),
        "the in-flight call dies with the process, got {terminal:?}"
    );
    door.shutdown().await;
}

#[tokio::test]
async fn dropping_the_door_closes_stdin_and_the_child_exits() {
    let (door, _) = spawn("ok").await.unwrap();
    door.shutdown().await; // closes stdin, waits for exit
    // A clean shutdown that returns is the assertion: the EOF rule is the
    // portable orphan guard on every platform.
}
