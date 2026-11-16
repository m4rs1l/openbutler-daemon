//! M2b criterion 4: worst-case memory measurement for the real sidecar.
//! Four concurrent calls carry a realistic worst payload (long history,
//! a megabyte-class image data URL, a huge assistant turn); the test
//! samples the process while it parses and builds every request and
//! pins the ceiling the contract promises — measured, not guessed.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{Response, StatusCode, header};
use axum::routing::post;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Observed Windows peak **with the production heap belt**
/// (`--max-old-space-size=64`): **107 MB working set / 139 MB private**
/// (12 samples at 100 ms, 4 concurrent ~2.5 MB calls; without the belt
/// the same load peaks 136/169). The ceiling sits just under 2× the
/// belted peak so a doubling trips it.
const RSS_CEILING_BYTES: u64 = 200 * 1024 * 1024;

/// How long the mock holds a response open — long enough for samples to
/// cover the request-build peak and afterwards.
const HOLD: Duration = Duration::from_millis(1200);

const CALLS: u32 = 4;

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
    Path(_scenario): Path<String>,
    _body: String,
) -> Response<Body> {
    tokio::time::sleep(HOLD).await;
    sse(
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"}}]}\n\n\
         data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":3,\"total_tokens\":103}}\n\n\
         data: [DONE]\n\n",
    )
}

async fn mock_server() -> SocketAddr {
    let app = Router::new()
        .route("/{scenario}/chat/completions", post(chat))
        .with_state(Mock)
        // The worst-case payload is megabytes per call; the default
        // 2 MB body limit would silently reject exactly what we measure.
        .layer(DefaultBodyLimit::max(32 * 1024 * 1024));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

fn entry_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("sidecar")
        .join("src")
        .join("entry.mjs")
}

/// One measurement from the sampler.
struct Measurement {
    peak_working: u64,
    peak_private: u64,
    samples: u32,
}

/// One persistent sampler process/task: a fresh powershell per sample
/// would miss the request-build peak behind its startup cost.
async fn start_sampler(
    pid: u32,
) -> (
    Arc<tokio::sync::Notify>,
    tokio::task::JoinHandle<Measurement>,
) {
    let stop = Arc::new(tokio::sync::Notify::new());
    let stop_signal = stop.clone();
    let handle = tokio::spawn(async move {
        let mut measurement = Measurement {
            peak_working: 0,
            peak_private: 0,
            samples: 0,
        };
        #[cfg(windows)]
        {
            // `("{0} {1}" -f …)`: expandable strings do not resolve
            // property access — `$p.WorkingSet64` would print an object.
            let script = format!(
                "while($true){{ $p = Get-Process -Id {pid} -ErrorAction SilentlyContinue; if($p){{ Write-Output (\"{{0}} {{1}}\" -f $p.WorkingSet64, $p.PrivateMemorySize64) }}; Start-Sleep -Milliseconds 100 }}"
            );
            let mut child = tokio::process::Command::new("powershell")
                .args(["-NoProfile", "-Command", &script])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("the sampler runs");
            let stdout = child.stdout.take().unwrap();
            let mut lines = BufReader::new(stdout).lines();
            loop {
                tokio::select! {
                    _ = stop_signal.notified() => {
                        let _ = child.kill().await;
                        break;
                    }
                    line = lines.next_line() => {
                        let Ok(Some(line)) = line else { break; };
                        let mut numbers = line.split_whitespace().filter_map(|n| n.parse::<u64>().ok());
                        if let (Some(working), Some(private)) = (numbers.next(), numbers.next()) {
                            measurement.peak_working = measurement.peak_working.max(working);
                            measurement.peak_private = measurement.peak_private.max(private);
                            measurement.samples += 1;
                        }
                    }
                }
            }
        }
        #[cfg(unix)]
        {
            loop {
                tokio::select! {
                    _ = stop_signal.notified() => break,
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {
                        if let Some((working, private)) = sample_rss(pid).await {
                            measurement.peak_working = measurement.peak_working.max(working);
                            measurement.peak_private = measurement.peak_private.max(private);
                            measurement.samples += 1;
                        }
                    }
                }
            }
        }
        measurement
    });
    (stop, handle)
}

/// A worst-ish payload: a long history, a megabyte-class image data URL
/// inside a real content part, and a huge assistant turn.
fn big_messages() -> serde_json::Value {
    let text: String = "The quick brown fox jumps over the lazy dog. ".repeat(16_000); // ~700 KB
    let pixels: String = "A".repeat(1_500_000); // base64-class payload
    let tail: String = "Answer carefully, in detail. ".repeat(9_000); // ~250 KB
    json!([
        { "role": "user", "content": text },
        {
            "role": "user",
            "content": [
                { "type": "text", "text": "look at this" },
                { "type": "image", "image": format!("data:image/png;base64,{pixels}") }
            ]
        },
        { "role": "assistant", "content": tail },
        { "role": "user", "content": "go" }
    ])
}

fn big_call(id: &str, addr: SocketAddr) -> String {
    json!({
        "type": "call",
        "id": id,
        "provider": { "key": "openai-compatible", "url": format!("http://{addr}/rss") },
        "model": "test-model",
        "messages": big_messages(),
    })
    .to_string()
}

#[tokio::test]
async fn the_worst_case_payload_stays_within_the_rss_ceiling() {
    let addr = mock_server().await;

    let mut child = tokio::process::Command::new("node")
        // The production spawn profile: the heap belt is part of every
        // measurement, never an afterthought.
        .arg("--max-old-space-size=64")
        .arg(entry_script())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("node runs the entry");
    let pid = child.id().expect("the child has a pid");
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();

    let ready = lines.next_line().await.unwrap().unwrap();
    assert!(ready.contains("\"type\":\"ready\""));

    // Sample from before the first byte of load to well after it.
    let (stop, sampler) = start_sampler(pid).await;

    // Four concurrent calls: the concurrency cap the host would apply.
    for index in 0..CALLS {
        let line = big_call(&format!("rss:{index}"), addr);
        stdin.write_all(line.as_bytes()).await.unwrap();
        stdin.write_all(b"\n").await.unwrap();
    }
    stdin.flush().await.unwrap();

    // Every call ends with finish (or a structured error the provider
    // returned — either way the stream closes).
    let mut finished = 0;
    while finished < CALLS {
        let line = lines.next_line().await.unwrap().expect("events arrive");
        let value: serde_json::Value = serde_json::from_str(&line).expect("stdout stays JSON");
        if value["type"] == "finish" || value["type"] == "error" {
            finished += 1;
        }
    }

    stop.notify_waiters();
    let measurement = sampler.await.expect("the sampler task finishes");
    drop(stdin);
    let _ = child.wait().await;

    let Measurement {
        peak_working: working,
        peak_private: private,
        samples,
    } = measurement;
    // A sampler that observed nothing must never read as "within the
    // ceiling": 0 ≤ anything would pass vacuously.
    assert!(
        working > 0 && samples > 0,
        "the sampler must observe the sidecar ({samples} samples)"
    );
    let working_mb = working / (1024 * 1024);
    let private_mb = private / (1024 * 1024);
    println!(
        "sidecar worst-case peak: working set {working_mb} MB, private {private_mb} MB, \
         {samples} samples, {CALLS} concurrent calls of ~2.5 MB each"
    );
    assert!(
        working <= RSS_CEILING_BYTES,
        "worst-case working set {working_mb} MB exceeds the {RSS_CEILING_BYTES} byte ceiling",
    );
}
