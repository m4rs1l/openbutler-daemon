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
use openbutler_daemon::settings::ApprovalMode;
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
        workspace_root: None,
        images: Vec::new(),
        attachments: Vec::new(),
        approval_mode: ApprovalMode::FullAccess,
        command_shell: None,
        approval_timeout_seconds: None,
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

fn finish_chunk(reason: &str) -> String {
    format!(
        "data: {}\n\n",
        json!({
            "id": "c1", "object": "chat.completion.chunk", "created": 1,
            "model": "m", "choices": [{ "index": 0, "delta": {}, "finish_reason": reason }]
        })
    )
}

fn tool_call_chunk(tool: &str, arguments: &str) -> String {
    chunk(json!({
        "tool_calls": [{
            "index": 0,
            "id": "call_batch",
            "function": { "name": tool, "arguments": arguments }
        }]
    }))
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
        // The batch scenarios answer the first request with one tool call
        // and the second with final text: request count decides the step.
        "batch" | "batch_outside" => {
            let requests = mock.bodies.lock().unwrap().len();
            if requests == 1 {
                let arguments = if scenario == "batch_outside" {
                    // Out of scope (the `..` path pattern) but harmless
                    // when an approval lets it run.
                    "{\"command\":\"cd .. && echo approved\"}"
                } else {
                    "{\"command\":\"echo batch-ok\"}"
                };
                sse(&format!(
                    "{}{}data: [DONE]\n\n",
                    tool_call_chunk("execute_command", arguments),
                    finish_chunk("tool_calls"),
                ))
            } else {
                sse(&format!(
                    "{}{}data: [DONE]\n\n",
                    chunk(json!({ "role": "assistant", "content": "batch done" })),
                    finish_chunk("stop"),
                ))
            }
        }
        // The question scenario: the model asks once and reads the
        // answer from the paired tool result on the second request.
        "question" => {
            let requests = mock.bodies.lock().unwrap().len();
            if requests == 1 {
                sse(&format!(
                    "{}{}data: [DONE]\n\n",
                    tool_call_chunk(
                        "question",
                        "{\"question\":\"Ship it today?\",\"options\":[\"yes\",\"no\"]}",
                    ),
                    finish_chunk("tool_calls"),
                ))
            } else {
                sse(&format!(
                    "{}{}data: [DONE]\n\n",
                    chunk(json!({ "role": "assistant", "content": "shipping" })),
                    finish_chunk("stop"),
                ))
            }
        }
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
        TurnRunner::open(&root, SESSION_ID, door, std::sync::Weak::new())
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
    let runner = TurnRunner::open(&root, SESSION_ID, door, std::sync::Weak::new())
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
    let runner = TurnRunner::open(&root, SESSION_ID, door, std::sync::Weak::new())
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
async fn an_unknown_tool_pairs_an_authored_error_and_fails_the_run() {
    let root = temp_root("tool");
    seed_session(&root);
    let (addr, _mock) = mock_server().await;
    let door = spawn_door().await;
    let runner = TurnRunner::open(&root, SESSION_ID, door, std::sync::Weak::new())
        .await
        .expect("open");

    let turn = runner
        .send(request("tool", addr, UserEntrySource::Terminal))
        .await
        .expect("turn");
    // The catalog never sent `get_weather`: the pair lands, the entry
    // settles, the run stops with the authored sentence (documented in
    // docs/tool-pipeline.md — the SDK would have thrown instead).
    assert_eq!(turn.outcome, TurnOutcome::Failed);

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
    assert_eq!(result, "Unknown tool: get_weather.");
    assert_eq!(running, Some(false), "a settled entry is not running");
    assert_eq!(meta.turn_id.as_deref(), Some(turn.turn_id.as_str()));

    let record = saved(&root);
    assert_eq!(
        notice_texts(&record),
        ["Unknown tool: get_weather."],
        "the authored failure is announced"
    );
    // The pair still lands: assistant tool-call + tool error result.
    assert_eq!(record.conversation.len(), 3, "user, assistant, tool");
    assert_eq!(
        record.conversation[2]["content"][0]["output"]["value"]["error"],
        "Unknown tool: get_weather."
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_provider_failure_writes_an_error_notice_and_fails_the_turn() {
    let root = temp_root("fail");
    seed_session(&root);
    let (addr, _mock) = mock_server().await;
    let door = spawn_door().await;
    let runner = TurnRunner::open(&root, SESSION_ID, door, std::sync::Weak::new())
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
        TurnRunner::open(&root, SESSION_ID, door, std::sync::Weak::new())
            .await
            .expect("open"),
    );

    let worker = {
        let runner = Arc::clone(&runner);
        let req = request("slow", addr, UserEntrySource::Terminal);
        tokio::spawn(async move { runner.send(req).await })
    };
    tokio::time::sleep(Duration::from_millis(400)).await;
    runner.stop(false);
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
    let runner = TurnRunner::open(&root, SESSION_ID, door, std::sync::Weak::new())
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
    let runner = TurnRunner::open(&root, SESSION_ID, door, std::sync::Weak::new())
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

#[tokio::test]
async fn workspace_and_channel_labels_reach_the_model_only() {
    let root = temp_root("labels");
    seed_session(&root);
    let (addr, mock) = mock_server().await;
    let door = spawn_door().await;
    let runner = TurnRunner::open(&root, SESSION_ID, door, std::sync::Weak::new())
        .await
        .expect("open");

    let mut req = request("basic", addr, UserEntrySource::Telegram);
    req.workspace_root = Some("work/space".to_owned());
    let turn = runner.send(req).await.expect("turn");
    assert_eq!(turn.outcome, TurnOutcome::Completed);

    let body = mock.bodies.lock().unwrap()[0].clone();
    assert!(
        body.contains("[Host context: active workspace work/space]"),
        "the workspace label travels to the model: {body}"
    );
    assert!(
        body.contains("[Host context: reply through send_message_to_owner"),
        "the Telegram delivery hint travels to the model: {body}"
    );

    // Neither label ever enters the transcript.
    let record = saved(&root);
    for entry in &record.entries {
        if let SessionEntry::User { text, .. } = entry {
            assert!(!text.contains("[Host context: active workspace"));
            assert!(!text.contains("send_message_to_owner"));
        }
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn attachments_reach_the_model_as_references_and_the_entry() {
    let root = temp_root("attach-refs");
    seed_session(&root);
    let (addr, mock) = mock_server().await;
    let door = spawn_door().await;
    let runner = TurnRunner::open(&root, SESSION_ID, door, std::sync::Weak::new())
        .await
        .expect("open");

    let attachment = openbutler_protocol::entry::FileAttachment {
        id: "a".repeat(64),
        path: "/data/sessions/x/attachments/aaa.png".to_owned(),
        name: "shot.png".to_owned(),
        media_type: "image/png".to_owned(),
        size_bytes: 12,
        sha256: "a".repeat(64),
    };
    let mut req = request("basic", addr, UserEntrySource::Web);
    req.workspace_root = Some("work/space".to_owned());
    req.attachments.push(attachment.clone());
    let turn = runner.send(req).await.expect("turn");
    assert_eq!(turn.outcome, TurnOutcome::Completed);

    let body = mock.bodies.lock().unwrap()[0].clone();
    let parsed: Value = serde_json::from_str(&body).expect("request body is JSON");
    assert_eq!(
        parsed["messages"][0]["content"][1]["text"].as_str(),
        Some("[image#1=\"/data/sessions/x/attachments/aaa.png\"]"),
        "the reference travels to the model as its own text part: {body}"
    );

    let record = saved(&root);
    let user = record
        .entries
        .iter()
        .find_map(|entry| match entry {
            SessionEntry::User { .. } => Some(entry.clone()),
            _ => None,
        })
        .expect("a user entry exists");
    let SessionEntry::User {
        workspace_root,
        image_count,
        attachments,
        ..
    } = user
    else {
        unreachable!()
    };
    assert_eq!(workspace_root.as_deref(), Some("work/space"));
    assert_eq!(image_count, Some(1), "one image attachment is counted");
    assert_eq!(attachments, Some(vec![attachment]), "stored on the entry");

    let _ = std::fs::remove_dir_all(&root);
}

// --- c2: the batch loop ------------------------------------------------------

/// Polls until the running turn publishes a pending approval (the queue
/// behind `session.approve`), then returns its id.
async fn await_approval(runner: &Arc<TurnRunner>) -> String {
    for _ in 0..400 {
        if let Some((approval_id, _gate_reason)) = runner.pending_approval() {
            return approval_id;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("a pending approval appears");
}

/// Polls until a question is waiting, then answers it (the queue behind
/// `session.answer`).
async fn answer_question(runner: &Arc<TurnRunner>, answer: &str) {
    for _ in 0..400 {
        if let Some(question_id) = runner.pending_question_id() {
            assert!(
                runner.answer_question(&question_id, Some(answer.to_owned())),
                "the id guard accepts the live question id"
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("a pending question appears");
}

fn tool_entries(record: &openbutler_daemon::state::SessionRecord) -> Vec<SessionEntry> {
    record
        .entries
        .iter()
        .filter(|entry| matches!(entry, SessionEntry::Tool { .. }))
        .cloned()
        .collect()
}

fn notice_texts(record: &openbutler_daemon::state::SessionRecord) -> Vec<String> {
    record
        .entries
        .iter()
        .filter_map(|entry| match entry {
            SessionEntry::Notice { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn ledger_states(root: &Path) -> Vec<String> {
    let connection = rusqlite::Connection::open(root.join("state.db")).unwrap();
    let mut statement = connection
        .prepare(
            "SELECT state FROM openbutler_action_transitions \
             WHERE request_id = ?1 ORDER BY id",
        )
        .unwrap();
    statement
        .query_map(["call_batch"], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

#[tokio::test]
async fn the_batch_loop_executes_the_tool_and_pairs_both_messages() {
    let root = temp_root("batch");
    seed_session(&root);
    let (addr, mock) = mock_server().await;
    let door = spawn_door().await;
    let runner = TurnRunner::open(&root, SESSION_ID, door, std::sync::Weak::new())
        .await
        .expect("open");

    let mut req = request("batch", addr, UserEntrySource::Terminal);
    req.workspace_root = Some(root.display().to_string());
    let turn = runner.send(req).await.expect("turn");
    assert_eq!(
        turn.outcome,
        TurnOutcome::Completed,
        "notices: {:?}",
        notice_texts(&saved(&root))
    );

    // The tool ran (echo), so a second model call carries both messages.
    let bodies = mock.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 2, "one tool step needs a second request");
    assert!(bodies[1].contains("\"tool_calls\""), "assistant tool part");
    assert!(bodies[1].contains("execute_command"));
    assert!(
        bodies[1].contains("call_batch"),
        "ids pair across both roles"
    );
    assert!(
        bodies[1].contains("batch-ok"),
        "the tool's stdout reaches the model as the result"
    );

    // The transcript carries the settled tool entry, not the settle text.
    let record = saved(&root);
    let tools = tool_entries(&record);
    assert_eq!(tools.len(), 1, "one tool entry");
    let SessionEntry::Tool {
        tool,
        result,
        running,
        duration_ms,
        ..
    } = &tools[0]
    else {
        unreachable!()
    };
    assert_eq!(tool.as_deref(), Some("execute_command"));
    assert!(result.contains("batch-ok"), "result text: {result}");
    assert!(!result.contains("not executed"), "the settle text is gone");
    assert_eq!(*running, Some(false), "the entry settles as not running");
    assert!(duration_ms.is_some(), "the execution duration is stamped");

    // The persisted conversation pairs assistant/tool per the SDK shapes.
    assert_eq!(record.conversation.len(), 4, "user, assistant, tool, final");
    assert_eq!(record.conversation[1]["role"], "assistant");
    let call_part = &record.conversation[1]["content"][0];
    assert_eq!(call_part["type"], "tool-call");
    assert_eq!(call_part["toolName"], "execute_command");
    assert_eq!(call_part["toolCallId"], "call_batch");
    assert_eq!(call_part["input"]["command"], "echo batch-ok");
    assert_eq!(record.conversation[2]["role"], "tool");
    let output = &record.conversation[2]["content"][0]["output"];
    assert_eq!(output["type"], "json");
    assert!(
        output["value"]["stdout"]
            .as_str()
            .is_some_and(|stdout| stdout.contains("batch-ok")),
        "the echo output reaches the model: {output}"
    );
    assert_eq!(record.conversation[3]["role"], "assistant");
    assert_eq!(record.conversation[3]["content"][0]["text"], "batch done");

    // The ledger wrote the real sequence for the real run (request id =
    // the model's tool call id, the source's `requestId` analogue).
    assert_eq!(
        ledger_states(&root),
        ["proposed", "approved", "started", "completed"]
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_declined_tool_stops_after_pairing_the_denial_result() {
    let root = temp_root("batch-decline");
    seed_session(&root);
    let (addr, mock) = mock_server().await;
    let door = spawn_door().await;
    let runner = TurnRunner::open(&root, SESSION_ID, door, std::sync::Weak::new())
        .await
        .expect("open");

    // Workspace mode + a command that reaches outside the workspace: the
    // gate queues a manual approval, and the client declines it.
    let mut req = request("batch_outside", addr, UserEntrySource::Terminal);
    req.workspace_root = Some(root.display().to_string());
    req.approval_mode = ApprovalMode::Workspace;
    let runner = Arc::new(runner);
    let worker = {
        let runner = Arc::clone(&runner);
        tokio::spawn(async move { runner.send(req).await })
    };
    let approval_id = await_approval(&runner).await;
    assert!(
        runner.resolve_approval(&approval_id, false),
        "the id guard accepts the live approval id"
    );
    let turn = worker.await.unwrap().expect("turn");

    assert_eq!(turn.outcome, TurnOutcome::Completed);
    assert_eq!(
        mock.bodies.lock().unwrap().len(),
        1,
        "a stopped run sends no second request"
    );

    let record = saved(&root);
    let notices = notice_texts(&record);
    assert_eq!(
        notices,
        ["The tool call was denied; this run has stopped."],
        "the source's stop notice (L2505)"
    );

    // The denial still pairs: model and transcript both see the result.
    assert_eq!(record.conversation.len(), 3, "user, assistant, tool");
    assert_eq!(record.conversation[2]["role"], "tool");
    let value = &record.conversation[2]["content"][0]["output"]["value"];
    assert_eq!(value["status"], "denied");
    assert_eq!(
        value["error"],
        "The approval was declined. The tool call was not run."
    );
    assert_eq!(ledger_states(&root), ["proposed", "denied"]);

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn an_approved_queue_entry_runs_the_tool_and_continues_the_batch() {
    let root = temp_root("batch-approve");
    seed_session(&root);
    let (addr, mock) = mock_server().await;
    let door = spawn_door().await;
    let runner = Arc::new(
        TurnRunner::open(&root, SESSION_ID, door, std::sync::Weak::new())
            .await
            .expect("open"),
    );

    let mut req = request("batch_outside", addr, UserEntrySource::Terminal);
    req.workspace_root = Some(root.display().to_string());
    req.approval_mode = ApprovalMode::Workspace;
    let worker = {
        let runner = Arc::clone(&runner);
        tokio::spawn(async move { runner.send(req).await })
    };
    let approval_id = await_approval(&runner).await;
    assert!(runner.resolve_approval(&approval_id, true));
    let turn = worker.await.unwrap().expect("turn");

    // The approval lets the out-of-scope command run; the run continues.
    assert_eq!(turn.outcome, TurnOutcome::Completed);
    assert_eq!(
        mock.bodies.lock().unwrap().len(),
        2,
        "the second request follows the executed batch"
    );
    assert!(
        notice_texts(&saved(&root)).is_empty(),
        "an approval announces nothing"
    );
    assert_eq!(
        ledger_states(&root),
        ["proposed", "approved", "started", "completed"]
    );
    let record = saved(&root);
    assert_eq!(record.conversation.len(), 4, "user, assistant, tool, final");
    assert!(
        record.conversation[2]["content"][0]["output"]["value"]["stdout"]
            .as_str()
            .is_some_and(|stdout| stdout.contains("approved"))
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn an_untouched_queue_times_out_with_both_source_sentences() {
    let root = temp_root("batch-timeout");
    seed_session(&root);
    let (addr, mock) = mock_server().await;
    let door = spawn_door().await;
    let runner = TurnRunner::open(&root, SESSION_ID, door, std::sync::Weak::new())
        .await
        .expect("open");

    let mut req = request("batch_outside", addr, UserEntrySource::Terminal);
    req.workspace_root = Some(root.display().to_string());
    req.approval_mode = ApprovalMode::Workspace;
    // One second instead of the source's 120-second default.
    req.approval_timeout_seconds = Some(1);
    let turn = runner.send(req).await.expect("turn");

    // A timeout never stops the run (registry L365): the denial pairs,
    // the loop continues, the second request settles the turn.
    assert_eq!(turn.outcome, TurnOutcome::Completed);
    assert_eq!(mock.bodies.lock().unwrap().len(), 2);
    let record = saved(&root);
    assert_eq!(
        notice_texts(&record),
        ["No approval decision within 1 seconds; the action was not run."],
        "requestManualApproval's transcript notice (L1819)"
    );
    let value = &record.conversation[2]["content"][0]["output"]["value"];
    assert!(
        value["error"]
            .as_str()
            .is_some_and(|error| error.starts_with("No approval decision within 1 seconds:")),
        "the registry's denial sentence"
    );
    assert_eq!(ledger_states(&root), ["proposed", "denied"]);

    let _ = std::fs::remove_dir_all(&root);
}

// --- live publish (c3c) -------------------------------------------------------

/// A sink that records every published frame.
#[derive(Default)]
struct RecordingSink {
    frames: std::sync::Mutex<Vec<Vec<u8>>>,
}

impl openbutler_daemon::dispatch::fanout::EventSink for RecordingSink {
    fn try_send(&self, frame: Vec<u8>) -> bool {
        self.frames.lock().unwrap().push(frame);
        true
    }
}

/// The bootstrap runtime view a prepared turn publishes with (the
/// conformance fixture's shape — see `runtime-view.json`).
fn runtime_view() -> openbutler_protocol::runtime::RuntimeViewData {
    serde_json::from_value(serde_json::json!({
        "supportsSessionAttachments": true,
        "supportsProjects": false,
        "workspaceRoot": "C:\\work\\demo",
        "modelName": "test-model",
        "providerId": "mock",
        "modelId": "test-model",
        "contextWindowTokens": null,
        "supportsVision": false,
        "reasoningVariants": [],
        "reasoningMetadataSource": null,
        "reasoningSupport": "model-unsupported",
        "reasoningDefault": "",
        "reasoningPreference": "",
        "showReasoningDefault": false,
    }))
    .expect("runtime fixture")
}

#[tokio::test]
async fn a_running_turn_publishes_live_state_to_the_fanout() {
    let root = temp_root("publish");
    seed_session(&root);
    // The `think` scenario answers in two chunks: at least two deltas
    // cross the wire while the turn runs.
    let (addr, _mock) = mock_server().await;
    let door = spawn_door().await;

    let host = Arc::new(openbutler_daemon::dispatch::Host::new());
    host.set_runtime_view(runtime_view());
    let sink = Arc::new(RecordingSink::default());
    host.fanout().subscribe(SESSION_ID, sink.clone());

    let runner = Arc::new(
        TurnRunner::open(&root, SESSION_ID, door, Arc::downgrade(&host))
            .await
            .expect("open"),
    );
    let turn = runner
        .send(request("think", addr, UserEntrySource::Terminal))
        .await
        .expect("turn");
    assert_eq!(turn.outcome, TurnOutcome::Completed);

    let frames = sink.frames.lock().unwrap().clone();
    assert!(
        frames.len() >= 3,
        "admission seeds, deltas publish, and the turn end publishes (got {})",
        frames.len()
    );
    let events: Vec<openbutler_protocol::DaemonEvent> = frames
        .iter()
        .map(|frame| {
            serde_json::from_slice(frame.strip_suffix(b"\n").unwrap_or(frame))
                .expect("frame decodes as an event")
        })
        .collect();

    // Mid-stream frames are patches with a live streaming index; the
    // last frame closes the turn with busy: false.
    let mid = &events[1];
    match mid {
        openbutler_protocol::DaemonEvent::Patch { patch, .. } => {
            assert!(patch.run.busy, "the stream publishes while busy");
            assert!(
                patch.run.streaming_assistant_index.is_some(),
                "the streaming entry's index rides the frame"
            );
        }
        other => panic!("expected a patch mid-stream, got {other:?}"),
    }
    let last = events.last().expect("at least one frame");
    match last {
        openbutler_protocol::DaemonEvent::Patch { patch, .. } => {
            assert!(!patch.run.busy, "the turn end publishes busy: false");
        }
        other => panic!("expected a closing patch, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_question_queues_and_resolves_through_the_answer_seam() {
    let root = temp_root("question-queue");
    seed_session(&root);
    let door = spawn_door().await;

    let host = Arc::new(openbutler_daemon::dispatch::Host::new());
    host.set_runtime_view(runtime_view());
    let sink = Arc::new(RecordingSink::default());
    host.fanout().subscribe(SESSION_ID, sink.clone());

    let runner = Arc::new(
        TurnRunner::open(&root, SESSION_ID, door, Arc::downgrade(&host))
            .await
            .expect("open"),
    );

    // A question queued for the owner (what the `question` tool calls).
    let asker = Arc::clone(&runner);
    let worker = tokio::spawn(async move {
        asker
            .ask(
                "Ship it today?".to_owned(),
                Some(vec!["yes".to_owned(), "no".to_owned()]),
            )
            .await
    });
    answer_question(&runner, "yes").await;

    // The answer rides back to the waiting ask.
    assert_eq!(worker.await.unwrap(), Some("yes".to_owned()));

    // A stale id no-ops (the source's questionId guard).
    assert!(!runner.answer_question("question-stale", Some("no".to_owned())));

    let _ = std::fs::remove_dir_all(&root);
}

/// The `question` tool end to end: the model's call queues, the roster
/// shows the pending question while the turn waits, the owner answers
/// through the seam, and the answer lands in the conversation pair.
/// (A direct `ask` has no turn owner to publish from — the roster
/// claim belongs to the send-driven path, so it lives here.)
#[tokio::test]
async fn the_question_tool_queues_publishes_and_resolves_during_a_turn() {
    let root = temp_root("question-e2e");
    seed_session(&root);
    let (addr, mock) = mock_server().await;
    let door = spawn_door().await;

    let host = Arc::new(openbutler_daemon::dispatch::Host::new());
    host.set_runtime_view(runtime_view());
    let sink = Arc::new(RecordingSink::default());
    host.fanout().subscribe(SESSION_ID, sink.clone());

    let runner = Arc::new(
        TurnRunner::open(&root, SESSION_ID, door, Arc::downgrade(&host))
            .await
            .expect("open"),
    );

    let asker = Arc::clone(&runner);
    let turn_root = root.clone();
    let worker = tokio::spawn(async move {
        let mut req = request("question", addr, UserEntrySource::Terminal);
        req.workspace_root = Some(turn_root.display().to_string());
        asker.send(req).await
    });

    // The roster must show the queued question while the turn waits —
    // published by `send`'s tool select the moment the tool queues it
    // (the tool itself runs under the held state lock).
    let question_id = {
        let mut seen = None;
        for _ in 0..500 {
            let frames = sink.frames.lock().unwrap().clone();
            for frame in &frames {
                let event = serde_json::from_slice::<openbutler_protocol::DaemonEvent>(
                    frame.strip_suffix(b"\n").unwrap_or(frame),
                )
                .ok();
                if let Some(openbutler_protocol::DaemonEvent::Patch { patch, .. }) = event
                    && let Some(pending) = &patch.run.pending_question
                {
                    seen = Some(pending.id.clone());
                    break;
                }
            }
            if seen.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if let Some(id) = seen {
            id
        } else {
            let frames_count = sink.frames.lock().unwrap().len();
            let bodies_count = mock.bodies.lock().unwrap().len();
            panic!(
                "the roster never shows the queued question (frames={frames_count}, \
                 bodies={bodies_count}, pending={:?})",
                runner.pending_question_id()
            )
        }
    };

    // The frame's id answers through the same seam the arm uses.
    assert!(
        runner.answer_question(&question_id, Some("yes".to_owned())),
        "the roster id answers through the seam"
    );

    let turn = worker.await.unwrap().expect("turn");
    assert_eq!(turn.outcome, TurnOutcome::Completed);

    // The answer reached the model as the tool result on the second
    // request, and the pair persisted: user, assistant, tool, final.
    let bodies = mock.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 2, "one question step needs a second request");
    assert!(
        bodies[1].contains("Ship it today?"),
        "the tool result carries the question"
    );
    // The result rides embedded in the tool message's JSON string, so
    // the quotes arrive backslash-escaped in the raw body.
    assert!(
        bodies[1].contains("\\\"answer\\\":\\\"yes\\\""),
        "and the answer"
    );

    let record = saved(&root);
    assert_eq!(record.conversation.len(), 4, "user, assistant, tool, final");
    let value = &record.conversation[2]["content"][0]["output"]["value"];
    assert_eq!(value["status"], "answered");
    assert_eq!(value["question"], "Ship it today?");
    assert_eq!(value["answer"], "yes");
    assert_eq!(record.conversation[3]["content"][0]["text"], "shipping");

    // The closing frame carries no pending question any more.
    let frames = sink.frames.lock().unwrap().clone();
    let last_frame = frames.last().expect("at least one frame");
    let last = serde_json::from_slice::<openbutler_protocol::DaemonEvent>(
        last_frame.strip_suffix(b"\n").unwrap_or(last_frame),
    )
    .expect("frame decodes as an event");
    match last {
        openbutler_protocol::DaemonEvent::Patch { patch, .. } => {
            assert!(!patch.run.busy, "the turn end publishes busy: false");
            assert!(
                patch.run.pending_question.is_none(),
                "the queue is empty after the answer"
            );
        }
        other => panic!("expected a closing patch, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&root);
}

/// The queued pick lands on the record at the turn boundary: the runner
/// takes it before the admission save, so the chat's saved model is the
/// picked one after one turn (legacy `selectModel` → session model).
#[tokio::test]
async fn a_queued_pick_lands_on_the_record_at_the_turn_boundary() {
    let root = temp_root("pick-boundary");
    seed_session(&root);
    let (addr, _mock) = mock_server().await;
    let door = spawn_door().await;

    let host = Arc::new(openbutler_daemon::dispatch::Host::new());
    host.set_runtime_view(runtime_view());
    host.queue_model_pick(SESSION_ID, "mock".to_owned(), "picked-model".to_owned());

    let runner = TurnRunner::open(&root, SESSION_ID, door, Arc::downgrade(&host))
        .await
        .expect("open");
    let turn = runner
        .send(request("basic", addr, UserEntrySource::Terminal))
        .await
        .expect("turn");
    assert_eq!(turn.outcome, TurnOutcome::Completed);

    let record = saved(&root);
    assert_eq!(
        record.meta.model_id, "picked-model",
        "the take writes the pick into the record"
    );
    assert_eq!(record.meta.provider_id, "mock");
    assert!(
        host.peek_model_pick(SESSION_ID).is_none(),
        "the pick is consumed, not reused"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Polls the roster until `queued_message_count` reaches the target.
async fn await_queue(sink: &Arc<RecordingSink>, count: u32) {
    for _ in 0..500 {
        let frames = sink.frames.lock().unwrap().clone();
        for frame in &frames {
            let event = serde_json::from_slice::<openbutler_protocol::DaemonEvent>(
                frame.strip_suffix(b"\n").unwrap_or(frame),
            )
            .ok();
            if let Some(openbutler_protocol::DaemonEvent::Patch { patch, .. }) = event
                && patch.run.queued_message_count >= count
            {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the roster never shows {count} queued message(s)");
}

/// A message sent while a run goes queues up (visible on the roster)
/// and drains as its own turn after the run ends (legacy
/// `enqueueMessage` → `runQueuedMessages`).
#[tokio::test]
async fn messages_queue_during_a_run_and_drain_after() {
    let root = temp_root("queue-drain");
    seed_session(&root);
    let (addr, mock) = mock_server().await;
    let door = spawn_door().await;

    let host = Arc::new(openbutler_daemon::dispatch::Host::new());
    host.set_runtime_view(runtime_view());
    let sink = Arc::new(RecordingSink::default());
    host.fanout().subscribe(SESSION_ID, sink.clone());

    let runner = Arc::new(
        TurnRunner::open(&root, SESSION_ID, door, Arc::downgrade(&host))
            .await
            .expect("open"),
    );

    // The slow first turn (1.5 s): wait until it owns the session
    // (the admission), then a message that arrives mid-run.
    let asker = Arc::clone(&runner);
    let worker = tokio::spawn(async move {
        asker
            .send(request("slow", addr, UserEntrySource::Terminal))
            .await
    });
    for _ in 0..500 {
        if runner.is_turn_active() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(runner.is_turn_active(), "the turn owns the session");
    runner.enqueue(request("basic", addr, UserEntrySource::Web));
    await_queue(&sink, 1).await;

    // The queued message runs on its own when the first turn settles.
    let first = worker.await.unwrap().expect("first turn");
    assert_eq!(first.outcome, TurnOutcome::Completed);
    for _ in 0..500 {
        if mock.bodies.lock().unwrap().len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        mock.bodies.lock().unwrap().len(),
        2,
        "slow(1 request) then the drained basic turn"
    );
    // The drain settles without an external trigger and the transcript
    // carries both user entries.
    for _ in 0..500 {
        if saved(&root).entries.len() >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let record = saved(&root);
    assert!(
        record.entries.len() >= 3,
        "user (slow), assistant, user (drained): {}",
        record.entries.len()
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// An explicit steer admits the queued message into the running task at
/// the next model step: the conversation carries it before the next
/// request is built (legacy `takeQueuedMessages` at prepareStep).
#[tokio::test]
async fn a_steered_message_admits_into_the_running_turn() {
    let root = temp_root("queue-steer");
    seed_session(&root);
    let (addr, mock) = mock_server().await;
    let door = spawn_door().await;

    let host = Arc::new(openbutler_daemon::dispatch::Host::new());
    host.set_runtime_view(runtime_view());
    let sink = Arc::new(RecordingSink::default());
    host.fanout().subscribe(SESSION_ID, sink.clone());

    let runner = Arc::new(
        TurnRunner::open(&root, SESSION_ID, door, Arc::downgrade(&host))
            .await
            .expect("open"),
    );

    let asker = Arc::clone(&runner);
    let worker = tokio::spawn(async move {
        asker
            .send(request("slow", addr, UserEntrySource::Terminal))
            .await
    });
    for _ in 0..500 {
        if runner.is_turn_active() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(runner.is_turn_active(), "the turn owns the session");

    // A web message never steers by default (owner decision 2026-10-31);
    // the explicit promote admits it into the live run.
    runner.enqueue({
        let mut queued = request("slow", addr, UserEntrySource::Web);
        queued.text = "steer me".to_owned();
        queued
    });
    await_queue(&sink, 1).await;
    let queued_id = {
        // The roster carries the id the manage action addresses.
        let frames = sink.frames.lock().unwrap().clone();
        let mut found = None;
        for frame in &frames {
            let event = serde_json::from_slice::<openbutler_protocol::DaemonEvent>(
                frame.strip_suffix(b"\n").unwrap_or(frame),
            )
            .ok();
            if let Some(openbutler_protocol::DaemonEvent::Patch { patch, .. }) = event
                && let Some(item) = patch
                    .run
                    .queued_messages
                    .first()
                    .and_then(|message| message.id.clone())
            {
                found = Some(item);
                break;
            }
        }
        found.expect("the queued message carries its id")
    };

    if let Err(error) =
        runner.manage_queue(&queued_id, openbutler_daemon::agent::QueueAction::Steer)
    {
        panic!("the steer is refused: {error} (queued_id={queued_id})");
    }

    let turn = worker.await.unwrap().expect("turn");
    assert_eq!(turn.outcome, TurnOutcome::Completed);

    // The steered message entered before the closing request: the
    // last body carries it as a user message.
    let bodies = mock.bodies.lock().unwrap().clone();
    assert!(bodies.len() >= 2, "one request, one steered step");
    assert!(
        bodies.last().unwrap().contains("steer me"),
        "the admission reaches the model"
    );
    let record = saved(&root);
    assert!(
        record
            .entries
            .iter()
            .any(|entry| matches!(entry, openbutler_protocol::entry::SessionEntry::User { text, .. } if text == "steer me")),
        "the admission lands in the transcript"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_queued_approval_reaches_the_roster_before_the_decision() {
    let root = temp_root("approval-publish");
    seed_session(&root);
    let (addr, mock) = mock_server().await;
    let door = spawn_door().await;

    let host = Arc::new(openbutler_daemon::dispatch::Host::new());
    host.set_runtime_view(runtime_view());
    let sink = Arc::new(RecordingSink::default());
    host.fanout().subscribe(SESSION_ID, sink.clone());

    let runner = Arc::new(
        TurnRunner::open(&root, SESSION_ID, door, Arc::downgrade(&host))
            .await
            .expect("open"),
    );
    let worker = {
        let runner = Arc::clone(&runner);
        let mut req = request("batch_outside", addr, UserEntrySource::Terminal);
        req.workspace_root = Some(root.display().to_string());
        req.approval_mode = ApprovalMode::Workspace;
        tokio::spawn(async move { runner.send(req).await })
    };
    // The outside-workspace call queues in workspace mode; while it waits
    // the roster must already show the pending approval (legacy
    // `requestManualApproval` publishes before it resolves).
    let approval_id = await_approval(&runner).await;
    let frames = sink.frames.lock().unwrap().clone();
    let visible = frames.iter().any(|frame| {
        serde_json::from_slice::<openbutler_protocol::DaemonEvent>(
            frame.strip_suffix(b"\n").unwrap_or(frame),
        )
        .ok()
        .is_some_and(|event| match event {
            openbutler_protocol::DaemonEvent::Patch { patch, .. } => {
                patch.run.pending_approval_id.as_deref() == Some(approval_id.as_str())
                    && patch.run.pending_approval.is_some()
            }
            _ => false,
        })
    });
    assert!(
        visible,
        "a frame carries pendingApproval with its id while the queue waits"
    );

    // Decline closes it; the run stops after the pair (no second call).
    assert!(runner.resolve_approval(&approval_id, false));
    let turn = worker.await.unwrap().expect("turn");
    assert_eq!(turn.outcome, TurnOutcome::Completed);
    assert_eq!(
        mock.bodies.lock().unwrap().len(),
        1,
        "the stop ends the batch"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A compaction requested mid-run queues with the source's notice and
/// runs when the turn finishes — the run-end fallback for the step
/// boundary (legacy `compact()` L2741-2748; the boundary itself
/// arrives with the auto-trigger slice). The notice write waits for
/// the run's state lock (the source holds it in memory instead), so
/// the text lands at the turn end rather than mid-stream.
#[tokio::test]
async fn a_compaction_requested_while_busy_queues_and_runs_at_the_turn_end() {
    let root = temp_root("compact-busy");
    seed_compactable(&root);
    let original = saved(&root).conversation.clone();
    let (addr, mock) = mock_server().await;
    // Settings feed only the compaction (the turn carries its own
    // URL): the fast scenario answers the summarisation.
    std::fs::write(
        root.join("settings.yaml"),
        format!(
            "providers:\n  openai-compatible:\n    enabled: true\n    url: http://{addr}/basic\n"
        ),
    )
    .expect("settings");
    std::fs::write(
        root.join("config.yaml"),
        "providers:\n  openai-compatible:\n    keys:\n      - sk-test-not-a-real-key\n",
    )
    .expect("config");
    let door = spawn_door().await;
    let host = Arc::new(openbutler_daemon::dispatch::Host::new());
    host.set_runtime_view(runtime_view());
    host.reload_settings(&root).expect("settings load");
    let runner = Arc::new(
        TurnRunner::open(&root, SESSION_ID, door, Arc::downgrade(&host))
            .await
            .expect("open"),
    );

    // Production shape: the drain loop drives the turn, so the run's
    // end is where the queued compaction gets served.
    runner.enqueue(request("slow", addr, UserEntrySource::Terminal));
    for _ in 0..500 {
        if runner.is_turn_active() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(runner.is_turn_active(), "the turn owns the session");

    // Mid-run: the request defers with the source's notice.
    runner.compact(false).await.expect("deferred");
    let mid = saved(&root);
    assert!(
        mid.entries.iter().any(|entry| matches!(
            entry,
            SessionEntry::Notice { text, .. }
                if text == "Compaction is queued; it runs at the next step boundary or when the run finishes."
        )),
        "the queued notice lands: {:?}",
        mid.entries
    );

    // The run finishes; the drain serves the queued compaction.
    let mut landed = false;
    for _ in 0..400 {
        let record = saved(&root);
        if record.entries.iter().any(|entry| {
            matches!(
                entry,
                SessionEntry::Compaction {
                    status: openbutler_protocol::entry::CompactionStatus::Done,
                    ..
                }
            )
        }) {
            landed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(landed, "the queued compaction runs at the turn end");

    let record = saved(&root);
    assert!(
        record.conversation.first().is_some_and(|message| {
            message["content"].as_str().is_some_and(|text| {
                text.starts_with("OpenButler conversation summary (context only):")
            })
        }),
        "checkpoint first: {:?}",
        record.conversation.first()
    );
    // The archive holds the conversation as it stood when the
    // rewrite applied: the seed transcript plus the run's own pair
    // (source `applyCompaction` copies at swap time, not at seed).
    let archive = record
        .conversation_archive
        .as_ref()
        .expect("the archive is written on the first rewrite");
    assert_eq!(
        &archive[..original.len()],
        original.as_slice(),
        "the seed transcript survives intact"
    );
    assert_eq!(
        archive.len(),
        original.len() + 2,
        "the run's own messages were part of what got compacted"
    );
    assert_eq!(
        mock.bodies.lock().unwrap().len(),
        2,
        "the turn plus the served compaction"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A chat with a user turn, a conversation to shrink, and the
/// provider key the sidecar allowlists.
fn seed_compactable(root: &Path) {
    let json = format!(
        r#"{{
  "version": 1,
  "id": "{SESSION_ID}",
  "title": "Compact busy",
  "createdAt": "2026-01-01T00:00:00.000Z",
  "updatedAt": "2026-01-01T00:00:00.000Z",
  "providerId": "openai-compatible",
  "modelId": "test-model",
  "contextTokens": 12345,
  "entries": [
    {{
      "kind": "user",
      "text": "start the work",
      "source": "terminal",
      "turnId": "turn-1"
    }},
    {{ "kind": "assistant", "text": "working on it" }}
  ],
  "conversation": [
    {{ "role": "user", "content": "start the work" }},
    {{ "role": "assistant", "content": "working on it" }}
  ]
}}"#
    );
    let record = parse_record(&json).expect("seed record parses");
    match save_session(root, &record, true).expect("seed save") {
        SaveOutcome::Written(_) => {}
        SaveOutcome::Skipped => panic!("allowEmpty writes the seed"),
    }
}
