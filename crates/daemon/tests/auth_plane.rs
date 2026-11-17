#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Auth plane contract: the token store, the one-shot pairing state, the
//! `pair.create` action, and the remote TCP admission gate.
//!
//! Fail-before: these run against scratch data directories and loopback
//! listeners, so the rules (salted-hash store that never holds the
//1 plaintext, fail-closed verification, single-use pairing codes, the three
//! fixed refusals, the 10 s silent-socket deadline) are pinned without
//! touching any real install or port.

use std::path::PathBuf;
use std::time::Duration;

use openbutler_daemon::auth::{PairingCodes, TokenStore};
use openbutler_daemon::dispatch::{self, Host};
use openbutler_daemon::server;
use openbutler_protocol::{AUTH_FAILED, DaemonRequest, DaemonResponse, REMOTE_FIRST_FRAME_REFUSED};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

fn scratch(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("ob-auth-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

// --- Token store -------------------------------------------------------------

#[test]
fn mint_is_verifiable_and_the_store_never_holds_the_plaintext() {
    let dir = scratch("mint");
    let store = TokenStore::new(&dir);

    let minted = store.mint().expect("mint must succeed on a fresh dir");
    assert!(minted.token.starts_with("obt_"), "token carries its prefix");
    assert_eq!(minted.id.len(), 16, "id is eight random bytes, hex");
    assert!(
        store.verify(&minted.token).expect("readable store"),
        "the minted token verifies"
    );
    assert!(
        !store.verify("obt_wrong_token").expect("readable store"),
        "a foreign token is rejected"
    );

    let raw = std::fs::read_to_string(dir.join("auth-tokens.json")).expect("store file exists");
    assert!(
        !raw.contains(&minted.token),
        "the plaintext token must never be written to disk"
    );
    assert!(raw.contains(&minted.id), "the record keeps its id");
    let parsed: Value = serde_json::from_str(&raw).expect("store is valid JSON");
    assert_eq!(parsed["version"], 1, "store format version");
    let record = &parsed["tokens"][0];
    assert!(
        record["salt"].as_str().unwrap().len() == 32,
        "16-byte salt, hex"
    );
    assert!(record["hash"].as_str().unwrap().len() == 64, "sha-256, hex");
    assert!(
        record["createdAt"].as_str().unwrap().ends_with('Z'),
        "RFC 3339 creation stamp"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn verification_fails_closed() {
    let dir = scratch("closed");
    let store = TokenStore::new(&dir);

    assert!(
        !store
            .verify("obt_anything")
            .expect("an absent store is not an error"),
        "an absent store admits nobody"
    );

    std::fs::write(dir.join("auth-tokens.json"), "{ not json").unwrap();
    assert!(
        store.verify("obt_anything").is_err(),
        "a corrupt store is an error the gate turns into a refusal, never an admit"
    );

    let error = TokenStore::new(&dir)
        .mint()
        .expect_err("minting into a corrupt store must fail");
    assert!(
        error.to_string().contains("auth-tokens.json"),
        "the mint error names the path: {error}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// --- Pairing codes ----------------------------------------------------------

#[test]
fn a_pairing_code_redeems_once_and_wrong_guesses_burn_nothing() {
    let pairing = PairingCodes::new();
    let before_issue = now_ms();
    let (code, expires_at) = pairing.issue(before_issue).expect("os entropy");

    assert_eq!(code.len(), 12, "six random bytes, hex");
    assert!(
        code.chars().all(|c| c.is_ascii_hexdigit()),
        "hex digits only"
    );
    assert_eq!(
        expires_at,
        before_issue + 300_000,
        "five minute TTL from the issue clock"
    );

    assert!(
        !pairing.redeem("000000000000", now_ms()),
        "wrong code fails"
    );
    assert!(
        pairing.redeem(&code, now_ms()),
        "the real code still works after a wrong guess"
    );
    assert!(
        !pairing.redeem(&code, now_ms()),
        "a redeemed code is consumed exactly once"
    );

    let _ = std::fs::remove_dir_all(scratch("pairing"));
}

#[test]
fn an_expired_code_does_not_redeem_and_a_reissue_kills_the_previous_code() {
    let pairing = PairingCodes::new();

    let (expired, _) = pairing.issue(1_000).expect("os entropy");
    assert!(
        !pairing.redeem(&expired, 1_000 + 300_001),
        "past the TTL the code is dead"
    );

    let (old, _) = pairing.issue(2_000).expect("os entropy");
    let (fresh, _) = pairing.issue(2_001).expect("os entropy");
    assert_ne!(old, fresh, "each issue mints a new code");
    assert!(
        !pairing.redeem(&old, 2_002),
        "reissue kills the previous code"
    );
    assert!(pairing.redeem(&fresh, 2_002), "the newest code lives");
}

// --- Dispatch ----------------------------------------------------------------

#[test]
fn pair_create_answers_with_a_live_code() {
    let dir = scratch("dispatch");
    let host = Host::new();
    let request: DaemonRequest = serde_json::from_value(json!({
        "version": 2,
        "id": "pair-1",
        "action": "pair.create"
    }))
    .expect("request shape");

    let mut state = dispatch::DispatchState::new();
    let DaemonResponse::Success { result, .. } =
        dispatch::handle(&host, &dir, &mut state, &request)
    else {
        panic!("pair.create must succeed");
    };
    let code = result["code"]
        .as_str()
        .expect("code is a string")
        .to_owned();
    assert_eq!(code.len(), 12, "12 hex characters");
    let expires_at = result["expiresAt"].as_u64().expect("expiry is epoch ms");
    assert!(expires_at > now_ms(), "expiry is in the future");

    // The same host redeems the code it just issued: dispatch and the gate
    // share one pairing state.
    assert!(
        host.pairing().redeem(&code, now_ms()),
        "the issued code redeems through the shared state"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// --- Remote TCP admission gate -----------------------------------------------

type Lines = tokio::io::Lines<BufReader<tokio::io::ReadHalf<TcpStream>>>;
type Writer = tokio::io::WriteHalf<TcpStream>;

async fn connect(addr: std::net::SocketAddr) -> (Writer, Lines) {
    let stream = TcpStream::connect(addr).await.expect("loopback connect");
    let (read, write) = tokio::io::split(stream);
    (write, BufReader::new(read).lines())
}

async fn send(writer: &mut Writer, value: &Value) {
    let mut frame = value.to_string();
    frame.push('\n');
    writer
        .write_all(frame.as_bytes())
        .await
        .expect("frame write");
}

async fn recv(lines: &mut Lines) -> Value {
    let line = tokio::time::timeout(Duration::from_secs(3), lines.next_line())
        .await
        .expect("response must arrive")
        .expect("connection must not close before a response")
        .expect("a complete line");
    serde_json::from_str(&line).expect("response is one JSON frame")
}

async fn recv_eof(lines: &mut Lines) {
    let outcome = tokio::time::timeout(Duration::from_secs(3), lines.next_line())
        .await
        .expect("the gate must close a refused connection promptly");
    assert!(
        matches!(outcome, Ok(None)),
        "expected EOF after the refusal"
    );
}

#[tokio::test]
async fn a_minted_token_admits_a_remote_socket_and_status_follows() {
    let dir = scratch("gate-ok");
    let host = std::sync::Arc::new(Host::new());
    let addr = server::socket::serve_tcp(
        "127.0.0.1:0",
        host.clone(),
        dir.clone(),
        Duration::from_millis(200),
    )
    .await
    .expect("bind");

    let token = TokenStore::new(&dir).mint().expect("mint").token;
    let (mut write, mut lines) = connect(addr).await;

    send(
        &mut write,
        &json!({"version": 2, "id": "a1", "action": "auth", "token": token}),
    )
    .await;
    let admitted = recv(&mut lines).await;
    assert_eq!(admitted["ok"], true);
    assert_eq!(admitted["result"]["authenticated"], true);
    assert_eq!(admitted["id"], "a1");

    send(
        &mut write,
        &json!({"version": 2, "id": "s1", "action": "status"}),
    )
    .await;
    let status = recv(&mut lines).await;
    assert_eq!(status["ok"], true, "dispatch runs after admission");
    assert!(
        status["result"]["pid"].as_u64().is_some(),
        "real status payload"
    );

    host.request_shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_bad_token_gets_the_fixed_refusal_and_the_connection_closes() {
    let dir = scratch("gate-bad");
    let host = std::sync::Arc::new(Host::new());
    let addr = server::socket::serve_tcp(
        "127.0.0.1:0",
        host.clone(),
        dir.clone(),
        Duration::from_millis(200),
    )
    .await
    .expect("bind");

    let (mut write, mut lines) = connect(addr).await;
    send(
        &mut write,
        &json!({"version": 2, "id": "a2", "action": "auth", "token": "obt_nope"}),
    )
    .await;
    let refused = recv(&mut lines).await;
    assert_eq!(refused["ok"], false);
    assert_eq!(refused["error"], AUTH_FAILED);
    recv_eof(&mut lines).await;

    host.request_shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_well_formed_request_as_first_frame_is_refused_and_closed() {
    let dir = scratch("gate-first");
    let host = std::sync::Arc::new(Host::new());
    let addr = server::socket::serve_tcp(
        "127.0.0.1:0",
        host.clone(),
        dir.clone(),
        Duration::from_millis(200),
    )
    .await
    .expect("bind");

    let (mut write, mut lines) = connect(addr).await;
    send(
        &mut write,
        &json!({"version": 2, "id": "s9", "action": "status"}),
    )
    .await;
    let refused = recv(&mut lines).await;
    assert_eq!(refused["ok"], false);
    assert_eq!(refused["error"], REMOTE_FIRST_FRAME_REFUSED);
    assert_eq!(refused["id"], "s9", "the refusal echoes the frame id");
    recv_eof(&mut lines).await;

    host.request_shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_pairing_frame_over_tcp_mints_a_working_token() {
    let dir = scratch("gate-pair");
    let host = std::sync::Arc::new(Host::new());
    let addr = server::socket::serve_tcp(
        "127.0.0.1:0",
        host.clone(),
        dir.clone(),
        Duration::from_millis(200),
    )
    .await
    .expect("bind");

    let request: DaemonRequest = serde_json::from_value(json!({
        "version": 2, "id": "pc", "action": "pair.create"
    }))
    .expect("request shape");
    let mut state = dispatch::DispatchState::new();
    let DaemonResponse::Success { result, .. } =
        dispatch::handle(&host, &dir, &mut state, &request)
    else {
        panic!("pair.create must succeed");
    };
    let code = result["code"].as_str().expect("code").to_owned();

    let (mut write, mut lines) = connect(addr).await;
    send(
        &mut write,
        &json!({"version": 2, "id": "p1", "action": "pair", "code": code}),
    )
    .await;
    let minted = recv(&mut lines).await;
    assert_eq!(minted["ok"], true);
    let token = minted["result"]["token"]
        .as_str()
        .expect("token")
        .to_owned();

    assert!(
        TokenStore::new(&dir)
            .verify(&token)
            .expect("store readable"),
        "the minted token verifies through the store"
    );

    host.request_shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_silent_remote_socket_is_closed_after_the_auth_deadline() {
    let dir = scratch("gate-silent");
    let host = std::sync::Arc::new(Host::new());
    let addr = server::socket::serve_tcp(
        "127.0.0.1:0",
        host.clone(),
        dir.clone(),
        Duration::from_millis(120),
    )
    .await
    .expect("bind");

    let (_write, mut lines) = connect(addr).await;
    recv_eof(&mut lines).await;

    host.request_shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}
