//! One accepted connection: decode frames in, dispatch, write frames out.
//!
//! The write path is a bounded queue drained by its own task, so a slow or
//! absent reader can never grow host memory (the legacy contract: queued
//! bytes beyond the frame cap disconnect the socket). Read-side failures —
//! malformed JSON, a breached frame cap, EOF — close this connection only.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use openbutler_protocol::{FrameDecoder, MAX_DAEMON_FRAME_BYTES};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::dispatch::{self, Host};

/// Per-socket outbound budget, mirroring `MAX_QUEUED_OUTPUT_BYTES`.
const MAX_QUEUED_OUTPUT_BYTES: usize = 32 * 1024 * 1024;

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

/// The platform stream differs; both split into a reader and a writer.
/// On Windows the accepted `NamedPipeServer` instance *is* the duplex stream.
#[cfg(windows)]
type Stream = tokio::net::windows::named_pipe::NamedPipeServer;
#[cfg(unix)]
type Stream = tokio::net::UnixStream;

pub async fn run(stream: Stream, host: Arc<Host>, data_dir: PathBuf) {
    let _guard = ClientGuard::enter(&host);
    serve_connection(stream, host, data_dir).await;
}

async fn serve_connection(stream: Stream, host: Arc<Host>, data_dir: PathBuf) {
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
    while let Ok(read) = reader.read(&mut buffer).await {
        if read == 0 {
            break; // peer closed
        }
        let Ok(frames) = decoder.append(&buffer[..read]) else {
            break; // frame cap breached: destroy the connection
        };
        for frame in frames {
            let Ok(request) = serde_json::from_slice::<openbutler_protocol::DaemonRequest>(&frame)
            else {
                return drop_connection(outbound, writer_task).await;
            };
            let response = dispatch::handle(&host, &data_dir, &request);
            let Ok(mut bytes) = serde_json::to_vec(&response) else {
                return drop_connection(outbound, writer_task).await;
            };
            bytes.push(b'\n');
            if outbound.send(bytes).await.is_err() {
                return drop_connection(outbound, writer_task).await;
            }
        }
        if outbound.tx.is_closed() {
            break; // writer task ended: the peer is gone
        }
    }
    drop_connection(outbound, writer_task).await;
}

/// Tears the write side down and waits for the writer to finish.
async fn drop_connection(outbound: Outbound, writer_task: tokio::task::JoinHandle<()>) {
    drop(outbound);
    let _ = writer_task.await;
}
