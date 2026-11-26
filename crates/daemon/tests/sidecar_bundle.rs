//! M2b criterion 3, bundle half: the built bundle must keep lazy
//! per-provider chunks (one model call loads one provider chunk, not
//! the whole set), carry every embedded provider, and behave exactly
//! like the source entry — pinned SDK version included.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxPath, State};
use axum::http::{Response, StatusCode, header};
use axum::routing::post;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

const PROVIDERS: [&str; 12] = [
    "anthropic",
    "cerebras",
    "cohere",
    "deepinfra",
    "gateway",
    "google",
    "groq",
    "mistral",
    "openai",
    "openai-compatible",
    "togetherai",
    "xai",
];

fn sidecar_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("sidecar")
}

#[derive(Clone)]
struct Mock;

fn sse(body: &str) -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

async fn chat(
    State(_mock): State<Mock>,
    AxPath(_scenario): AxPath<String>,
    _body: String,
) -> Response<Body> {
    sse(
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"}}]}\n\n\
         data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" bundle\"}}]}\n\n\
         data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2,\"total_tokens\":9}}\n\n\
         data: [DONE]\n\n",
    )
}

async fn mock_server() -> SocketAddr {
    let app = Router::new()
        .route("/{scenario}/chat/completions", post(chat))
        .with_state(Mock);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

#[tokio::test]
async fn the_bundle_splits_lazy_chunks_and_behaves_like_the_source() {
    // 1. Build — the machine this runs on may omit devDependencies
    //    (NODE_ENV=production): the build is invoked explicitly.
    let build = Command::new("node")
        .arg("build.mjs")
        .current_dir(sidecar_dir())
        .stdin(Stdio::null())
        .output()
        .await
        .expect("the bundler runs");
    assert!(
        build.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );

    // 2. The metafile proves the contract: every provider is in the
    //    graph, and splitting produced a chunked output set.
    let meta: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(sidecar_dir().join("dist/metafile.json"))
            .expect("the metafile is written"),
    )
    .expect("the metafile parses");
    let inputs: Vec<String> = meta["inputs"]
        .as_object()
        .unwrap()
        .keys()
        .map(|key| key.replace('\\', "/"))
        .collect();
    for provider in PROVIDERS {
        assert!(
            inputs
                .iter()
                .any(|input| input.contains(&format!("@ai-sdk/{provider}/"))),
            "`{provider}` must be part of the bundle graph"
        );
    }
    let outputs: Vec<&String> = meta["outputs"]
        .as_object()
        .unwrap()
        .keys()
        .filter(|name| name.ends_with(".js"))
        .collect();
    assert!(
        outputs.len() >= 12,
        "splitting must yield a chunk set, got {}",
        outputs.len()
    );

    // 3. Run the built entry with module tracing: it must answer the
    //    handshake and one call exactly like the source entry, while
    //    loading strictly fewer chunks than exist (lazy providers).
    let addr = mock_server().await;
    let mut child = Command::new("node")
        .env("NODE_DEBUG", "esm")
        .arg(sidecar_dir().join("dist").join("entry.js"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("node runs the bundle");
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    // Drain stderr concurrently — a full pipe freezes the child.
    let tracer = tokio::spawn(async move {
        let mut trace = String::new();
        let mut reader = BufReader::new(stderr);
        let mut chunk = String::new();
        while reader.read_line(&mut chunk).await.unwrap() > 0 {
            trace.push_str(&chunk);
            chunk.clear();
        }
        trace
    });

    let mut lines = BufReader::new(stdout).lines();
    let ready =
        serde_json::from_str::<serde_json::Value>(lines.next_line().await.unwrap().unwrap().trim())
            .expect("ready is JSON");
    assert_eq!(ready["aiSdkVersion"], "7.0.173");
    assert_eq!(ready["protocolVersion"], 1);
    assert_eq!(ready["providers"].as_array().unwrap().len(), 12);

    let call = json!({
        "type": "call",
        "id": "bundle:1",
        "provider": { "key": "openai-compatible", "url": format!("http://{addr}/basic") },
        "model": "test-model",
        "messages": [{ "role": "user", "content": "hi" }],
    });
    stdin
        .write_all(format!("{call}\n").as_bytes())
        .await
        .unwrap();

    let mut deltas: Vec<String> = Vec::new();
    let mut finished = false;
    while !finished {
        let line = lines.next_line().await.unwrap().expect("events arrive");
        let value: serde_json::Value = serde_json::from_str(&line).expect("stdout stays JSON");
        match value["type"].as_str() {
            Some("text-delta") => deltas.push(value["delta"].as_str().unwrap().to_owned()),
            Some("usage") | Some("finish") => finished = value["type"] == "finish",
            Some(other) => panic!("unexpected event {other}: {line}"),
            None => {}
        }
    }
    drop(stdin);
    let trace = tracer.await.unwrap();
    let _ = child.wait().await;
    assert_eq!(deltas, vec!["Hello", " bundle"], "identical behavior");

    // 4. Lazy proof on the built artifact: the distinct dist files the
    //    loader touched are far fewer than the outputs — the other
    //    providers' chunks stayed on disk, unexecuted.
    // NODE_DEBUG prints normalized file URLs. sidecar_dir() keeps a
    // `..` segment, so the raw path never matches — canonicalize first;
    // on Windows that returns the verbatim `\\?\C:\...` form, whose URL
    // form must drop the whole `//?/` prefix or the root ends up with
    // four slashes and never matches.
    let dist_root = {
        let raw = std::fs::canonicalize(sidecar_dir().join("dist"))
            .expect("dist must exist after the build")
            .to_string_lossy()
            .replace('\\', "/");
        let normalized = raw.strip_prefix("//?/").unwrap_or(&raw);
        format!("file:///{normalized}")
    };
    let mut loaded: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for line in trace.lines() {
        if let Some(start) = line.find(&dist_root) {
            let rest = &line[start..];
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            let path = &rest[..end];
            if path.ends_with(".js") {
                loaded.insert(path.to_owned());
            }
        }
    }
    assert!(
        !loaded.is_empty(),
        "the tracer must show the dist files it loaded"
    );
    assert!(
        loaded.len() < outputs.len(),
        "one call must not load every chunk: {} loaded of {} outputs",
        loaded.len(),
        outputs.len()
    );
    // Every loaded file must live under dist (nothing leaked out).
    assert!(
        loaded.iter().all(|path| path.starts_with(&dist_root)),
        "unexpected loaded path: {loaded:?}"
    );
}
