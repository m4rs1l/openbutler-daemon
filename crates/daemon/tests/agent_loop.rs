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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
            "{}{}data: {{\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}}}\n\ndata: [DONE]\n\n",
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
        // The stop scenario: one command long enough that a stop lands
        // while it is still running. Nothing but the host's own stop may
        // end it inside the assertion window.
        "long_command" => {
            let requests = mock.bodies.lock().unwrap().len();
            if requests == 1 {
                let command = if cfg!(windows) {
                    "powershell -NoProfile -Command Start-Sleep 6"
                } else {
                    "sleep 6"
                };
                sse(&format!(
                    "{}{}data: [DONE]\n\n",
                    tool_call_chunk(
                        "execute_command",
                        &json!({ "command": command }).to_string()
                    ),
                    finish_chunk("tool_calls"),
                ))
            } else {
                sse(&format!(
                    "{}{}data: [DONE]\n\n",
                    chunk(json!({ "role": "assistant", "content": "the command was interrupted" })),
                    finish_chunk("stop"),
                ))
            }
        }
        // Four calls in one step: three finish, the fourth blocks until the
        // owner stops it. The body of the answer decides which call ran.
        "batch_four" => {
            let requests = mock.bodies.lock().unwrap().len();
            if requests == 1 {
                let mut body = String::new();
                let quick = [
                    ("call_b1", "echo one"),
                    ("call_b2", "echo two"),
                    ("call_b3", "echo three"),
                ];
                for (index, (id, command)) in quick.iter().enumerate() {
                    body.push_str(&chunk(json!({
                        "tool_calls": [{
                            "index": index,
                            "id": id,
                            "function": {
                                "name": "execute_command",
                                "arguments": json!({ "command": command }).to_string()
                            }
                        }]
                    })));
                }
                let slow = if cfg!(windows) {
                    "powershell -NoProfile -Command Start-Sleep 6"
                } else {
                    "sleep 6"
                };
                body.push_str(&chunk(json!({
                    "tool_calls": [{
                        "index": 3,
                        "id": "call_b4",
                        "function": {
                            "name": "execute_command",
                            "arguments": json!({ "command": slow }).to_string()
                        }
                    }]
                })));
                body.push_str(&chunk(json!({
                    "tool_calls": [{
                        "index": 4,
                        "id": "call_b5",
                        "function": {
                            "name": "execute_command",
                            "arguments": json!({ "command": "echo five" }).to_string()
                        }
                    }]
                })));
                body.push_str(&finish_chunk("tool_calls"));
                body.push_str("data: [DONE]\n\n");
                sse(&body)
            } else {
                sse(&format!(
                    "{}{}data: [DONE]\n\n",
                    chunk(json!({ "role": "assistant", "content": "batch complete" })),
                    finish_chunk("stop"),
                ))
            }
        }
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

/// Whether the host has recorded the start of one call. The table appears
/// only once the ledger writer opens, and these polls may run first, so a
/// missing table is "not yet", not a failure.
fn command_started_for(root: &Path, request_id: &str) -> bool {
    let Ok(connection) = rusqlite::Connection::open(root.join("state.db")) else {
        return false;
    };
    let Ok(mut statement) = connection.prepare(
        "SELECT state FROM openbutler_action_transitions WHERE request_id = ?1 ORDER BY id",
    ) else {
        return false;
    };
    let Ok(rows) = statement.query_map([request_id], |row| row.get::<_, String>(0)) else {
        return false;
    };
    rows.filter_map(Result::ok).any(|state| state == "started")
}

fn command_started(root: &Path) -> bool {
    command_started_for(root, "call_batch")
}

/// The ledger's recorded states for one call, oldest first.
fn ledger_states_for(root: &Path, request_id: &str) -> Vec<String> {
    let connection = rusqlite::Connection::open(root.join("state.db")).unwrap();
    let mut statement = connection
        .prepare(
            "SELECT state FROM openbutler_action_transitions \
             WHERE request_id = ?1 ORDER BY id",
        )
        .unwrap();
    statement
        .query_map([request_id], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

/// Four calls in one step, the owner stops during the fourth: the three that
/// finished keep their own results and the interrupted one names the stop.
/// Every call keeps exactly one result — a batch must never leave a tool call
/// unanswered, or the next request to the provider is invalid.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_mid_batch_keeps_the_finished_calls_and_pairs_the_interrupted_one() {
    let root = temp_root("stop-batch");
    seed_session(&root);
    let (addr, mock) = mock_server().await;
    let door = spawn_door().await;
    let runner = Arc::new(
        TurnRunner::open(&root, SESSION_ID, door, std::sync::Weak::new())
            .await
            .expect("open"),
    );

    let mut req = request("batch_four", addr, UserEntrySource::Terminal);
    req.workspace_root = Some(root.display().to_string());
    let worker = {
        let runner = Arc::clone(&runner);
        tokio::spawn(async move { runner.send(req).await })
    };

    let deadline = Instant::now() + Duration::from_secs(40);
    while !command_started_for(&root, "call_b4") {
        assert!(
            Instant::now() < deadline,
            "the fourth command never started"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    runner.stop(false);

    let turn = worker.await.expect("join").expect("turn settles");
    assert_eq!(turn.outcome, TurnOutcome::Cancelled);

    // Every finished call keeps its own result; the interrupted one says so.
    let record = saved(&root);
    let tools = tool_entries(&record);
    assert_eq!(tools.len(), 5, "one entry per call");
    let results: Vec<String> = tools
        .iter()
        .map(|entry| match entry {
            SessionEntry::Tool { result, .. } => result.clone(),
            _ => unreachable!(),
        })
        .collect();
    assert!(results[0].contains("one"), "first: {}", results[0]);
    assert!(results[1].contains("two"), "second: {}", results[1]);
    assert!(results[2].contains("three"), "third: {}", results[2]);
    assert!(
        results[3].contains("stopped by the user"),
        "fourth: {}",
        results[3]
    );
    assert!(
        results[4].contains("did not run"),
        "a call after the interrupted one never starts: {}",
        results[4]
    );

    // The pairing invariant: four calls, four results, in order.
    assert_eq!(record.conversation.len(), 7, "user, assistant, five tools");
    let ids: Vec<&str> = record.conversation[2..]
        .iter()
        .map(|message| {
            message["content"][0]["toolCallId"]
                .as_str()
                .unwrap_or_default()
        })
        .collect();
    assert_eq!(ids, ["call_b1", "call_b2", "call_b3", "call_b4", "call_b5"]);
    assert_eq!(
        record.conversation[5]["content"][0]["output"]["value"]["status"],
        "stopped"
    );

    // The ledger keeps three completed runs and one cancellation.
    for id in ["call_b1", "call_b2", "call_b3"] {
        assert_eq!(
            ledger_states_for(&root, id),
            ["proposed", "approved", "started", "completed"],
            "{id} finished"
        );
    }
    assert_eq!(
        ledger_states_for(&root, "call_b4"),
        ["proposed", "approved", "started", "cancelled"],
        "the interrupted call is a cancellation"
    );
    assert!(
        ledger_states_for(&root, "call_b5").is_empty(),
        "a call that never started has no ledger row"
    );

    // The next turn's request carries all four: three outputs and the stop.
    let mut next = request("batch_four", addr, UserEntrySource::Terminal);
    next.workspace_root = Some(root.display().to_string());
    next.text = "what happened?".to_owned();
    let second = runner.send(next).await.expect("second turn");
    assert_eq!(second.outcome, TurnOutcome::Completed);

    let bodies = mock.bodies.lock().unwrap().clone();
    let last = bodies.last().expect("the next turn makes a request");
    assert_eq!(
        last.matches("\"role\":\"tool\"").count(),
        5,
        "five paired tool messages in one request"
    );
    // The pairing is per call and in order: a missing or reordered id
    // would make the next request invalid.
    let mut cursor = 0;
    for id in ["call_b1", "call_b2", "call_b3", "call_b4", "call_b5"] {
        let needle = format!("\"tool_call_id\":\"{id}\"");
        let at = last[cursor..]
            .find(&needle)
            .unwrap_or_else(|| panic!("{id} has no paired result in the next request"));
        cursor += at + needle.len();
    }
    assert!(last.contains("stopped by the user"), "the stop survives");
    assert!(
        last.contains("Start-Sleep") || last.contains("sleep 6"),
        "and the command it interrupted"
    );
    assert!(
        last.contains("one") && last.contains("two") && last.contains("three"),
        "the finished calls keep their outputs"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A stop pressed while a command runs must interrupt that command, and the
/// interruption must survive into the next turn: the pair names the stop and
/// still carries the command it cut short. Without the stop reaching the tool
/// the turn simply waits out the command, and the next request reads it as a
/// finished command — the state the source could not recover from.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_during_a_running_command_ends_it_and_the_next_turn_sees_it() {
    let root = temp_root("stop-command");
    seed_session(&root);
    let (addr, mock) = mock_server().await;
    let door = spawn_door().await;
    let runner = Arc::new(
        TurnRunner::open(&root, SESSION_ID, door, std::sync::Weak::new())
            .await
            .expect("open"),
    );

    let mut req = request("long_command", addr, UserEntrySource::Terminal);
    req.workspace_root = Some(root.display().to_string());
    let worker = {
        let runner = Arc::clone(&runner);
        tokio::spawn(async move { runner.send(req).await })
    };

    // The host's own ledger is the record that the command is running;
    // waiting on it puts the stop inside the command, not before it.
    let deadline = Instant::now() + Duration::from_secs(20);
    while !command_started(&root) {
        assert!(Instant::now() < deadline, "the command never started");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let stopped_at = Instant::now();
    runner.stop(false);
    let turn = worker.await.expect("join").expect("turn settles");
    let elapsed = stopped_at.elapsed();

    assert!(
        elapsed < Duration::from_secs(3),
        "the stop waited {elapsed:?} for the command; it must interrupt it"
    );
    assert_eq!(turn.outcome, TurnOutcome::Cancelled);

    // The transcript names the interruption instead of a finished command.
    let record = saved(&root);
    let tools = tool_entries(&record);
    assert_eq!(tools.len(), 1, "one tool entry");
    let SessionEntry::Tool {
        result, running, ..
    } = &tools[0]
    else {
        unreachable!()
    };
    assert_eq!(
        *running,
        Some(false),
        "the interrupted command settles as not running"
    );
    assert!(
        result.contains("stopped by the user"),
        "the entry names the stop: {result}"
    );
    assert_eq!(
        ledger_states(&root),
        ["proposed", "approved", "started", "cancelled"],
        "the ledger records the interruption, not a finished command"
    );

    // The next turn's request carries the stopped pair together with the
    // command it cut short, so the model knows what did not finish.
    let mut next = request("long_command", addr, UserEntrySource::Terminal);
    next.workspace_root = Some(root.display().to_string());
    next.text = "what happened?".to_owned();
    let second = runner.send(next).await.expect("second turn");
    assert_eq!(second.outcome, TurnOutcome::Completed);

    let bodies = mock.bodies.lock().unwrap().clone();
    let last = bodies.last().expect("the next turn makes a request");
    assert!(
        last.contains("what happened?"),
        "the last body is the next turn's request"
    );
    assert!(
        last.contains("stopped by the user"),
        "the next turn sees the interrupted command: {last}"
    );
    assert!(
        last.contains("Start-Sleep") || last.contains("sleep 6"),
        "and the command it named: {last}"
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

/// The step boundary serves a queued compaction *before the next
/// model request*: the second turn request carries the checkpoint-first
/// conversation, not the original one (legacy `compactBeforeStep`,
/// agent.ts L426 — the safe boundary the request waited for).
#[tokio::test]
async fn a_queued_compaction_serves_at_the_step_boundary_before_the_next_request() {
    let root = temp_root("compact-boundary");
    seed_compactable(&root);
    // No modelSpecs: the automatic half stays silent (no budget), so
    // exactly three calls pin the queued-forced compaction's place.
    let (addr, mock) = mock_server().await;
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

    // The batch scenario: first request asks for a tool, the second
    // finishes — the boundary sits between them.
    runner.enqueue(request("batch", addr, UserEntrySource::Terminal));
    for _ in 0..500 {
        if runner.is_turn_active() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(runner.is_turn_active(), "the turn owns the session");
    Arc::clone(&runner).compact(false).await.expect("queued");

    // The turn settles (served compaction inline), then the row.
    for _ in 0..500 {
        if !runner.is_turn_active() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
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
    assert!(landed, "the queued row settles");

    // The ordering pin: turn call, compaction call, final call — and
    // the final call already carries the swapped conversation.
    let bodies = mock.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 3, "tool turn, compaction, final turn");
    let final_request: serde_json::Value =
        serde_json::from_str(&bodies[2]).expect("final request is JSON");
    assert!(
        final_request["messages"][0]["content"]
            .as_str()
            .is_some_and(|text| text.starts_with("OpenButler conversation summary (context only):")),
        "the boundary swapped before the next request: {:?}",
        final_request["messages"][0]
    );
    // The queued notice carries the running turn (the source's
    // addEntry stamps activeTurnId while a run is live).
    let record = saved(&root);
    // The run's own user entry (last wins — `lastTranscriptTurnId`):
    // the seed's entry is a different, older turn.
    let user_turn = record.entries.iter().rev().find_map(|entry| match entry {
        SessionEntry::User { meta, .. } => Some(meta.turn_id.clone()),
        _ => None,
    });
    let queued_turn = record.entries.iter().find_map(|entry| match entry {
        SessionEntry::Notice { meta, text, .. }
            if text == "Compaction is queued; it runs at the next step boundary or when the run finishes." =>
        {
            Some(meta.turn_id.clone())
        }
        _ => None,
    });
    assert_eq!(
        queued_turn.flatten(),
        user_turn.flatten(),
        "the queued notice names the running turn"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Above the trigger the boundary compacts on its own — before the
/// first request even goes out (legacy: the callback runs
/// `compactConversation` without force, the engine's trigger decides).
#[tokio::test]
async fn the_boundary_compacts_automatically_above_the_trigger() {
    let root = temp_root("compact-auto");
    // Pure byte path: no measured context, so the estimate alone has
    // to clear the 40,210-token trigger for the 80,420 window.
    let filler = "lorem ipsum dolor sit amet consectetur ".repeat(4_700); // ~174 KB
    let json = format!(
        r#"{{
  "version": 1,
  "id": "{SESSION_ID}",
  "title": "Auto compact",
  "createdAt": "2026-01-01T00:00:00.000Z",
  "updatedAt": "2026-01-01T00:00:00.000Z",
  "providerId": "openai-compatible",
  "modelId": "test-model",
  "entries": [
    {{
      "kind": "user",
      "text": "summarize the archive",
      "source": "terminal",
      "turnId": "turn-1"
    }}
  ],
  "conversation": [
    {{ "role": "user", "content": "{filler}" }},
    {{ "role": "assistant", "content": "acknowledged" }}
  ]
}}"#
    );
    let record = parse_record(&json).expect("seed record parses");
    match save_session(&root, &record, true).expect("seed save") {
        SaveOutcome::Written(_) => {}
        SaveOutcome::Skipped => panic!("allowEmpty writes the seed"),
    }
    let (addr, mock) = mock_server().await;
    std::fs::write(
        root.join("settings.yaml"),
        format!(
            "providers:\n  openai-compatible:\n    enabled: true\n    url: http://{addr}/basic\n    modelSpecs:\n      test-model:\n        context: 80420\n"
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

    let turn = runner
        .send(request("basic", addr, UserEntrySource::Terminal))
        .await
        .expect("turn");
    assert_eq!(turn.outcome, TurnOutcome::Completed);

    // The compaction ran first (bodies[0]), then the turn request
    // went out over the compacted conversation (bodies[1]).
    let bodies = mock.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 2, "auto compaction then one turn call");
    let turn_request: serde_json::Value =
        serde_json::from_str(&bodies[1]).expect("turn request is JSON");
    assert!(
        turn_request["messages"][0]["content"]
            .as_str()
            .is_some_and(|text| text.starts_with("OpenButler conversation summary (context only):")),
        "the first request already runs on the checkpoint: {:?}",
        turn_request["messages"][0]
    );
    let record = saved(&root);
    assert!(
        record.entries.iter().any(|entry| matches!(
            entry,
            SessionEntry::Compaction {
                status: openbutler_protocol::entry::CompactionStatus::Done,
                ..
            }
        )),
        "the automatic row settles: {:?}",
        record.entries
    );

    let _ = std::fs::remove_dir_all(&root);
}

// --- usage accounting (onStepUsage -> the ledger) ---------------------------

/// One finished step -> one `openbutler_usage` row: the source's key,
/// run kind, status, and the token counts the fake stated.
#[tokio::test]
async fn a_turn_records_one_usage_row_per_model_request() {
    let root = temp_root("usage");
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

    let rows = usage_rows(&root);
    assert_eq!(rows.len(), 1, "one model request, one row: {rows:?}");
    let row = &rows[0];
    assert_eq!(
        row.id,
        format!("{}:{}:1", SESSION_ID, row.at_ms),
        "the source's primary key: sessionId:startedAt:attempt"
    );
    assert_eq!(row.run_kind, "chat");
    assert_eq!(row.status, "ok");
    assert_eq!(row.attempt, 1);
    assert_eq!(row.provider_id, "openai-compatible");
    assert_eq!(row.model_id, "test-model");
    assert_eq!(
        (row.input_tokens, row.output_tokens, row.total_tokens),
        (Some(10), Some(5), Some(15))
    );
    assert_eq!(row.usage_source, "provider");
    assert_eq!(
        row.cost_source.as_deref(),
        Some("none"),
        "the fake states no cost"
    );
    assert!(row.duration_ms >= 0, "duration is recorded");
    assert_eq!(
        row.generation_id.as_deref(),
        Some("c1"),
        "the provider's response id travels through finish-step"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// The turn's step index is the source's stepCount: a tool round-trip
/// records attempt 1, then 2 — one row per model request, never one per
/// turn.
#[tokio::test]
async fn each_step_of_a_tool_turn_records_its_attempt() {
    let root = temp_root("usage-steps");
    seed_session(&root);
    let (addr, _mock) = mock_server().await;
    let door = spawn_door().await;
    let runner = TurnRunner::open(&root, SESSION_ID, door, std::sync::Weak::new())
        .await
        .expect("open");

    let mut req = request("batch", addr, UserEntrySource::Terminal);
    req.workspace_root = Some(root.display().to_string());
    let turn = runner.send(req).await.expect("turn");
    assert_eq!(turn.outcome, TurnOutcome::Completed);

    let rows = usage_rows(&root);
    let attempts: Vec<i64> = rows.iter().map(|row| row.attempt).collect();
    assert_eq!(attempts, vec![1, 2], "one row per step, in order: {rows:?}");
    assert!(
        rows.iter()
            .all(|row| row.run_kind == "chat" && row.status == "ok"),
        "chat rows, all settled: {rows:?}"
    );
    assert!(
        rows.iter()
            .all(|row| row.workspace_root.as_deref() == Some(root.to_str().unwrap())),
        "the turn's workspace travels: {rows:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[derive(Debug)]
struct UsageRowView {
    id: String,
    at_ms: i64,
    run_kind: String,
    status: String,
    attempt: i64,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    total_tokens: Option<i64>,
    usage_source: String,
    cost_source: Option<String>,
    cost_usd: Option<f64>,
    duration_ms: i64,
    generation_id: Option<String>,
    provider_id: String,
    model_id: String,
    workspace_root: Option<String>,
}

fn usage_rows(root: &Path) -> Vec<UsageRowView> {
    let connection = rusqlite::Connection::open(root.join("state.db")).unwrap();
    let mut statement = connection
        .prepare(
            "SELECT id, at_ms, run_kind, status, attempt, input_tokens, output_tokens,
                    total_tokens, usage_source, cost_source, cost_usd, duration_ms, generation_id,
                    provider_id, model_id, workspace_root
             FROM openbutler_usage ORDER BY at_ms, attempt",
        )
        .unwrap();
    statement
        .query_map([], |row| {
            Ok(UsageRowView {
                id: row.get(0)?,
                at_ms: row.get(1)?,
                run_kind: row.get(2)?,
                status: row.get(3)?,
                attempt: row.get(4)?,
                input_tokens: row.get(5)?,
                output_tokens: row.get(6)?,
                total_tokens: row.get(7)?,
                usage_source: row.get(8)?,
                cost_source: row.get(9)?,
                cost_usd: row.get(10)?,
                duration_ms: row.get(11)?,
                generation_id: row.get(12)?,
                provider_id: row.get(13)?,
                model_id: row.get(14)?,
                workspace_root: row.get(15)?,
            })
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

// --- catalog pricing on a real turn -----------------------------------------

/// A transport the pricing pin never needs (the catalog is pre-written).
struct NoFetchTransport;

impl openbutler_daemon::catalog::Transport for NoFetchTransport {
    fn fetch(&self, _url: &'static str) -> openbutler_daemon::catalog::FetchFuture {
        Box::pin(async { Err("no network in tests".to_owned()) })
    }
}

/// A finished step is priced from the catalog's real prices (owner rule:
/// an estimate comes from real prices, or there is none) — the host's
/// catalog answers and the row says `catalog-estimate`.
#[tokio::test]
async fn a_turn_prices_its_step_from_the_catalog() {
    let root = temp_root("usage-catalog");
    seed_session(&root);
    let (addr, _mock) = mock_server().await;
    let door = spawn_door().await;

    // The catalog carries the model the turn runs (provider/model ids come
    // from the door request: openai-compatible / test-model).
    openbutler_daemon::catalog::write_catalog_snapshot(
        &root,
        &json!({
            "providers": {
                "openai-compatible": {
                    "models": {
                        "test-model": { "cost": { "input": 2.0, "output": 8.0 } }
                    }
                }
            }
        }),
        1_700_000_000_000,
        1_700_000_000_000,
    )
    .unwrap();
    let host = std::sync::Arc::new(openbutler_daemon::dispatch::Host::new());
    host.set_catalog(Arc::new(
        openbutler_daemon::catalog::CatalogStore::with_transport(&root, Arc::new(NoFetchTransport)),
    ));

    let runner = TurnRunner::open(&root, SESSION_ID, door, Arc::downgrade(&host))
        .await
        .expect("open");
    runner
        .send(request("basic", addr, UserEntrySource::Terminal))
        .await
        .expect("turn");

    let rows = usage_rows(&root);
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(
        row.cost_source.as_deref(),
        Some("catalog-estimate"),
        "a real catalog price labels the estimate"
    );
    // The fake states input 10 / output 5 and no cache tokens:
    // (10 * 2.0 + 5 * 8.0) / 1_000_000.
    let cost = row.cost_usd.expect("the step is priced");
    assert!((cost - 0.000_06).abs() < 1e-12, "got {cost}");
}

#[tokio::test]
async fn a_failed_admission_save_does_not_leave_the_runner_busy() {
    // The failure the guard exists for: the record path becomes a directory,
    // so the admission save cannot write and `send` returns through its
    // first `?`. Before the guard, `active_current` stayed `Some(..)` for
    // good: `session.queue` would accept steered messages nothing drains and
    // a queued compaction would wait forever.
    let root = temp_root("admit-fail");
    seed_session(&root);
    let door = spawn_door().await;
    let runner = TurnRunner::open(&root, SESSION_ID, door, std::sync::Weak::new())
        .await
        .expect("open");

    let record = root
        .join("sessions")
        .join(SESSION_ID)
        .join(format!("{SESSION_ID}.json"));
    std::fs::remove_file(&record).expect("remove the seeded record");
    std::fs::create_dir(&record).expect("a directory where the record must be");

    let addr: SocketAddr = "127.0.0.1:9".parse().expect("addr");
    let error = runner
        .send(request("never-reached", addr, UserEntrySource::Terminal))
        .await
        .expect_err("the admission save must fail");
    assert!(
        !runner.is_turn_active(),
        "a turn that never ran must not keep the runner busy: {error}"
    );
}
