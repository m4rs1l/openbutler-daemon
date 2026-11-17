//! The framed socket door: the local pipe and the remote TCP listener.
//!
//! The accept loop binds the endpoint, hands every accepted connection to a
//! task, and stops when the host requests shutdown. Framing, the frame cap,
//! and per-socket backpressure live in the connection module; action routing
//! lives in `dispatch` — this module only moves bytes to and from sockets.

mod connection;
mod endpoint;

pub use endpoint::{Endpoint, ResolvedEndpoint, endpoint_for};

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::dispatch::Host;

/// Serves the IPC endpoint until the host requests shutdown.
///
/// A bind failure (endpoint already taken) is returned to the caller as an
/// I/O error; a malformed frame or a breached frame cap closes only that
/// connection, never the server.
pub async fn serve(
    endpoint: ResolvedEndpoint,
    host: Arc<Host>,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let data_dir = endpoint.data_dir().to_path_buf();

    // The platforms wait for a client differently, so the wait is split
    // here: Windows creates a fresh pipe instance per wait (the connected
    // instance is handed to a task, and the next client needs a new one),
    // while Unix keeps a single listener alive across accepts so a
    // connection being served never hides the next one. The spawn and the
    // shutdown branch are identical on both sides.
    #[cfg(windows)]
    {
        loop {
            let server = bind(&endpoint)?;
            // `NamedPipeServer::connect` is the accept-equivalent: it waits
            // for a client. The future owns the instance so the branch can
            // hand the connected stream to a task.
            let wait_for_client = async move { server.connect().await.map(|()| server) };
            tokio::select! {
                changed = shutdown.changed() => {
                    // Either the host asked to stop, or the sender is gone.
                    let _ = changed;
                    return Ok(());
                }
                connected = wait_for_client => {
                    let stream = connected?;
                    let host = host.clone();
                    let data_dir = data_dir.clone();
                    tokio::spawn(async move {
                        connection::run(stream, host, data_dir, connection::Admission::Local).await;
                    });
                }
            }
        }
    }

    #[cfg(unix)]
    {
        let listener = bind(&endpoint)?;
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    let _ = changed;
                    return Ok(());
                }
                accepted = listener.accept() => {
                    let (stream, _peer) = accepted?;
                    let host = host.clone();
                    let data_dir = data_dir.clone();
                    tokio::spawn(async move {
                        connection::run(stream, host, data_dir, connection::Admission::Local).await;
                    });
                }
            }
        }
    }
}

/// Binds a TCP listener and serves it behind the admission gate until the
/// host requests shutdown. Returns the bound address (pass `:0` to pick a
/// free port). Every accepted socket is remote: its first frame must pass
/// the `auth` / `pair` gate or the connection is closed.
pub async fn serve_tcp(
    addr: &str,
    host: Arc<Host>,
    data_dir: PathBuf,
    auth_timeout: Duration,
) -> std::io::Result<std::net::SocketAddr> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    let mut shutdown = host.subscribe_shutdown();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    // Either the host asked to stop, or the sender is gone.
                    let _ = changed;
                    break;
                }
                accepted = listener.accept() => {
                    let Ok((stream, _peer)) = accepted else { break; };
                    let host = host.clone();
                    let data_dir = data_dir.clone();
                    tokio::spawn(async move {
                        connection::run(
                            stream,
                            host,
                            data_dir,
                            connection::Admission::Remote { auth_timeout },
                        )
                        .await;
                    });
                }
            }
        }
    });
    Ok(local)
}

/// Binds one listener instance for the endpoint (platform transport lives
/// in this function only).
#[cfg(windows)]
fn bind(
    endpoint: &ResolvedEndpoint,
) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    tokio::net::windows::named_pipe::ServerOptions::new()
        .first_pipe_instance(false)
        .create(endpoint.socket_name())
}

#[cfg(unix)]
fn bind(endpoint: &ResolvedEndpoint) -> std::io::Result<tokio::net::UnixListener> {
    // A stale socket file from a crashed run must not block the next start.
    let _ = std::fs::remove_file(endpoint.socket_name());
    tokio::net::UnixListener::bind(endpoint.socket_name())
}
