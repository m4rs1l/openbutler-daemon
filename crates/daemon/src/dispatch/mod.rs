//! Action routing and the host record.
//!
//! `Host` is the authority for what `status` reports (identity, start time,
//! live connection count, shutdown signal). `handle` is the single dispatch
//! point: every action is routed here, validated against the envelope rules,
//! and answered as one protocol response — no other module constructs
//! responses.

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use openbutler_protocol::{
    DAEMON_PROTOCOL_VERSION, DaemonRequest, DaemonResponse, DaemonStatus, validate_request,
};
use serde_json::json;
use tokio::sync::watch;

use crate::state::{self, WorkspaceFilter};

/// What the daemon reports about itself and what keeps it alive.
pub struct Host {
    started_at: String,
    connected_clients: AtomicU32,
    shutdown: watch::Sender<bool>,
}

impl Host {
    /// Identity and clock start now; the shutdown channel opens closed.
    pub fn new() -> Self {
        let started_at = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned());
        let (shutdown, _) = watch::channel(false);
        Self {
            started_at,
            connected_clients: AtomicU32::new(0),
            shutdown,
        }
    }

    /// The current status view (`status` action).
    pub fn status(&self) -> DaemonStatus {
        DaemonStatus {
            pid: std::process::id(),
            started_at: self.started_at.clone(),
            // Workspace runtimes are not implemented yet: an honest zero,
            // not a fabricated count.
            workspace_count: 0,
            connected_clients: self.connected_clients.load(Ordering::SeqCst),
            schedules: None,
            telegram: None,
            legacy: None,
            unresponsive: None,
            entry: None,
            // A source run reports an explicit null (contract: null = source
            // run, absent = older host).
            build_id: Some(None),
        }
    }

    /// Live IPC connections are counted by RAII guards in the server.
    pub(crate) fn client_enter(&self) {
        self.connected_clients.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn client_exit(&self) {
        self.connected_clients.fetch_sub(1, Ordering::SeqCst);
    }

    /// Subscribe to the shutdown signal (serve loop + tests).
    pub fn subscribe_shutdown(&self) -> watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    /// Ask the serve loop to stop; idempotent.
    pub fn request_shutdown(&self) {
        let _ = self.shutdown.send(true);
    }
}

impl Default for Host {
    fn default() -> Self {
        Self::new()
    }
}

/// The one dispatch point: validate the envelope, route the action, answer.
pub fn handle(host: &Host, data_dir: &Path, request: &DaemonRequest) -> DaemonResponse {
    if let Err(error) = validate_request(request) {
        return failure(request, error.to_string());
    }
    match request.action.as_str() {
        "status" => success(
            request,
            serde_json::to_value(host.status()).unwrap_or_else(|_| json!({})),
        ),
        "session.list" => match state::list_sessions(data_dir, &WorkspaceFilter::Any) {
            Ok(sessions) => success(
                request,
                serde_json::to_value(sessions).unwrap_or_else(|_| json!([])),
            ),
            Err(error) => failure(request, error.to_string()),
        },
        "stop" => {
            host.request_shutdown();
            success(request, json!({ "stopping": true }))
        }
        other => failure(request, format!("Unknown action: {other}")),
    }
}

/// Success envelope at the current protocol version, echoing the request id.
fn success(request: &DaemonRequest, result: serde_json::Value) -> DaemonResponse {
    DaemonResponse::Success {
        version: DAEMON_PROTOCOL_VERSION,
        id: request.id.clone(),
        ok: true,
        result,
    }
}

/// Failure envelope at the current protocol version, echoing the request id.
fn failure(request: &DaemonRequest, error: String) -> DaemonResponse {
    DaemonResponse::Failure {
        version: DAEMON_PROTOCOL_VERSION,
        id: request.id.clone(),
        ok: false,
        error,
    }
}
