//! M2b acceptance: the real sidecar against a local OpenAI-compatible
//! mock — no network, no keys. Criteria 1-3 of `docs/sidecar-door.md`:
//! the SDK stops at a tool call instead of running its own loop, the
//! sidecar refuses unknown providers and keeps keys off stdout, and the
//! pinned SDK version plus per-provider lazy registry are what the
//! handshake reports.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{Response, StatusCode, header};
use axum::routing::post;
use openbutler_daemon::sidecar::{DoorConfig, Lane, SidecarDoor, sidecar_call};
use openbutler_protocol::sidecar::{SidecarCall, SidecarEvent, SidecarTool};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// The eleven keys the registry embeds — same order as the conformance
/// vector.
const PROVIDERS: [&str; 11] = [
    "anthropic",
    "cerebras",
    "cohere",
    "deepinfra",
    "google",
    "groq",
    "mistral",
    "openai",
    "openai-compatible",
    "togetherai",
    "xai",
];

#[derive(Clone, Default)]
struct Mock {
    /// Scenario per request: the tool test asserts exactly one call, so
    /// an SDK-side loop cannot hide.
    requests: Arc<Mutex<Vec<String>>>,
    /// The raw request bodies, so wire-level pass-through (budgets,
    /// provider options) can be asserted where the provider sees them.
    bodies: Arc<Mutex<Vec<String>>>,
}

fn sse(body: &str) -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

async fn chat(
    State(mock): State<Mock>,
    Path(scenario): Path<String>,
    body: String,
) -> Response<Body> {
    mock.bodies.lock().unwrap().push(body);
    mock.requests.lock().unwrap().push(scenario.clone());
    match scenario.as_str() {
        "basic" => sse(
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"}}]}\n\n\
             data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"}}]}\n\n\
             data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n\
             data: [DONE]\n\n",
        ),
        "tool" => sse(
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_mock_1\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{\\\"city\\\":\\\"Berlin\\\"}\"}}]}}]}\n\n\
             data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
             data: [DONE]\n\n",
        ),
        "rate" => Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header(header::RETRY_AFTER, "2")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                "{\"error\":{\"message\":\"slow down\",\"type\":\"rate_limit_error\"}}",
            ))
            .unwrap(),
        "slow" => {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            sse(
                "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"late\"}}]}\n\n\
                 data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                 data: [DONE]\n\n",
            )
        }
        _ => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("unknown scenario"))
            .unwrap(),
    }
}

async fn mock_server() -> (SocketAddr, Mock) {
    let mock = Mock::default();
    let app = Router::new()
        .route("/{scenario}/chat/completions", post(chat))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, mock)
}

fn entry_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("sidecar")
        .join("src")
        .join("entry.mjs")
}

fn config() -> DoorConfig {
    DoorConfig {
        node_program: PathBuf::from("node"),
        node_args: Vec::new(),
        script: entry_script(),
        mode: String::new(),
        providers: Some(PROVIDERS.iter().map(|key| (*key).to_owned()).collect()),
        log_file: None,
        env: Vec::new(),
        spawn_timeout: Duration::from_secs(15),
        idle_kill: Duration::from_secs(3600),
        drain_max: Duration::from_millis(900),
        queue_max_wait: Duration::from_millis(2500),
        first_event_timeout: Duration::from_secs(10),
        inter_event_timeout: Duration::from_secs(30),
        concurrency: 4,
        line_cap: 64 * 1024,
    }
}

/// A call pointed at the mock for one scenario.
fn mock_call(id: &str, scenario: &str, addr: SocketAddr) -> SidecarCall {
    let mut call = sidecar_call(id, "openai-compatible");
    call.provider.url = Some(format!("http://{addr}/{scenario}"));
    call.model = "test-model".to_owned();
    call.messages = json!([
        { "role": "user", "content": "hi" }
    ]);
    call
}

async fn drain(
    stream: &mut openbutler_daemon::sidecar::CallStream,
) -> (
    Vec<SidecarEvent>,
    Option<openbutler_daemon::sidecar::DoorError>,
) {
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

#[tokio::test]
async fn handshake_reports_the_pinned_sdk_and_all_providers() {
    let (_addr, _mock) = mock_server().await;
    let (door, ready) = SidecarDoor::spawn(config())
        .await
        .expect("real sidecar boots");
    assert_eq!(ready.protocol_version, 1);
    assert_eq!(
        ready.ai_sdk_version, "7.0.173",
        "the pinned SDK, not a surprise"
    );
    assert_eq!(
        ready.providers, PROVIDERS,
        "the whole registry, alphabetically"
    );
    assert_eq!(door.state(), openbutler_daemon::sidecar::DoorState::Ready);
    door.shutdown().await;
}

#[tokio::test]
async fn a_real_model_call_streams_text_usage_and_finish() {
    let (addr, _mock) = mock_server().await;
    let (door, _) = SidecarDoor::spawn(config()).await.unwrap();

    let mut stream = door
        .call(Lane::Interactive, mock_call("basic:1", "basic", addr))
        .await
        .unwrap();
    let (events, terminal) = drain(&mut stream).await;
    assert!(terminal.is_none(), "{terminal:?}");

    let deltas: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            SidecarEvent::TextDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, vec!["Hello", " world"]);

    assert!(
        events.iter().any(|event| matches!(
            event,
            SidecarEvent::Usage {
                prompt_tokens: Some(10),
                completion_tokens: Some(5),
                total_tokens: Some(15),
                ..
            }
        )),
        "usage arrives: {events:?}"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        SidecarEvent::Finish { finish_reason, .. } if finish_reason.as_deref() == Some("stop")
    )));
    door.shutdown().await;
}

#[tokio::test]
async fn tools_end_the_stream_at_the_tool_call_without_an_sdk_loop() {
    let (addr, mock) = mock_server().await;
    let (door, _) = SidecarDoor::spawn(config()).await.unwrap();

    let mut call = mock_call("tool:1", "tool", addr);
    call.tools = vec![SidecarTool {
        name: "get_weather".to_owned(),
        description: Some("Look up the weather".to_owned()),
        parameters: json!({
            "type": "object",
            "properties": { "city": { "type": "string" } },
            "required": ["city"]
        }),
    }];
    let mut stream = door.call(Lane::Interactive, call).await.unwrap();
    let (events, terminal) = drain(&mut stream).await;
    assert!(terminal.is_none(), "{terminal:?}");

    let tool = events
        .iter()
        .find_map(|event| match event {
            SidecarEvent::ToolCall {
                name,
                arguments,
                call_id,
                ..
            } => Some((name, arguments, call_id)),
            _ => None,
        })
        .expect("the tool call surfaces complete");
    assert_eq!(tool.0, "get_weather");
    assert_eq!(tool.1, &json!({ "city": "Berlin" }));
    assert_eq!(tool.2.as_deref(), Some("call_mock_1"));
    assert!(
        events.iter().any(|event| matches!(
            event,
            SidecarEvent::Finish { finish_reason, .. } if finish_reason.as_deref() == Some("tool-calls")
        )),
        "finish carries the tool-calls reason: {events:?}"
    );

    // The core of criterion 1: the SDK asked the model exactly once —
    // no second step, no loop of its own.
    let requests = mock.requests.lock().unwrap().clone();
    assert_eq!(
        requests,
        vec!["tool".to_owned()],
        "exactly one model request"
    );
    door.shutdown().await;
}

#[tokio::test]
async fn a_429_maps_to_the_rate_limit_taxonomy() {
    let (addr, _mock) = mock_server().await;
    let (door, _) = SidecarDoor::spawn(config()).await.unwrap();

    let mut stream = door
        .call(Lane::Interactive, mock_call("rate:1", "rate", addr))
        .await
        .unwrap();
    let (events, terminal) = drain(&mut stream).await;
    assert!(terminal.is_none(), "a structured error is not a door error");

    let Some(SidecarEvent::Error {
        kind,
        retryable,
        status,
        retry_after_ms,
        message,
        ..
    }) = events.first()
    else {
        panic!("the error event arrives: {events:?}");
    };
    assert_eq!(
        *kind,
        openbutler_protocol::sidecar::SidecarErrorKind::RateLimit
    );
    assert!(retryable);
    assert_eq!(*status, Some(429));
    assert_eq!(*retry_after_ms, Some(2000), "retry-after seconds become ms");
    assert!(message.as_deref().unwrap_or("").contains("slow down"));
    door.shutdown().await;
}

#[tokio::test]
async fn cancel_aborts_a_real_stream_quickly() {
    let (addr, _mock) = mock_server().await;
    let (door, _) = SidecarDoor::spawn(config()).await.unwrap();

    let mut stream = door
        .call(Lane::Interactive, mock_call("slow:1", "slow", addr))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    stream.cancel().await;
    let (events, terminal) = drain(&mut stream).await;

    assert!(events.is_empty(), "late events are dropped: {events:?}");
    assert!(
        matches!(
            terminal,
            Some(openbutler_daemon::sidecar::DoorError::Cancelled)
        ),
        "{terminal:?}"
    );
    door.shutdown().await;
}

/// Criterion 3 (lazy half): one call must load exactly one provider
/// module. `NODE_DEBUG=esm` prints every module load to stderr — after
/// an openai-compatible call only that package may appear (the ready
/// line legitimately contains the word "anthropic" as a key).
#[tokio::test]
async fn providers_load_lazily_one_call_at_a_time() {
    let mut child = tokio::process::Command::new("node")
        .env("NODE_DEBUG", "esm")
        .arg(entry_script())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("node runs the entry");
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    // Drain stderr concurrently: NODE_DEBUG output fills the pipe long
    // before the call is answered (the same reason the door itself
    // drains stderr — a full pipe freezes the child).
    let stderr_drain = tokio::spawn(async move {
        let mut debug = String::new();
        let mut reader = BufReader::new(stderr);
        let mut chunk = String::new();
        while reader.read_line(&mut chunk).await.unwrap() > 0 {
            debug.push_str(&chunk);
            chunk.clear();
        }
        debug
    });

    let mut lines = BufReader::new(stdout).lines();

    let ready = lines.next_line().await.unwrap().unwrap();
    assert!(ready.contains("\"type\":\"ready\""));

    // No mock needed: the module loads before the connection fails.
    let call = "{\"type\":\"call\",\"id\":\"lazy:1\",\"provider\":{\"key\":\"openai-compatible\",\"url\":\"http://127.0.0.1:1/v1\"},\"model\":\"m\",\"messages\":[]}";
    stdin
        .write_all(format!("{call}\n").as_bytes())
        .await
        .unwrap();
    stdin.flush().await.unwrap();

    // Wait for the call's error event, then close stdin (EOF exit) and
    // drain the debug log.
    loop {
        let line = lines
            .next_line()
            .await
            .unwrap()
            .expect("an event answers the call");
        let value: serde_json::Value = serde_json::from_str(&line).expect("stdout stays JSON");
        if value["id"] == "lazy:1" {
            break;
        }
    }
    drop(stdin);
    let debug = stderr_drain.await.unwrap();
    let _ = child.wait().await;

    assert!(
        debug.contains("@ai-sdk/openai-compatible/dist"),
        "the called provider loads"
    );
    for key in PROVIDERS {
        if key == "openai-compatible" {
            continue;
        }
        let needle = format!("@ai-sdk/{key}/dist");
        assert!(
            !debug.contains(&needle),
            "`{key}` must stay unloaded until asked for"
        );
    }
}

/// Direct spawn — no host gate in front: the sidecar itself must refuse
/// an unknown provider key, and nothing on stdout may ever carry a key.
#[tokio::test]
async fn the_sidecar_refuses_unknown_keys_and_never_prints_a_secret() {
    let mut child = tokio::process::Command::new("node")
        .arg(entry_script())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("node runs the entry");
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();

    let ready =
        serde_json::from_str::<serde_json::Value>(lines.next_line().await.unwrap().unwrap().trim())
            .unwrap();
    assert_eq!(ready["type"], "ready");

    let secret = "sk-secret-XYZ-do-not-print";
    let call = json!({
        "type": "call",
        "id": "mystery:1",
        "provider": { "key": "mystery-provider", "url": "http://127.0.0.1:1/v1", "apiKey": secret },
        "model": "anything",
        "messages": []
    });
    stdin
        .write_all(format!("{call}\n").as_bytes())
        .await
        .unwrap();
    stdin.flush().await.unwrap();

    let response =
        serde_json::from_str::<serde_json::Value>(lines.next_line().await.unwrap().unwrap().trim())
            .expect("every stdout line is a JSON event");
    assert_eq!(response["type"], "error");
    assert_eq!(response["kind"], "invalid-request");
    assert_eq!(response["id"], "mystery:1");

    // Hygiene: everything the sidecar ever wrote is scanned — the
    // secret must not appear on stdout anywhere.
    let mut transcript = format!("{}\n{}\n", ready, response);
    drop(stdin);
    while let Some(line) = lines.next_line().await.unwrap() {
        transcript.push_str(&line);
        transcript.push('\n');
    }
    let _ = child.wait().await;
    assert!(
        !transcript.contains(secret),
        "a key leaked onto protocol stdout: {transcript}"
    );
}

#[tokio::test]
async fn budget_and_provider_options_reach_the_provider_wire() {
    let (addr, mock) = mock_server().await;
    let (door, _) = SidecarDoor::spawn(config()).await.unwrap();

    let mut call = mock_call("budget:1", "basic", addr);
    call.max_output_tokens = Some(16_384);
    call.provider_options = Some(serde_json::json!({
        "openai-compatible": { "reasoningEffort": "low" }
    }));
    let mut stream = door.call(Lane::Interactive, call).await.unwrap();
    let (events, terminal) = drain(&mut stream).await;
    assert!(terminal.is_none(), "{terminal:?}");

    let bodies = mock.bodies.lock().unwrap();
    assert_eq!(bodies.len(), 1, "exactly one provider request");
    let body = &bodies[0];
    assert!(
        body.contains("\"max_tokens\":16384"),
        "the output ceiling reaches the provider: {body}"
    );
    assert!(
        body.contains("\"reasoning_effort\":\"low\""),
        "request-level provider options reach the provider under its own namespace: {body}"
    );
    door.shutdown().await;
}
