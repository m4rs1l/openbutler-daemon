//! One accepted connection: decode frames in, dispatch, write frames out.
//!
//! The write path is a bounded queue drained by its own task, so a slow or
//! absent reader can never grow host memory (the legacy contract: queued
//! bytes beyond the frame cap disconnect the socket). Read-side failures —
//! malformed JSON, a breached frame cap, EOF — close this connection only.
//!
//! A remote connection passes the admission gate first: its opening frame
//! must be `auth` or `pair` (`openbutler_protocol::auth` owns the rules and
//! the wording), answered before dispatch ever runs. A refused, malformed,
//! or silent socket is closed — fail closed, so an open port never holds an
//! unauthenticated connection alive.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use openbutler_protocol::auth::{self as auth_plane, AuthSuccess, FirstFrame, PairSuccess};
use openbutler_protocol::{DaemonResponse, FrameDecoder, MAX_DAEMON_FRAME_BYTES};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::auth::{TokenStore, now_ms};
use crate::dispatch::{self, Host};

/// Per-socket outbound budget, mirroring `MAX_QUEUED_OUTPUT_BYTES`.
const MAX_QUEUED_OUTPUT_BYTES: usize = 32 * 1024 * 1024;

/// How a connection earns the right to reach dispatch.
pub(crate) enum Admission {
    /// Local pipe/socket: no gate, exactly the legacy behavior.
    Local,
    /// Remote TCP: the first frame must pass the admission plane within the
    /// deadline; silence closes the connection.
    Remote { auth_timeout: std::time::Duration },
}

/// Tracks this connection's share of `connectedClients`.
struct ClientGuard(Arc<Host>);

impl ClientGuard {
    fn enter(host: &Arc<Host>) -> Self {
        host.client_enter();
        Self(host.clone())
    }
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.client_exit();
    }
}

/// Sends frames to the writer task, enforcing the byte budget first.
struct Outbound {
    tx: mpsc::Sender<Vec<u8>>,
    queued: Arc<AtomicUsize>,
}

impl Outbound {
    async fn send(&self, frame: Vec<u8>) -> Result<(), ()> {
        let length = frame.len();
        let previous = self.queued.fetch_add(length, Ordering::SeqCst);
        if previous + length > MAX_QUEUED_OUTPUT_BYTES {
            self.queued.fetch_sub(length, Ordering::SeqCst);
            return Err(());
        }
        if self.tx.send(frame).await.is_err() {
            self.queued.fetch_sub(length, Ordering::SeqCst);
            return Err(());
        }
        Ok(())
    }
}

pub async fn run<S>(stream: S, host: Arc<Host>, data_dir: PathBuf, admission: Admission)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let _guard = ClientGuard::enter(&host);
    serve_connection(stream, host, data_dir, admission).await;
}

async fn serve_connection<S>(stream: S, host: Arc<Host>, data_dir: PathBuf, admission: Admission)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
    let queued = Arc::new(AtomicUsize::new(0));

    let write_budget = queued.clone();
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if writer.write_all(&frame).await.is_err() {
                break;
            }
            write_budget.fetch_sub(frame.len(), Ordering::SeqCst);
        }
        let _ = writer.shutdown().await;
    });

    let outbound = Outbound { tx, queued };

    let mut decoder = FrameDecoder::new(MAX_DAEMON_FRAME_BYTES);
    let mut buffer = vec![0u8; 16 * 1024];
    let mut pending: VecDeque<Vec<u8>> = VecDeque::new();
    // The chat this connection carries between requests (`session.load`).
    let mut dispatch_state = dispatch::DispatchState::new();

    if let Admission::Remote { auth_timeout } = admission {
        let first = tokio::time::timeout(
            auth_timeout,
            next_frame(&mut reader, &mut decoder, &mut buffer, &mut pending),
        )
        .await;
        let admitted = match first {
            Ok(Some(frame)) => admit(&host, &data_dir, &frame, &outbound).await,
            // EOF, a breached cap, or the deadline: close without a word.
            Ok(None) | Err(_) => false,
        };
        if !admitted {
            return drop_connection(outbound, writer_task).await;
        }
    }

    while let Some(frame) = next_frame(&mut reader, &mut decoder, &mut buffer, &mut pending).await {
        let Ok(request) = serde_json::from_slice::<openbutler_protocol::DaemonRequest>(&frame)
        else {
            return drop_connection(outbound, writer_task).await;
        };
        let response = dispatch::handle(&host, &data_dir, &mut dispatch_state, &request);
        let Ok(mut bytes) = serde_json::to_vec(&response) else {
            return drop_connection(outbound, writer_task).await;
        };
        bytes.push(b'\n');
        if outbound.send(bytes).await.is_err() {
            return drop_connection(outbound, writer_task).await;
        }
        if outbound.tx.is_closed() {
            break; // writer task ended: the peer is gone
        }
    }
    drop_connection(outbound, writer_task).await;
}

/// Yields the next complete frame, reading until the decoder has one.
/// `None` means EOF or a breached frame cap — the caller closes.
async fn next_frame<R>(
    reader: &mut R,
    decoder: &mut FrameDecoder,
    buffer: &mut [u8],
    pending: &mut VecDeque<Vec<u8>>,
) -> Option<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    loop {
        if let Some(frame) = pending.pop_front() {
            return Some(frame);
        }
        let read = reader.read(buffer).await.ok()?;
        if read == 0 {
            return None;
        }
        let frames = decoder.append(&buffer[..read]).ok()?;
        pending.extend(frames);
    }
}

/// The admission verdict for one remote opening frame: answer exactly as the
/// legacy host does, and report whether dispatch may run.
async fn admit(host: &Host, data_dir: &Path, frame: &[u8], outbound: &Outbound) -> bool {
    let value: Value = serde_json::from_slice(frame).unwrap_or(Value::Null);

    match auth_plane::classify_first_frame(&value) {
        FirstFrame::Auth { id, token } => {
            // An unreadable store verifies as `false`: fail closed.
            let admitted = TokenStore::new(data_dir).verify(token).unwrap_or(false);
            if admitted {
                let result = serde_json::to_value(AuthSuccess {
                    authenticated: true,
                })
                .unwrap_or(Value::Null);
                reply(outbound, dispatch::envelope_success(id, result))
                    .await
                    .is_ok()
            } else {
                let _ = reply(
                    outbound,
                    dispatch::envelope_failure(id, auth_plane::AUTH_FAILED),
                )
                .await;
                false
            }
        }
        FirstFrame::Pair { id, code } => {
            if !host.pairing().redeem(code, now_ms()) {
                let _ = reply(
                    outbound,
                    dispatch::envelope_failure(id, auth_plane::PAIR_FAILED),
                )
                .await;
                return false;
            }
            match TokenStore::new(data_dir).mint() {
                Ok(minted) => {
                    let result = serde_json::to_value(PairSuccess {
                        token: minted.token,
                    })
                    .unwrap_or(Value::Null);
                    reply(outbound, dispatch::envelope_success(id, result))
                        .await
                        .is_ok()
                }
                Err(error) => {
                    let _ =
                        reply(outbound, dispatch::envelope_failure(id, &error.to_string())).await;
                    false
                }
            }
        }
        FirstFrame::Refused { id } => {
            let _ = reply(
                outbound,
                dispatch::envelope_failure(id, auth_plane::REMOTE_FIRST_FRAME_REFUSED),
            )
            .await;
            false
        }
    }
}

/// Serialises one response frame and queues it for the writer task.
async fn reply(outbound: &Outbound, response: DaemonResponse) -> Result<(), ()> {
    let mut bytes = serde_json::to_vec(&response).map_err(|_| ())?;
    bytes.push(b'\n');
    outbound.send(bytes).await
}

/// Tears the write side down and waits for the writer to finish.
async fn drop_connection(outbound: Outbound, writer_task: tokio::task::JoinHandle<()>) {
    drop(outbound);
    let _ = writer_task.await;
}
