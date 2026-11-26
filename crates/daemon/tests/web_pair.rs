#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Web/pair surface contract: the bundle guard, static serving rules, the
//! one-shot pairing link, the cookie + origin API gate, and the persistent
//! session token.
//!
//! Fail-before: each expectation mirrors the TypeScript web server field for
//! field (strings are literals here on purpose — a wording change in the
//! implementation must fail this suite), and every server runs on loopback
//! with scratch assets and data directories.

use std::path::{Path, PathBuf};

use openbutler_daemon::server::http::{
    AssetsSource, WebOptions, read_build_id, require_default_assets_dir, start_web_server,
};

fn scratch(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("ob-web-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

/// A minimal but real bundle: index plus one hashed asset.
fn write_bundle(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("index.html"),
        "<!doctype html><title>OpenButler</title>\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("assets")).unwrap();
    std::fs::write(
        dir.join("assets/app-abc123.js"),
        "export const ready = true;\n",
    )
    .unwrap();
}

async fn start(tag: &str) -> openbutler_daemon::server::http::WebServer {
    let root = scratch(tag);
    let assets = root.join("client");
    write_bundle(&assets);
    start_web_server(WebOptions {
        bind: "127.0.0.1:0".to_owned(),
        data_dir: root.join("data"),
        assets: AssetsSource::Explicit(assets),
    })
    .expect("server must start on loopback")
}

/// One keep-alive-free request: `Connection: close` makes the peer end the
/// stream after the response, so reading to EOF is the whole parse.
async fn get(
    addr: std::net::SocketAddr,
    target: &str,
    headers: &[(&str, &str)],
) -> (u16, Vec<(String, String)>, String) {
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .expect("loopback connect");
    let mut request = format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    tokio::io::AsyncWriteExt::write_all(&mut stream, request.as_bytes())
        .await
        .expect("request write");
    let mut raw = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut raw)
        .await
        .expect("response read");
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").expect("head and body");
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .expect("status line")
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .parse::<u16>()
        .expect("numeric status");
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    (status, headers, body.to_owned())
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// Pairs a browser through the link and returns the session cookie value —
/// the credential the API gate wants, which is *not* the link token.
async fn pair(addr: std::net::SocketAddr, pair_token: &str) -> String {
    let (status, headers, _) = get(addr, &format!("/auth/pair?t={pair_token}"), &[]).await;
    assert_eq!(status, 302, "the link pairs a browser");
    let cookie = header(&headers, "set-cookie").expect("a session cookie is set");
    cookie
        .strip_prefix("openbutler_web=")
        .expect("the session cookie name")
        .split(';')
        .next()
        .expect("the cookie value")
        .to_owned()
}

// --- Bundle guard ------------------------------------------------------------

#[test]
fn a_missing_bundle_refuses_to_start_with_the_actionable_message() {
    let empty = scratch("no-bundle");
    let error = start_web_server(WebOptions {
        bind: "127.0.0.1:0".to_owned(),
        data_dir: empty.join("data"),
        assets: AssetsSource::Default {
            start_from: empty.clone(),
        },
    })
    .expect_err("no bundle anywhere in the walk must refuse to start");
    assert_eq!(
        error.to_string(),
        "The browser client bundle (dist/client) is missing. Re-run the installer with --web, or build it with npm run build:web."
    );
    let _ = std::fs::remove_dir_all(&empty);
}

#[test]
fn the_default_lookup_finds_a_bundle_walked_up_from_the_start_directory() {
    let root = scratch("walk");
    write_bundle(&root.join("dist").join("client"));
    let start = root.join("app").join("src");
    std::fs::create_dir_all(&start).unwrap();

    let found = require_default_assets_dir(&start).expect("walk finds dist/client");
    assert_eq!(found, root.join("dist").join("client"));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_build_id_is_the_index_digest_and_tracks_rebuilds() {
    let root = scratch("build-id");
    let assets = root.join("client");
    write_bundle(&assets);

    let first = read_build_id(&assets).expect("a bundle has a build id");
    assert_eq!(first.len(), 12, "twelve hex characters");
    assert!(first.chars().all(|c| c.is_ascii_hexdigit()), "hex only");
    assert_eq!(
        read_build_id(&assets).expect("second read"),
        first,
        "memoized"
    );

    std::fs::write(
        assets.join("index.html"),
        "<!doctype html><title>Rebuilt</title>\n",
    )
    .unwrap();
    let second = read_build_id(&assets).expect("rebuilt bundle has a build id");
    assert_ne!(second, first, "a rebuild changes the index digest");

    let _ = std::fs::remove_dir_all(&root);
}

// --- Pairing link ------------------------------------------------------------

#[tokio::test]
async fn the_pair_link_sets_the_session_cookie_exactly_once() {
    let server = start("pair-link").await;
    let addr = server.addr();

    let (status, headers, _) =
        get(addr, &format!("/auth/pair?t={}", server.pair_token()), &[]).await;
    assert_eq!(status, 302, "the link redirects home");
    assert_eq!(header(&headers, "location"), Some("/"));
    let cookie = header(&headers, "set-cookie").expect("a session cookie is set");
    let session = cookie
        .strip_prefix("openbutler_web=")
        .expect("the session cookie name")
        .split(';')
        .next()
        .expect("the cookie value");
    assert_eq!(
        cookie,
        format!("openbutler_web={session}; HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000"),
        "the cookie mirrors the TypeScript Set-Cookie byte for byte"
    );
    assert_eq!(session.len(), 43, "32 random bytes, base64url");
    assert_ne!(
        session,
        server.pair_token(),
        "the cookie is the session credential, not the link itself"
    );

    let (status, _, body) = get(addr, &format!("/auth/pair?t={}", server.pair_token()), &[]).await;
    assert_eq!(status, 403, "the link is single use");
    assert_eq!(
        body,
        "This pairing link is invalid or has expired. Run `openbutler web` again for a fresh link."
    );

    let (status, _, _) = get(addr, "/auth/pair?t=wrong-token", &[]).await;
    assert_eq!(status, 403, "a wrong token is refused the same way");

    server.close().await;
}

// --- API gate ----------------------------------------------------------------

#[tokio::test]
async fn the_api_gate_checks_origin_then_cookie() {
    let server = start("api-gate").await;
    let addr = server.addr();

    let (status, _, body) = get(addr, "/api/health", &[]).await;
    assert_eq!(status, 401, "an unpaired browser is refused");
    assert_eq!(
        body,
        "{\"error\":\"Pair this browser first: run `openbutler web --pair`.\"}"
    );

    let session = pair(addr, server.pair_token()).await;
    let cookie = format!("openbutler_web={session}");
    let (status, _, body) = get(
        addr,
        "/api/health",
        &[("Origin", "http://evil.example"), ("Cookie", &cookie)],
    )
    .await;
    assert_eq!(status, 403, "a cross-origin request is refused first");
    assert_eq!(body, "{\"error\":\"Cross-origin request refused.\"}");

    let (status, _, body) = get(
        addr,
        "/api/health",
        &[("Origin", "http://127.0.0.1"), ("Cookie", &cookie)],
    )
    .await;
    assert_eq!(status, 200, "same-origin with the cookie is admitted");
    let health: serde_json::Value = serde_json::from_str(&body).expect("health is JSON");
    assert_eq!(health["ok"], true);
    assert!(health["uptimeMs"].as_u64().is_some(), "uptime is reported");

    server.close().await;
}

// --- Static serving ----------------------------------------------------------

#[tokio::test]
async fn static_serving_pins_caching_content_types_and_traversal() {
    let server = start("static").await;
    let addr = server.addr();

    let (status, headers, body) = get(addr, "/", &[]).await;
    assert_eq!(status, 200, "the shell is served");
    assert_eq!(
        header(&headers, "content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(header(&headers, "cache-control"), Some("no-store"));
    assert!(body.contains("OpenButler"), "the index reaches the browser");

    let (status, headers, _) = get(addr, "/assets/app-abc123.js", &[]).await;
    assert_eq!(status, 200, "hashed assets are served");
    assert_eq!(
        header(&headers, "content-type"),
        Some("text/javascript; charset=utf-8")
    );
    assert_eq!(
        header(&headers, "cache-control"),
        Some("public, max-age=31536000, immutable")
    );

    for target in [
        "/../secret.txt",
        "/%2e%2e/secret.txt",
        "/assets/../../secret.txt",
    ] {
        let (status, _, _) = get(addr, target, &[]).await;
        assert_eq!(status, 404, "traversal is refused: {target}");
    }

    let (status, _, body) = get(addr, "/assets/absent.js", &[]).await;
    assert_eq!(status, 404, "a missing asset is a plain 404");
    assert_eq!(body, "Not found.");

    let (status, _, body) = get(addr, "/no/such/page", &[]).await;
    assert_eq!(status, 404, "an unknown page is a plain 404");
    assert_eq!(body, "Not found.");

    server.close().await;
}

/// A request path that names an absolute location must never escape the
/// bundle: `Path::join` with a drive-prefixed or rooted argument discards
/// the base directory on Windows.
#[tokio::test]
async fn an_absolute_looking_path_never_reads_outside_the_bundle() {
    let root = scratch("absolute");
    let assets = root.join("client");
    write_bundle(&assets);
    let secret = root.join("secret.txt");
    std::fs::write(&secret, "outside the bundle").unwrap();
    let server = start_web_server(WebOptions {
        bind: "127.0.0.1:0".to_owned(),
        data_dir: root.join("data"),
        assets: AssetsSource::Explicit(assets),
    })
    .expect("server must start on loopback");

    let absolute = secret.to_string_lossy().replace('\\', "/");
    assert!(!absolute.contains(' '), "the target must be one token");
    for target in [format!("/{absolute}"), format!("//{absolute}")] {
        let (status, _, body) = get(server.addr(), &target, &[]).await;
        assert_eq!(status, 404, "absolute path is refused: {target}");
        assert!(
            !body.contains("outside the bundle"),
            "a file outside the bundle leaked: {target}"
        );
    }

    server.close().await;
}

// --- Persistence --------------------------------------------------------------

#[tokio::test]
async fn the_session_token_survives_a_restart() {
    let root = scratch("persist");
    let assets = root.join("client");
    write_bundle(&assets);

    let options = |data_dir: PathBuf| WebOptions {
        bind: "127.0.0.1:0".to_owned(),
        data_dir,
        assets: AssetsSource::Explicit(assets.clone()),
    };

    let first = start_web_server(options(root.join("data"))).expect("first start");
    let session = pair(first.addr(), first.pair_token()).await;
    let cookie = format!("openbutler_web={session}");
    let (status, _, _) = get(first.addr(), "/api/health", &[("Cookie", &cookie)]).await;
    assert_eq!(status, 200, "the fresh cookie works");
    first.close().await;

    let token_file = root.join("data").join("web-session-token");
    assert!(token_file.exists(), "the token is persisted for restarts");

    let second = start_web_server(options(root.join("data"))).expect("second start");
    let (status, _, _) = get(second.addr(), "/api/health", &[("Cookie", &cookie)]).await;
    assert_eq!(
        status, 200,
        "the cookie from the first run still pairs after a restart"
    );
    second.close().await;

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_pair_link_expires_five_minutes_after_it_is_minted() {
    use openbutler_daemon::server::http::auth::PairToken;

    let at_mint = PairToken::mint(1_000).expect("os entropy");
    assert!(
        at_mint.try_consume(at_mint.token(), 1_000),
        "valid at mint time"
    );

    let inside = PairToken::mint(1_000).expect("os entropy");
    assert!(
        inside.try_consume(inside.token(), 1_000 + 300_000 - 1),
        "valid one millisecond inside the window"
    );

    let at_edge = PairToken::mint(1_000).expect("os entropy");
    assert!(
        !at_edge.try_consume(at_edge.token(), 1_000 + 300_000),
        "expired exactly at the five minute mark"
    );

    let wrong = PairToken::mint(1_000).expect("os entropy");
    assert!(
        !wrong.try_consume("wrong", 1_000),
        "a wrong token fails regardless of the clock, and burns nothing"
    );
    assert!(
        wrong.try_consume(wrong.token(), 1_001),
        "the real one still works"
    );

    let once = PairToken::mint(1_000).expect("os entropy");
    assert!(once.try_consume(once.token(), 1_000), "first use succeeds");
    assert!(
        !once.try_consume(once.token(), 1_001),
        "the link is single use"
    );
}
