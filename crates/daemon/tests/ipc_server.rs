#![allow(clippy::unwrap_used, clippy::expect_used)]
//! IPC server contract tests: endpoint derivation, framing, dispatch.
//!
//! Fail-before: every test runs its own server on a scratch endpoint (never
//! the machine's real pipe) and speaks the wire protocol as a client would.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::{pretty_session, temp_data_dir, uuid, write_session};
use openbutler_daemon::dispatch::Host;
use openbutler_daemon::server;
use openbutler_protocol::{DAEMON_PROTOCOL_VERSION, DaemonResponse, validate_response};
use serde_json::Value;

// --- Endpoint derivation ----------------------------------------------------

#[cfg(windows)]
#[test]
fn endpoint_matches_the_legacy_pipe_name_formula() {
    use sha2::{Digest, Sha256};

    // `daemon.ts getDaemonEndpoint`: sha256 of the lowercased resolved data
    // directory, first 24 hex chars, `\\.\pipe\openbutler-<suffix>`.
    let data_dir = r"C:\ob-endpoint-fixture";
    let mut hasher = Sha256::new();
    hasher.update(data_dir.to_lowercase().to_ascii_lowercase().as_bytes());
    let digest = hasher.finalize();
    let suffix: String = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();

    let endpoint = server::endpoint_for(Path::new(data_dir));
    let server::Endpoint::NamedPipe(name) = endpoint.target() else {
        panic!("expected a named pipe endpoint on Windows");
    };
    assert_eq!(*name, format!(r"\\.\pipe\openbutler-{suffix}"));
}

#[cfg(unix)]
#[test]
fn endpoint_is_the_daemon_socket_inside_the_data_dir() {
    let endpoint = server::endpoint_for(Path::new("/tmp/ob-endpoint-fixture"));
    let server::Endpoint::UnixSocket(path) = endpoint.target() else {
        panic!("expected a unix socket endpoint");
    };
    assert_eq!(
        path,
        &std::path::PathBuf::from("/tmp/ob-endpoint-fixture/daemon.sock")
    );
}

// --- Helpers ----------------------------------------------------------------

/// A test client: named pipe on Windows, unix socket elsewhere.
#[cfg(windows)]
mod client {
    pub type Stream = tokio::net::windows::named_pipe::NamedPipeClient;

    pub async fn connect(endpoint: &openbutler_daemon::server::ResolvedEndpoint) -> Stream {
        loop {
            match tokio::net::windows::named_pipe::ClientOptions::new().open(endpoint.socket_name())
            {
                Ok(client) => return client,
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            }
        }
    }
}

#[cfg(unix)]
mod client {
    pub type Stream = tokio::net::UnixStream;

    pub async fn connect(endpoint: &openbutler_daemon::server::ResolvedEndpoint) -> Stream {
        loop {
            match Stream::connect(endpoint.socket_name()).await {
                Ok(stream) => return stream,
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            }
        }
    }
}

async fn request(client: &mut client::Stream, frame: &str) -> Value {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // Frames are JSON + newline on the wire.
    let mut payload = frame.as_bytes().to_vec();
    payload.push(b'\n');
    client.write_all(&payload).await.unwrap();
    let mut buffer = Vec::new();
    loop {
        let mut chunk = [0u8; 4096];
        let read = client.read(&mut chunk).await.unwrap();
        assert!(read > 0, "connection closed before a response arrived");
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
            let line = String::from_utf8(buffer[..newline].to_vec()).unwrap();
            return serde_json::from_str(&line).expect("response frame must be JSON");
        }
    }
}

// --- Status -----------------------------------------------------------------

#[tokio::test]
async fn status_reports_this_process_over_the_wire() {
    let data_dir = temp_data_dir("ipc-status");
    let endpoint = server::endpoint_for(&data_dir);
    let host = Arc::new(Host::new());
    let shutdown = host.subscribe_shutdown();
    let served = tokio::spawn(server::serve(endpoint.clone(), host.clone(), shutdown));

    let mut client = client::connect(&endpoint).await;
    let response: Value = request(
        &mut client,
        r#"{"version":2,"id":"t-status","action":"status"}"#,
    )
    .await;

    let typed: DaemonResponse = serde_json::from_value(response).unwrap();
    validate_response(&typed, DAEMON_PROTOCOL_VERSION).expect("envelope must validate");
    let DaemonResponse::Success { result, .. } = typed else {
        panic!("status must succeed");
    };
    let status: openbutler_protocol::DaemonStatus = serde_json::from_value(result).unwrap();
    assert_eq!(status.pid, std::process::id());
    assert_eq!(status.workspace_count, 0, "no workspace runtimes yet");
    assert_eq!(
        status.build_id,
        Some(None),
        "a source run reports buildId as explicit null"
    );
    assert!(status.started_at.ends_with('Z'));

    host.request_shutdown();
    served
        .await
        .expect("server must stop")
        .expect("server must stop cleanly");
    let _ = std::fs::remove_dir_all(&data_dir);
}

// --- session.list ------------------------------------------------------------

#[tokio::test]
async fn session_list_returns_summaries_from_disk() {
    let data_dir = temp_data_dir("ipc-list");
    let id = uuid(21);
    write_session(
        &data_dir,
        &id,
        &pretty_session(&id, None, "Wire chat", true),
    );

    let endpoint = server::endpoint_for(&data_dir);
    let host = Arc::new(Host::new());
    let shutdown = host.subscribe_shutdown();
    let served = tokio::spawn(server::serve(endpoint.clone(), host.clone(), shutdown));

    let mut client = client::connect(&endpoint).await;
    let response: Value = request(
        &mut client,
        r#"{"version":2,"id":"t-list","action":"session.list"}"#,
    )
    .await;

    let typed: DaemonResponse = serde_json::from_value(response).unwrap();
    validate_response(&typed, DAEMON_PROTOCOL_VERSION).expect("envelope must validate");
    let DaemonResponse::Success { result, .. } = typed else {
        panic!("session.list must succeed");
    };
    let sessions = result.as_array().expect("result is the summary array");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["id"], id);
    assert_eq!(sessions[0]["title"], "Wire chat");

    host.request_shutdown();
    served
        .await
        .expect("server must stop")
        .expect("server must stop cleanly");
    let _ = std::fs::remove_dir_all(&data_dir);
}

// --- Failure paths -----------------------------------------------------------

#[tokio::test]
async fn unknown_action_is_a_failure_response() {
    let data_dir = temp_data_dir("ipc-unknown");
    let endpoint = server::endpoint_for(&data_dir);
    let host = Arc::new(Host::new());
    let shutdown = host.subscribe_shutdown();
    let served = tokio::spawn(server::serve(endpoint.clone(), host.clone(), shutdown));

    let mut client = client::connect(&endpoint).await;
    let response: Value = request(
        &mut client,
        r#"{"version":2,"id":"t-unknown","action":"does.not.exist"}"#,
    )
    .await;

    let typed: DaemonResponse = serde_json::from_value(response).unwrap();
    validate_response(&typed, DAEMON_PROTOCOL_VERSION).expect("envelope must validate");
    let DaemonResponse::Failure { error, ok, .. } = typed else {
        panic!("unknown actions must fail");
    };
    assert!(!ok);
    assert!(error.contains("does.not.exist"));

    host.request_shutdown();
    served
        .await
        .expect("server must stop")
        .expect("server must stop cleanly");
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[tokio::test]
async fn unsupported_version_is_rejected_with_the_current_version_echoed() {
    let data_dir = temp_data_dir("ipc-version");
    let endpoint = server::endpoint_for(&data_dir);
    let host = Arc::new(Host::new());
    let shutdown = host.subscribe_shutdown();
    let served = tokio::spawn(server::serve(endpoint.clone(), host.clone(), shutdown));

    let mut client = client::connect(&endpoint).await;
    let response: Value = request(
        &mut client,
        r#"{"version":1,"id":"t-v1","action":"status"}"#,
    )
    .await;

    assert_eq!(response["version"], DAEMON_PROTOCOL_VERSION);
    assert_eq!(response["ok"], false);
    assert!(
        response["error"]
            .as_str()
            .expect("error text")
            .contains("protocol version")
    );

    host.request_shutdown();
    served
        .await
        .expect("server must stop")
        .expect("server must stop cleanly");
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[tokio::test]
async fn malformed_frame_closes_the_connection() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let data_dir = temp_data_dir("ipc-malformed");
    let endpoint = server::endpoint_for(&data_dir);
    let host = Arc::new(Host::new());
    let shutdown = host.subscribe_shutdown();
    let served = tokio::spawn(server::serve(endpoint.clone(), host.clone(), shutdown));

    let mut client = client::connect(&endpoint).await;
    client.write_all(b"{not json\n").await.unwrap();
    let mut buffer = [0u8; 64];
    let read = client.read(&mut buffer).await.unwrap();
    assert_eq!(read, 0, "the server must close a malformed connection");

    host.request_shutdown();
    served
        .await
        .expect("server must stop")
        .expect("server must stop cleanly");
    let _ = std::fs::remove_dir_all(&data_dir);
}

// --- Lifecycle ---------------------------------------------------------------

#[tokio::test]
async fn stop_action_shuts_the_server_down() {
    let data_dir = temp_data_dir("ipc-stop");
    let endpoint = server::endpoint_for(&data_dir);
    let host = Arc::new(Host::new());
    let shutdown = host.subscribe_shutdown();
    let served = tokio::spawn(server::serve(endpoint.clone(), host.clone(), shutdown));

    let mut client = client::connect(&endpoint).await;
    let response: Value = request(
        &mut client,
        r#"{"version":2,"id":"t-stop","action":"stop"}"#,
    )
    .await;
    assert_eq!(response["ok"], true);

    tokio::time::timeout(Duration::from_secs(5), served)
        .await
        .expect("serve must return after stop")
        .expect("serve task must not panic")
        .expect("serve must exit cleanly");
    let _ = std::fs::remove_dir_all(&data_dir);
}
