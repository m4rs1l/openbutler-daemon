//! The sidecar door: one stateless Node process, many calls.
//!
//! Layout: `codec` owns line framing and the handshake, `process` owns
//! spawning, `queue` owns lanes and admission, `route` owns event
//! routing and call termination, `supervisor` owns the state machine
//! and timers. Contract: `docs/sidecar-door.md`; wire types:
//! `openbutler_protocol::sidecar`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use openbutler_protocol::sidecar::{
    SIDECAR_PROTOCOL_VERSION, SidecarCall, SidecarCommand, SidecarEvent, SidecarReady,
};
use serde_json::json;
use tokio::sync::{Mutex, mpsc};

mod codec;
mod process;
mod queue;
mod route;
mod supervisor;

use queue::Entry;
use route::{Control, Route};

pub use supervisor::DoorState;

/// Which lane a call belongs to: interactive work jumps queued
/// background work, and background work can never hold every slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lane {
    Background,
    Interactive,
}

/// Everything the door needs to exist. Testable timings; production
/// values come from the daemon's configuration when that lands.
#[derive(Debug, Clone)]
pub struct DoorConfig {
    /// The Node executable (`node` next to the packaged runtime in
    /// production).
    pub node_program: PathBuf,
    /// Extra node flags before the script path — the production heap
    /// belt (`--max-old-space-size=64`) lives here so every measurement
    /// runs the spawn profile production will use.
    pub node_args: Vec<String>,
    /// The sidecar entry script.
    pub script: PathBuf,
    /// The fake-sidecar behavior selector (test only; empty in
    /// production).
    pub mode: String,
    /// Providers the fake sidecar reports (test only; empty in
    /// production).
    pub providers: Option<Vec<String>>,
    /// Receipt log for the fake sidecar (test only).
    pub log_file: Option<PathBuf>,
    /// Extra environment for the child, on top of the minimal base.
    pub env: Vec<(String, String)>,
    pub spawn_timeout: Duration,
    pub idle_kill: Duration,
    pub drain_max: Duration,
    pub queue_max_wait: Duration,
    pub first_event_timeout: Duration,
    pub inter_event_timeout: Duration,
    pub concurrency: usize,
    pub line_cap: usize,
}

impl DoorConfig {
    fn child_env(&self) -> Vec<(String, String)> {
        let mut env = self.env.clone();
        if !self.mode.is_empty() {
            env.push(("FAKE_MODE".to_owned(), self.mode.clone()));
        }
        if let Some(providers) = &self.providers {
            env.push((
                "FAKE_PROVIDERS".to_owned(),
                serde_json::to_string(providers).unwrap_or_else(|_| "[]".to_owned()),
            ));
        }
        if let Some(log) = &self.log_file {
            env.push(("FAKE_LOG".to_owned(), log.display().to_string()));
        }
        env
    }
}

/// Door-level refusals. The message is the contract where it surfaces
/// to callers.
#[derive(Debug, Clone, thiserror::Error)]
pub enum DoorError {
    #[error("the sidecar handshake never arrived within {0:?}")]
    HandshakeTimeout(Duration),
    #[error("sidecar protocol version {got} does not match host version {expected}")]
    ProtocolVersionMismatch { expected: u32, got: u32 },
    #[error("provider `{0}` was not in the handshake allowlist")]
    ProviderNotAllowed(String),
    #[error("sidecar protocol violation: {0}")]
    ProtocolViolation(String),
    #[error("the call waited in the queue longer than allowed")]
    QueueTimeout,
    #[error("no sidecar event arrived within the first-event window")]
    FirstEventTimeout,
    #[error("the sidecar went quiet between events")]
    InterEventTimeout,
    #[error("the call was cancelled")]
    Cancelled,
    #[error("the sidecar process exited")]
    ProcessExited,
    #[error("sidecar spawn failed: {0}")]
    SpawnFailed(String),
    #[error("sidecar i/o: {0}")]
    Io(String),
}

/// The shared interior of the door.
pub(crate) struct DoorInner {
    pub(crate) config: DoorConfig,
    pub(crate) ready: SidecarReady,
    pub(crate) state: tokio::sync::watch::Sender<DoorState>,
    pub(crate) writes: Mutex<process::ChildWriter>,
    /// call id → its route; whoever removes a route delivers its
    /// terminal outcome, and late events after cancel find nothing.
    pub(crate) routes: Mutex<HashMap<String, Route>>,
    pub(crate) inflight: AtomicUsize,
    pub(crate) background_inflight: AtomicUsize,
    pub(crate) queue: queue::Queue,
    pub(crate) last_used_ms: AtomicU64,
    /// Wakes the single admission loop: new work, a freed slot, or any
    /// other event that could let a queued call go out.
    pub(crate) admit: tokio::sync::Notify,
}

/// The door handle: cheap to clone, one per caller.
#[derive(Clone)]
pub struct SidecarDoor {
    pub(crate) inner: Arc<DoorInner>,
}

impl std::fmt::Debug for SidecarDoor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SidecarDoor")
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

/// One in-flight call: events until the stream ends (or a terminal
/// door error).
pub struct CallStream {
    rx: mpsc::Receiver<Result<SidecarEvent, DoorError>>,
    door: SidecarDoor,
    id: String,
    cancelled: bool,
    cancel_reported: bool,
}

impl std::fmt::Debug for CallStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallStream")
            .field("id", &self.id)
            .field("cancelled", &self.cancelled)
            .finish_non_exhaustive()
    }
}

impl SidecarDoor {
    /// Spawn the process and wait for the handshake.
    pub async fn spawn(config: DoorConfig) -> Result<(Self, SidecarReady), DoorError> {
        let (mut writer, stdout) = process::spawn(&config).await?;
        let line_cap = config.line_cap;
        let (events_tx, mut events_rx) = mpsc::channel::<Result<SidecarEvent, DoorError>>(64);
        tokio::spawn(codec::read_stdout(stdout, events_tx, line_cap));

        let ready = match codec::handshake(&mut events_rx, &config).await {
            Ok(ready) => ready,
            Err(error) => {
                let _ = writer.kill().await;
                let _ = writer.wait().await;
                return Err(error);
            }
        };
        if ready.protocol_version != SIDECAR_PROTOCOL_VERSION {
            let _ = writer.kill().await;
            let _ = writer.wait().await;
            return Err(DoorError::ProtocolVersionMismatch {
                expected: SIDECAR_PROTOCOL_VERSION,
                got: ready.protocol_version,
            });
        }

        let (state, state_rx) = tokio::sync::watch::channel(DoorState::Ready);
        let announce = ready.clone();
        let inner = Arc::new(DoorInner {
            config,
            ready,
            state,
            writes: Mutex::new(writer),
            routes: Mutex::new(HashMap::new()),
            inflight: AtomicUsize::new(0),
            background_inflight: AtomicUsize::new(0),
            queue: queue::Queue::new(),
            last_used_ms: AtomicU64::new(now_ms()),
            admit: tokio::sync::Notify::new(),
        });
        let door = Self { inner };

        // The router: stdout lines → per-call channels.
        let router = door.clone();
        tokio::spawn(async move {
            loop {
                match events_rx.recv().await {
                    None => {
                        router.on_child_eof().await;
                        break;
                    }
                    Some(Ok(event)) => {
                        if matches!(event, SidecarEvent::Ready(_)) {
                            continue; // only the handshake consumes ready
                        }
                        router.route_event(event).await;
                    }
                    Some(Err(violation)) => {
                        router.on_protocol_violation(violation).await;
                        break;
                    }
                }
            }
        });

        // The supervisor: idle, draining, force-kill.
        let watch = door.clone();
        tokio::spawn(supervisor::run(watch, state_rx));

        // The admission loop: the single owner of "may this queued call
        // go out now". Everything else only notifies it, so admission is
        // serialized without holding locks across tasks.
        let admitter = door.clone();
        let mut admit_state = door.inner.state.subscribe();
        tokio::spawn(async move {
            loop {
                admitter.pump().await;
                tokio::select! {
                    _ = admitter.inner.admit.notified() => {}
                    changed = admit_state.changed() => {
                        if changed.is_err() || *admit_state.borrow() == DoorState::Dead {
                            return;
                        }
                    }
                }
            }
        });

        Ok((door, announce))
    }

    /// The current state.
    pub fn state(&self) -> DoorState {
        *self.inner.state.borrow()
    }

    /// Queue one call and try to admit it right away. The stream carries
    /// every outcome from here: queue expiry, timeouts, cancellation,
    /// provider errors, process death.
    pub async fn call(&self, lane: Lane, call: SidecarCall) -> Result<CallStream, DoorError> {
        if !self.inner.ready.providers.contains(&call.provider.key) {
            return Err(DoorError::ProviderNotAllowed(call.provider.key));
        }
        let id = call.id.clone();
        let (tx, rx) = mpsc::channel(64);
        let control = Control::new(lane);
        self.inner.routes.lock().await.insert(
            id.clone(),
            Route {
                tx,
                control: control.clone(),
            },
        );
        self.inner.queue.push(
            lane,
            Entry {
                id: id.clone(),
                call,
                control: control.clone(),
            },
        );

        // The queue deadline starts at entry; it stops mattering the
        // moment the call is sent (the expiry checks the flags).
        let watchdog = self.clone();
        let queue_id = id.clone();
        let queue_wait = self.inner.config.queue_max_wait;
        tokio::spawn(async move {
            tokio::time::sleep(queue_wait).await;
            watchdog.expire_unsent(&queue_id, &control).await;
        });

        self.inner.admit.notify_one();
        Ok(CallStream {
            rx,
            door: self.clone(),
            id,
            cancelled: false,
            cancel_reported: false,
        })
    }

    /// Admit queued calls while there is room: capacity check, claim and
    /// write form one critical section under the writer lock, so pump,
    /// cancel and expiry never disagree about a call. Only the admission
    /// loop calls this.
    pub(crate) async fn pump(&self) {
        loop {
            if self.state() != DoorState::Ready {
                return;
            }
            let mut writes = self.inner.writes.lock().await;
            let total = self.inner.inflight.load(Ordering::Acquire);
            let background = self.inner.background_inflight.load(Ordering::Acquire);
            let Some(entry) =
                self.inner
                    .queue
                    .pop_next(total, background, self.inner.config.concurrency)
            else {
                return;
            };
            let line = match serde_json::to_string(&SidecarCommand::Call(entry.call.clone())) {
                Ok(line) => line,
                Err(error) => {
                    drop(writes);
                    self.fail_call(
                        &entry.id,
                        DoorError::Io(format!("command encoding: {error}")),
                    )
                    .await;
                    continue;
                }
            };
            if entry.control.is_sent() || entry.control.is_aborted() {
                drop(writes);
                continue; // cancelled or expired while queued — try the next entry
            }
            if entry.control.lane == Lane::Background {
                self.inner
                    .background_inflight
                    .fetch_add(1, Ordering::AcqRel);
            }
            self.inner.inflight.fetch_add(1, Ordering::AcqRel);
            entry.control.mark_sent();
            if writes.write_line(&line).await.is_err() {
                // The pipe is gone: killing ends the process and the
                // router's EOF fails every route with ProcessExited.
                let _ = writes.kill().await;
            }
            drop(writes);

            self.touch();
            let watchdog = self.clone();
            let id = entry.id.clone();
            let control = entry.control.clone();
            let first = self.inner.config.first_event_timeout;
            let inter = self.inner.config.inter_event_timeout;
            tokio::spawn(async move {
                watchdog.watch_events(id, control, first, inter).await;
            });
        }
    }

    /// Close stdin and wait for the child; the EOF rule is the portable
    /// orphan guard. A child that ignores EOF is killed after a grace.
    pub async fn shutdown(self) {
        let graceful = {
            let mut writes = self.inner.writes.lock().await;
            writes.close_stdin();
            tokio::time::timeout(Duration::from_millis(700), writes.wait()).await
        };
        if graceful.is_err() {
            let mut writes = self.inner.writes.lock().await;
            let _ = writes.kill().await;
            let _ = writes.wait().await;
        }
        // The router's EOF marks the door dead; nothing else is needed.
    }

    /// Admission is the only thing that resets the idle clock: events on
    /// a running call are activity, not new work.
    pub(crate) fn touch(&self) {
        self.inner.last_used_ms.store(now_ms(), Ordering::Relaxed);
    }
}

impl CallStream {
    /// The next event, or `None` once the call ended. After a local
    /// cancel the stream reports `Cancelled` exactly once and stops.
    pub async fn next(&mut self) -> Option<Result<SidecarEvent, DoorError>> {
        if self.cancelled {
            if self.cancel_reported {
                return None;
            }
            self.cancel_reported = true;
            return Some(Err(DoorError::Cancelled));
        }
        self.rx.recv().await
    }

    /// Cancel this call: queued it never reaches the sidecar, in flight
    /// it gets the cancel command, and late events find no route.
    pub async fn cancel(&mut self) {
        if self.cancelled {
            return;
        }
        self.cancelled = true;
        self.door.cancel_call(&self.id).await;
    }
}

/// A call payload for tests and callers that carry no messages yet.
pub fn sidecar_call(id: &str, provider_key: &str) -> SidecarCall {
    SidecarCall {
        id: id.to_owned(),
        provider: openbutler_protocol::sidecar::SidecarProvider {
            key: provider_key.to_owned(),
            url: None,
            api_key: None,
        },
        model: "test-model".to_owned(),
        messages: json!([]),
        system: None,
        tools: Vec::new(),
        max_output_tokens: None,
        provider_options: None,
    }
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0)
}
