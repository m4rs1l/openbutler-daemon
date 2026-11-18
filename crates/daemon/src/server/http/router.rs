//! The HTTP server: router, handlers, lifecycle.
//!
//! Route order: the pairing link first (it is
//! the only unauthenticated browser route), then the gated `/api/*` tree,
//! then static serving, then a plain 404. Session-backed routes arrive
//! with the runtime slice; nothing here invents data the host owns.

use std::net::{IpAddr, SocketAddr, TcpListener as StdTcpListener};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Request, State};
use axum::http::{StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use super::assets::{AssetsSource, resolve_static};
use super::auth::{self, PairToken, persistent_session_token};

/// The default bind: loopback only, the port every installer and
/// docstring already names.
pub const DEFAULT_WEB_BIND: &str = "127.0.0.1:45278";

const PAIR_REFUSED: &str =
    "This pairing link is invalid or has expired. Run `openbutler web` again for a fresh link.";
const API_UNPAIRED: &str = "Pair this browser first: run `openbutler web --pair`.";
const CROSS_ORIGIN: &str = "Cross-origin request refused.";

/// What [`start_web_server`] needs to come up.
pub struct WebOptions {
    /// `ip:port` to bind, e.g. `127.0.0.1:45278` or `0.0.0.0:45278`.
    pub bind: String,
    /// Where the persistent session token lives.
    pub data_dir: PathBuf,
    /// Where the bundle comes from.
    pub assets: AssetsSource,
}

#[derive(Clone)]
struct AppState {
    pair: Arc<PairToken>,
    session_token: Arc<str>,
    assets_dir: Arc<PathBuf>,
    started: Instant,
}

/// A running web server. Dropping it leaves the task running;
/// [`WebServer::close`] stops it.
pub struct WebServer {
    addr: SocketAddr,
    pair: Arc<PairToken>,
    task: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for WebServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebServer")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

impl WebServer {
    /// The bound address the web server listens on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The pairing token the gate accepts.
    pub fn pair_token(&self) -> &str {
        self.pair.token()
    }

    /// The one-time link the owner opens in a browser: loopback or
    /// unspecified binds render as addresses this machine's browser can
    /// reach, IPv6 bracketed.
    pub fn pair_url(&self) -> String {
        let host = match self.addr.ip() {
            IpAddr::V4(ip) if ip.is_unspecified() => "127.0.0.1".to_owned(),
            IpAddr::V6(ip) if ip.is_unspecified() => "::1".to_owned(),
            IpAddr::V6(ip) => format!("[{ip}]"),
            ip => ip.to_string(),
        };
        format!(
            "http://{host}:{}/auth/pair?t={}",
            self.addr.port(),
            self.pair_token()
        )
    }

    /// Stops the listener and waits for the task to finish.
    pub async fn close(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

/// Starts the web server. The bundle guard runs before any port opens, so
/// a base install refuses to start rather than serving an empty shell.
/// The success path must be called from inside a Tokio runtime.
pub fn start_web_server(options: WebOptions) -> std::io::Result<WebServer> {
    let assets_dir = match options.assets {
        AssetsSource::Default { start_from } => {
            super::assets::require_default_assets_dir(&start_from)?
        }
        AssetsSource::Explicit(dir) => dir,
    };
    let session_token: Arc<str> = persistent_session_token(&options.data_dir)?.into();
    let pair = Arc::new(PairToken::mint(crate::auth::now_ms())?);

    // Bind synchronously: the guard above has already refused a broken
    // bundle before any port exists, and a sync bind keeps the refusal
    // callable from a plain test.
    let listener = StdTcpListener::bind(&options.bind)?;
    listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(listener)?;
    let addr = listener.local_addr()?;

    let state = AppState {
        pair: pair.clone(),
        session_token: session_token.clone(),
        assets_dir: Arc::new(assets_dir),
        started: Instant::now(),
    };
    let app = Router::new()
        .route("/auth/pair", get(pair_link))
        .nest(
            "/api",
            Router::new()
                .route("/health", get(health))
                .layer(middleware::from_fn_with_state(state.clone(), api_gate)),
        )
        .fallback(static_file)
        .with_state(state);

    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(WebServer { addr, pair, task })
}

/// `GET /auth/pair?t=…`: the one exchange that pairs a browser.
async fn pair_link(State(state): State<AppState>, uri: Uri) -> Response {
    // The token is base64url, so the raw query value is byte-identical to
    // what `encodeURIComponent` produced — no decode to get wrong.
    let supplied = uri
        .query()
        .and_then(|query| {
            query.split('&').find_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                (key == "t").then_some(value)
            })
        })
        .unwrap_or("");

    if state.pair.try_consume(supplied, crate::auth::now_ms()) {
        let cookie = format!(
            "{}={}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
            auth::SESSION_COOKIE,
            state.session_token,
            auth::SESSION_TTL_SECONDS
        );
        return (
            StatusCode::FOUND,
            [
                (header::SET_COOKIE, cookie),
                (header::LOCATION, "/".to_owned()),
            ],
            (),
        )
            .into_response();
    }
    (
        StatusCode::FORBIDDEN,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        PAIR_REFUSED,
    )
        .into_response()
}

/// The gate in front of every `/api/*` route: origin first, then cookie.
/// The refusal carries its reason, because "stale pairing" and "proxy
/// dropped the header" are different reports.
async fn api_gate(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok());
    if !auth::origin_allowed(origin, host) {
        return json_error(StatusCode::FORBIDDEN, CROSS_ORIGIN);
    }

    let paired = request
        .headers()
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(auth::session_cookie)
        .is_some_and(|token| crate::auth::ct_eq(token.as_bytes(), state.session_token.as_bytes()));
    if !paired {
        return json_error(StatusCode::UNAUTHORIZED, API_UNPAIRED);
    }
    next.run(request).await
}

async fn health(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({
        "ok": true,
        "clients": 0,
        "uptimeMs": state.started.elapsed().as_millis() as u64,
    }))
}

/// Static serving with the caching headers; anything else is a plain 404 —
/// this server carries no page of its own.
async fn static_file(State(state): State<AppState>, request: Request) -> Response {
    let method_allows_static =
        request.method() == axum::http::Method::GET || request.method() == axum::http::Method::HEAD;
    if !method_allows_static {
        return not_found();
    }
    match resolve_static(&state.assets_dir, request.uri().path()) {
        Some(file) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, file.content_type),
                (header::CACHE_CONTROL, file.cache_control),
            ],
            file.body,
        )
            .into_response(),
        None => not_found(),
    }
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        "Not found.",
    )
        .into_response()
}

fn json_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({ "error": message })),
    )
        .into_response()
}
