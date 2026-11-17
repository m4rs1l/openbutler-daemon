//! c1b — the agent turn loop against the real door: admission persistence,
//! the transcript mapping rules, and the turn's outcome stamping.
//!
//! Sources: legacy `src/agent-session.ts` — `runQueuedMessages` (L2175),
//! `addUserEntry` (L1957), `finishTurn` (L1976), `appendStreamedText`
//! (L1335), `appendStreamedReasoning` (L1382), `finishStreamedReasoning`
//! (L1412), `queuedUserMessageToModelMessage` (L3336).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxPath, State};
use axum::http::{Response, StatusCode, header};
use axum::routing::post;
use openbutler_daemon::agent::{TurnRequest, TurnRunner};
use openbutler_daemon::sidecar::{DoorConfig, SidecarDoor};
use openbutler_daemon::state::{SaveOutcome, parse_record, save_session};
use openbutler_protocol::SessionEntryMeta;
use openbutler_protocol::entry::{SessionEntry, TurnOutcome, UserEntrySource};
use serde_json::{Value, json};

// --- fixtures ----------------------------------------------------------------

fn temp_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "ob-agent-loop-{tag}-{}-{nanos:x}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const SESSION_ID: &str = "22222222-2222-4222-8222-222222222222";

/// Seeds an empty session through the real write path (c1a), then the loop
/// opens it the way the runtime will.
fn seed_session(root: &Path) {
    let json = format!(
        r#"{{
  "version": 1,
  "id": "{SESSION_ID}",
  "title": "Loop session",
  "createdAt": "2026-01-01T00:00:00.000Z",
  "updatedAt": "2026-01-01T00:00:00.000Z",
  "providerId": "mock",
  "modelId": "test-model",
  "entries": [],
  "conversation": []
}}"#
    );
    let record = parse_record(&json).expect("seed record parses");
    match save_session(root, &record, true).expect("seed save") {
        SaveOutcome::Written(_) => {}
        SaveOutcome::Skipped => panic!("allowEmpty writes the seed"),
    }
}

fn saved(root: &Path) -> openbutler_daemon::state::SessionRecord {
    let path = root
        .join("sessions")
        .join(SESSION_ID)
        .join(format!("{SESSION_ID}.json"));
    let text = std::fs::read_to_string(&path).expect("session file exists");
    parse_record(&text).expect("session file parses")
}

fn request(scenario: &str, addr: SocketAddr, source: UserEntrySource) -> TurnRequest {
    TurnRequest {
        text: "hello loop".to_owned(),
        source,
        provider_key: "openai-compatible".to_owned(),
        provider_url: Some(format!("http://{addr}/{scenario}")),
        provider_api_key: Some("sk-test-not-a-real-key".to_owned()),
        model: "test-model".to_owned(),
        system: None,
    }
}

// --- the door ---------------------------------------------------------------

fn entry_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("sidecar")
        .join("src")
        .join("entry.mjs")
}

fn door_config() -> DoorConfig {
    DoorConfig {
        node_program: PathBuf::from("node"),
        node_args: Vec::new(),
        script: entry_script(),
        mode: String::new(),
        providers: Some(vec!["openai-compatible".to_owned()]),
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

async fn spawn_door() -> SidecarDoor {
    let (door, _ready) = SidecarDoor::spawn(door_config()).await.expect("door boots");
    door
}

// --- the mock provider ------------------------------------------------------

#[derive(Clone, Default)]
struct Mock {
    bodies: Arc<Mutex<Vec<String>>>,
}

fn sse(body: &str) -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

fn chunk(delta: Value) -> String {
    format!(
        "data: {}\n\n",
        json!({
            "id": "c1", "object": "chat.completion.chunk", "created": 1,
            "model": "m", "choices": [{ "index": 0, "delta": delta }]
        })
    )
}

async fn chat(
    State(mock): State<Mock>,
    AxPath(scenario): AxPath<String>,
    body: String,
) -> Response<Body> {
    mock.bodies.lock().unwrap().push(body);
    match scenario.as_str() {
        "basic" => sse(&format!(
            "{}{}data: {{\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n",
            chunk(json!({"role": "assistant", "content": "Hello"})),
            chunk(json!({"content": " world"})),
        )),
        "many" => {
            let mut out = String::new();
            for index in 0..10 {
                out.push_str(&chunk(json!({ "content": index.to_string() })));
            }
            out.push_str("data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n");
            sse(&out)
        }
        "think" => sse(&format!(
            "{}{}{}data: {{\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n",
            chunk(json!({"role": "assistant", "content": "A"})),
            chunk(json!({"reasoning_content": "deep thought"})),
            chunk(json!({"content": "B"})),
        )),
        "tool" => sse(
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{\\\"city\\\":\\\"Berlin\\\"}\"}}]}}]}\n\n\
             data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
             data: [DONE]\n\n",
        ),
        "fail" => Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                "{\"error\":{\"message\":\"nope\",\"type\":\"invalid_request_error\"}}",
            ))
            .unwrap(),
        "slow" => {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            sse(&format!(
                "{}{}data: {{\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n",
                chunk(json!({"role": "assistant", "content": "late "})),
                chunk(json!({"content": "answer"})),
            ))
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

// --- assertions ---------------------------------------------------------------

fn user_entry(record: &openbutler_daemon::state::SessionRecord) -> (SessionEntryMeta, String) {
    let SessionEntry::User { meta, text, .. } = record
        .entries
        .iter()
        .find_map(|entry| match entry {
            SessionEntry::User { .. } => Some(entry.clone()),
            _ => None,
        })
        .expect("a user entry exists")
    else {
        unreachable!()
    };
    (meta, text)
}

fn assistant_texts(record: &openbutler_daemon::state::SessionRecord) -> Vec<String> {
    record
        .entries
        .iter()
        .filter_map(|entry| match entry {
            SessionEntry::Assistant { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

// --- tests -------------------------------------------------------------------

#[tokio::test]
async fn the_user_entry_persists_before_the_model_answers_and_carries_the_source_label() {
    let root = temp_root("admit");
    seed_session(&root);
    let (addr, mock) = mock_server().await;
    let door = spawn_door().await;
    let runner = Arc::new(
        TurnRunner::open(&root, SESSION_ID, door)
            .await
            .expect("open"),
    );

    let worker = {
        let runner = Arc::clone(&runner);
        let req = request("slow", addr, UserEntrySource::Terminal);
        tokio::spawn(async move { runner.send(req).await })
    };
    // The slow scenario answers after 1.5s; the admission save must land
    // while the model is still thinking.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let mid = saved(&root);
    let (meta, text) = user_entry(&mid);
    assert_eq!(text, "hello loop", "the transcript keeps the raw text");
    assert!(meta.turn_id.is_some(), "the turn id is minted at admission");
    assert!(meta.turn_outcome.is_none(), "the turn has not settled yet");

    let turn = worker.await.unwrap().expect("turn");

    assert_eq!(turn.outcome, TurnOutcome::Completed);
    assert_eq!(turn.turn_id, meta.turn_id.as_deref().unwrap());

    // The model-visible message carries the source label; the transcript
    // does not (queuedUserMessageToModelMessage vs addUserEntry).
    let body = mock.bodies.lock().unwrap()[0].clone();
    assert!(
        body.contains("[Host context: owner message via terminal]"),
        "the label travels to the model only: {body}"
    );
    assert!(
        saved(&root).entries.iter().all(|entry| match entry {
            SessionEntry::User { text, .. } => !text.contains("[Host context:"),
            _ => true,
        }),
        "labels never enter the transcript"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn deltas_merge_into_one_assistant_entry() {
    let root = temp_root("merge");
    seed_session(&root);
    let (addr, _mock) = mock_server().await;
    let door = spawn_door().await;
    let runner = TurnRunner::open(&root, SESSION_ID, door)
        .await
        .expect("open");

    runner
        .send(request("many", addr, UserEntrySource::Terminal))
        .await
        .expect("turn");

    let record = saved(&root);
    let texts = assistant_texts(&record);
    assert_eq!(texts.len(), 1, "one streamed reply is one entry: {texts:?}");
    assert_eq!(texts[0], "0123456789", "deltas concatenate in order");
    let SessionEntry::Assistant { meta, .. } = &record.entries[1] else {
        panic!("assistant follows the user entry: {:?}", record.entries[1]);
    };
    assert!(meta.turn_id.is_some());
    assert!(meta.at.is_some());

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn reasoning_splices_before_the_streamed_assistant_and_gets_a_duration() {
    let root = temp_root("think");
    seed_session(&root);
    let (addr, _mock) = mock_server().await;
    let door = spawn_door().await;
    let runner = TurnRunner::open(&root, SESSION_ID, door)
        .await
        .expect("open");

    runner
        .send(request("think", addr, UserEntrySource::Terminal))
        .await
        .expect("turn");

    let record = saved(&root);
    let kinds: Vec<&str> = record
        .entries
        .iter()
        .map(|entry| match entry {
            SessionEntry::User { .. } => "user",
            SessionEntry::Reasoning { .. } => "reasoning",
            SessionEntry::Assistant { .. } => "assistant",
            other => panic!("unexpected entry: {other:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        vec!["user", "reasoning", "assistant"],
        "mid-stream thinking lands before the reply it interrupted"
    );
    let SessionEntry::Reasoning {
        meta,
        text,
        duration_ms,
        ..
    } = &record.entries[1]
    else {
        unreachable!()
    };
    assert_eq!(text, "deep thought");
    assert!(meta.turn_id.is_some());
    assert!(duration_ms.is_some(), "the block end stamps a duration");
    assert_eq!(
        assistant_texts(&record),
        vec!["AB"],
        "text resumes in the same entry"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_tool_call_settles_as_not_executed_and_the_turn_completes() {
    let root = temp_root("tool");
    seed_session(&root);
    let (addr, _mock) = mock_server().await;
    let door = spawn_door().await;
    let runner = TurnRunner::open(&root, SESSION_ID, door)
        .await
        .expect("open");

    let turn = runner
        .send(request("tool", addr, UserEntrySource::Terminal))
        .await
        .expect("turn");
    assert_eq!(turn.outcome, TurnOutcome::Completed);

    let record = saved(&root);
    let SessionEntry::Tool {
        meta,
        input,
        result,
        tool,
        running,
        ..
    } = record
        .entries
        .iter()
        .find_map(|entry| match entry {
            SessionEntry::Tool { .. } => Some(entry.clone()),
            _ => None,
        })
        .expect("the tool call lands as an entry")
    else {
        unreachable!()
    };
    assert_eq!(tool.as_deref(), Some("get_weather"));
    assert_eq!(input.unwrap().get("city").unwrap(), "Berlin");
    assert_eq!(result, "The tool call was not executed.");
    assert!(running.is_none(), "a settled entry is not running");
    assert_eq!(meta.turn_id.as_deref(), Some(turn.turn_id.as_str()));
    // Tool execution and the second model batch arrive with c2; the loop
    // ends at the provider's finish for now.
    assert_eq!(
        saved(&root).conversation.len(),
        1,
        "no assistant message without an answer"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_provider_failure_writes_an_error_notice_and_fails_the_turn() {
    let root = temp_root("fail");
    seed_session(&root);
    let (addr, _mock) = mock_server().await;
    let door = spawn_door().await;
    let runner = TurnRunner::open(&root, SESSION_ID, door)
        .await
        .expect("open");

    let turn = runner
        .send(request("fail", addr, UserEntrySource::Terminal))
        .await
        .expect("turn reports");
    assert_eq!(turn.outcome, TurnOutcome::Failed);

    let record = saved(&root);
    let SessionEntry::Notice {
        meta, text, tone, ..
    } = record
        .entries
        .iter()
        .find_map(|entry| match entry {
            SessionEntry::Notice { .. } => Some(entry.clone()),
            _ => None,
        })
        .expect("the failure is announced")
    else {
        unreachable!()
    };
    assert_eq!(
        text, "nope",
        "the provider's message reaches the transcript"
    );
    assert_eq!(tone, Some(openbutler_protocol::entry::NoticeTone::Error));
    assert_eq!(meta.turn_id.as_deref(), Some(turn.turn_id.as_str()));

    let (user_meta, _) = user_entry(&record);
    assert_eq!(user_meta.turn_outcome, Some(TurnOutcome::Failed));
    assert!(user_meta.turn_ended_at.is_some());
    assert!(
        record.conversation.len() == 1,
        "a failed turn keeps only the user message"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn stop_cancels_the_running_turn() {
    let root = temp_root("cancel");
    seed_session(&root);
    let (addr, _mock) = mock_server().await;
    let door = spawn_door().await;
    let runner = Arc::new(
        TurnRunner::open(&root, SESSION_ID, door)
            .await
            .expect("open"),
    );

    let worker = {
        let runner = Arc::clone(&runner);
        let req = request("slow", addr, UserEntrySource::Terminal);
        tokio::spawn(async move { runner.send(req).await })
    };
    tokio::time::sleep(Duration::from_millis(400)).await;
    runner.stop();
    let turn = worker.await.unwrap().expect("turn settles");

    assert_eq!(turn.outcome, TurnOutcome::Cancelled);
    let record = saved(&root);
    let (user_meta, _) = user_entry(&record);
    assert_eq!(user_meta.turn_outcome, Some(TurnOutcome::Cancelled));
    assert!(user_meta.turn_ended_at.is_some());
    let notice = record
        .entries
        .iter()
        .find_map(|entry| match entry {
            SessionEntry::Notice { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("the cancellation is announced");
    assert_eq!(notice, "Run cancelled.");
    assert!(
        assistant_texts(&record).is_empty(),
        "a cancelled stream writes no assistant entry"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_completed_turn_persists_both_conversation_messages() {
    let root = temp_root("conversation");
    seed_session(&root);
    let (addr, _mock) = mock_server().await;
    let door = spawn_door().await;
    let runner = TurnRunner::open(&root, SESSION_ID, door)
        .await
        .expect("open");

    runner
        .send(request("basic", addr, UserEntrySource::Terminal))
        .await
        .expect("turn");

    let record = saved(&root);
    assert_eq!(record.conversation.len(), 2, "user then assistant");
    assert_eq!(record.conversation[0]["role"], "user");
    assert_eq!(
        record.conversation[0]["content"][0]["type"], "text",
        "ModelMessage content is a parts array"
    );
    assert!(
        record.conversation[0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("hello loop")
    );
    assert_eq!(record.conversation[1]["role"], "assistant");
    assert_eq!(
        record.conversation[1]["content"][0]["text"], "Hello world",
        "the assistant message carries the merged reply"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn system_prompt_reaches_the_provider() {
    let root = temp_root("system");
    seed_session(&root);
    let (addr, mock) = mock_server().await;
    let door = spawn_door().await;
    let runner = TurnRunner::open(&root, SESSION_ID, door)
        .await
        .expect("open");

    let mut req = request("basic", addr, UserEntrySource::Terminal);
    req.system = Some("You are OpenButler, the machine custodian.".to_owned());
    let turn = runner.send(req).await.expect("turn");
    assert_eq!(turn.outcome, TurnOutcome::Completed);

    // End-to-end: contract field → door frame → runner → streamText →
    // the provider request body the mock recorded. The OpenAI-compatible
    // transport folds the SDK's `system` option into the first message.
    let body = mock.bodies.lock().unwrap()[0].clone();
    let parsed: Value = serde_json::from_str(&body).expect("request body is JSON");
    assert_eq!(
        parsed["messages"][0]["role"].as_str(),
        Some("system"),
        "the system travels as the provider's first message: {body}"
    );
    assert_eq!(
        parsed["messages"][0]["content"].as_str(),
        Some("You are OpenButler, the machine custodian."),
        "the assembled system message reaches the provider: {body}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
