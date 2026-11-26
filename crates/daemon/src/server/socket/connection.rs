//! One accepted connection: decode frames in, dispatch, write frames out.
//!
//! The write path is a bounded queue drained by its own task, so a slow or
//! absent reader can never grow host memory (the contract: queued
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
    /// Local pipe/socket: no gate, exactly as a local client.
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

/// Tracks this connection's roster entry so a host notification never
/// reaches a closed socket (the source removed a socket on close).
struct RosterGuard {
    host: Arc<Host>,
    id: u64,
}

impl Drop for RosterGuard {
    fn drop(&mut self) {
        self.host.clients().unregister(self.id);
    }
}

/// Sends frames to the writer task, enforcing the byte budget first.
#[derive(Clone)]
struct Outbound {
    tx: mpsc::Sender<(Vec<u8>, Option<tokio::sync::oneshot::Sender<()>>)>,
    queued: Arc<AtomicUsize>,
}

impl Outbound {
    async fn send(&self, frame: Vec<u8>) -> Result<(), ()> {
        let length = frame.len();
        self.open_budget(length)?;
        if self.tx.send((frame, None)).await.is_err() {
            self.queued.fetch_sub(length, Ordering::SeqCst);
            return Err(());
        }
        Ok(())
    }

    /// Queues the frame and waits until the writer task has written it:
    /// the response that triggers a shutdown must be on the wire first
    /// (the source's `writeResponse` before `setImmediate(requestStop)`).
    async fn send_confirmed(&self, frame: Vec<u8>) -> Result<(), ()> {
        let length = frame.len();
        self.open_budget(length)?;
        let (ack, wait) = tokio::sync::oneshot::channel();
        if self.tx.send((frame, Some(ack))).await.is_err() {
            self.queued.fetch_sub(length, Ordering::SeqCst);
            return Err(());
        }
        wait.await.map_err(|_| ())
    }

    fn open_budget(&self, length: usize) -> Result<(), ()> {
        let previous = self.queued.fetch_add(length, Ordering::SeqCst);
        if previous + length > MAX_QUEUED_OUTPUT_BYTES {
            self.queued.fetch_sub(length, Ordering::SeqCst);
            return Err(());
        }
        Ok(())
    }
}

/// The fan-out seam: the same bounded queue the response path uses,
/// without the await (a publish must never block on a writer task).
impl crate::dispatch::fanout::EventSink for Outbound {
    fn try_send(&self, frame: Vec<u8>) -> bool {
        let length = frame.len();
        let previous = self.queued.fetch_add(length, Ordering::SeqCst);
        if previous + length > MAX_QUEUED_OUTPUT_BYTES {
            self.queued.fetch_sub(length, Ordering::SeqCst);
            return false;
        }
        if self.tx.try_send((frame, None)).is_err() {
            self.queued.fetch_sub(length, Ordering::SeqCst);
            return false;
        }
        true
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
    let (tx, mut rx) = mpsc::channel::<(Vec<u8>, Option<tokio::sync::oneshot::Sender<()>>)>(64);
    let queued = Arc::new(AtomicUsize::new(0));

    let write_budget = queued.clone();
    let writer_task = tokio::spawn(async move {
        while let Some((frame, ack)) = rx.recv().await {
            if writer.write_all(&frame).await.is_err() {
                break;
            }
            write_budget.fetch_sub(frame.len(), Ordering::SeqCst);
            if let Some(ack) = ack {
                let _ = ack.send(());
            }
        }
        let _ = writer.shutdown().await;
    });

    let outbound = Outbound { tx, queued };
    // One sink shared by dispatch and the host roster: the stop broadcast
    // skips the requester by pointer identity, so both must be the same Arc.
    let sink: crate::dispatch::fanout::SharedSink = Arc::new(outbound.clone());
    let roster_id = host.clients().register(sink.clone());
    let roster = RosterGuard {
        host: host.clone(),
        id: roster_id,
    };

    let mut decoder = FrameDecoder::new(MAX_DAEMON_FRAME_BYTES);
    let mut buffer = vec![0u8; 16 * 1024];
    let mut pending: VecDeque<Vec<u8>> = VecDeque::new();
    // The chat this connection carries between requests (`session.load`).
    let mut dispatch_state = dispatch::DispatchState::new();
    // Every frame this connection may publish (`attach` subscribes it).
    dispatch_state.set_sink(sink);

    if let Admission::Remote { auth_timeout } = admission {
        dispatch_state.set_remote(true);
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
            return {
                drop(dispatch_state);
                // The roster's sink clone keeps the writer's channel open:
                // it must leave before the join or the join never returns.
                drop(roster);
                drop_connection(outbound, writer_task).await
            };
        }
    }

    while let Some(frame) = next_frame(&mut reader, &mut decoder, &mut buffer, &mut pending).await {
        let Ok(request) = serde_json::from_slice::<openbutler_protocol::DaemonRequest>(&frame)
        else {
            return {
                drop(dispatch_state);
                drop(roster);
                drop_connection(outbound, writer_task).await
            };
        };
        let response = dispatch::handle(&host, &data_dir, &mut dispatch_state, &request);
        let Ok(mut bytes) = serde_json::to_vec(&response) else {
            return {
                drop(dispatch_state);
                drop(roster);
                drop_connection(outbound, writer_task).await
            };
        };
        bytes.push(b'\n');
        // The response is confirmed written before anything it triggers
        // happens: a stop's shutdown must never beat its own ack (the
        // source wrote the response, then asked for the stop next tick).
        if outbound.send_confirmed(bytes).await.is_err() {
            return {
                drop(dispatch_state);
                drop(roster);
                drop_connection(outbound, writer_task).await
            };
        }
        // Events an action queued behind its answer (the attach snapshot
        // event) go out only after the response is on the wire.
        for frame in dispatch_state.take_pending_events() {
            if outbound.send(frame).await.is_err() {
                return {
                    drop(dispatch_state);
                    drop(roster);
                    drop_connection(outbound, writer_task).await
                };
            }
        }
        if outbound.tx.is_closed() {
            break; // writer task ended: the peer is gone
        }
        // A stop/restart asked for the end only after its ack went out.
        if dispatch_state.take_shutdown_request() {
            host.request_shutdown();
            break;
        }
    }
    drop(dispatch_state);
    drop(roster);
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

/// The admission verdict for one remote opening frame: answer with the fixed
/// wording, and report whether dispatch may run.
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
