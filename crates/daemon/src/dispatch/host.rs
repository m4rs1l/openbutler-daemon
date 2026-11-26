//! The shared record: what one connection carries between requests, and
//! what the daemon reports.
//!
//! `DispatchState` is the per-connection stand-in for a host-wide current
//! controller; `Host` is the authority for what `status` reports (identity,
//! start time, live connection count, shutdown signal) and owns the live
//! services (settings, fanout, executor, runtime view).

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};

use openbutler_protocol::DaemonStatus;
use openbutler_protocol::ScreenLine;
use openbutler_protocol::entry::UserEntrySource;
use openbutler_protocol::runtime::RuntimeViewData;
use tokio::sync::watch;

use super::fanout::{self, Fanout};
use crate::agent::TurnExecutor;
use crate::auth::PairingCodes;
use crate::settings::{LoadedSettings, SettingsError, load_settings};

/// The chat this connection has opened. `session.load` records it; the
/// paging and attachment reads address it — the per-connection stand-in for
/// a host-wide current controller (there is no runtime in this build,
/// so the record on disk is the whole truth).
#[derive(Default)]
/// Per-connection dispatch state (current chat, sink, queue flags).
pub struct DispatchState {
    pub(super) current_session: Option<String>,
    /// The client kind the connection attached with; until `attach` sets
    /// it, sends read as terminal (the source's default channel).
    pub(super) client_kind: Option<UserEntrySource>,
    /// This connection's write path, set by the server before dispatch
    /// runs; `attach` subscribes it to the session's fan-out.
    pub(super) sink: Option<fanout::SharedSink>,
    /// The fan-out subscription this connection owns (dropped on a new
    /// attach or when the connection's state is discarded).
    pub(super) subscription: Option<u64>,
    /// Frames an action wants sent *after* its response (the attach
    /// snapshot event rides behind the answer, daemon-host.ts L1459).
    pub(super) pending_events: Vec<Vec<u8>>,
    /// View-only binding: set when the attached chat belongs to a worker
    /// session (arrives with the workers slice); notices stay postable.
    pub read_only: bool,
    /// The workspace this connection is bound to, set by `attach` (and
    /// re-set when `session.load` switches chats, L3633). Every
    /// post-attach action must echo this key (daemon-host.ts L3478).
    pub binding_key: Option<String>,
    /// The action asked the host to stop (the `stop` arm). The connection
    /// performs the request after the response is written: the source
    /// wrote the ack before `setImmediate(requestStop)` (L3211-3212), and
    /// a process that exits first would lose the answer.
    pub(super) shutdown_after_response: bool,
    /// Whether this connection arrived through the remote TCP door. The
    /// server sets it for `Admission::Remote`; credential-minting actions
    /// (`pair.create`, `token.*`) refuse a remote caller — a new contract,
    /// the source had no gate there.
    pub remote: bool,
}

impl DispatchState {
    /// A fresh connection holds no chat yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The id `session.load` last succeeded with (tests and diagnostics).
    pub fn current_session(&self) -> Option<&str> {
        self.current_session.as_deref()
    }

    /// Hands dispatch the connection's write path (server wiring; tests
    /// pass a recording sink).
    pub fn set_sink(&mut self, sink: fanout::SharedSink) {
        self.sink = Some(sink);
    }

    /// The frames queued behind the current response, drained by the
    /// connection after it writes it.
    pub fn take_pending_events(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.pending_events)
    }

    /// Whether the action that just ran asked for a shutdown; the
    /// connection calls `Host::request_shutdown` once the response is out.
    pub fn take_shutdown_request(&mut self) -> bool {
        let requested = self.shutdown_after_response;
        self.shutdown_after_response = false;
        requested
    }

    /// Marks the connection as remote (the server, before the first frame).
    pub fn set_remote(&mut self, remote: bool) {
        self.remote = remote;
    }
}

/// What the daemon reports about itself and what keeps it alive.
pub struct Host {
    started_at: String,
    connected_clients: AtomicU32,
    shutdown: watch::Sender<bool>,
    pairing: PairingCodes,
    /// The live settings copy every runtime read goes through. Loaded and
    /// reloaded only by `reload_settings` — one writer for the host.
    settings: RwLock<Option<LoadedSettings>>,
    /// The host-wide turn executor (door + runners). Set once at boot;
    /// `session.send` refuses without it.
    executor: RwLock<Option<Arc<dyn TurnExecutor>>>,
    /// Who receives a session's published events (c3): the roster lives
    /// with the host because a runner publishes across connections.
    fanout: Fanout,
    /// The runtime view every published frame carries. Built when a turn
    /// is prepared (`session.send`) until the full `toRuntimeView` port
    /// lands with `session.attach`.
    runtime_view: RwLock<Option<RuntimeViewData>>,
    /// Model picks waiting for their turn boundary
    /// (`session.select-model`): the arm records here, `prepare_send`
    /// reads it for the request, and the runner takes it into the
    /// record when the turn starts. Std mutex — every section is a map
    /// touch, never spanning an await.
    model_picks: std::sync::Mutex<std::collections::HashMap<String, (String, String)>>,
    /// The schedule runner (trigger lifetimes). Set once at boot; the
    /// `schedule.manage` CRUD reloads it and `list` reads its statuses.
    scheduler: RwLock<Option<crate::schedule::ScheduleRunner>>,
    /// The host's own weak handle (set at boot): lets a `&self` method
    /// spawn the schedule pump with an owned `Arc<Host>` — spawned
    /// tasks need `'static`, and dispatch only ever holds `&Host`.
    self_ref: std::sync::OnceLock<std::sync::Weak<Host>>,
    /// Stop counters per lane key (the source's `stopGenerations`):
    /// `session.stop` bumps, the pump captures before each firing and
    /// refuses a turn whose generation moved. Map touch, never an await.
    stop_generations: std::sync::Mutex<std::collections::HashMap<String, u32>>,
    /// The models.dev catalog, set once at boot: usage pricing reads it
    /// (`catalog-estimate`) and later slices serve the pickers from it.
    /// Optional so tests that do not exercise catalog behavior stay
    /// unchanged — without it, usage rows keep `cost_source: none`.
    catalog: std::sync::OnceLock<std::sync::Arc<crate::catalog::CatalogStore>>,
    /// A catalog download is in flight (the click-free "load it" path of
    /// `config.report`). Compare-exchange, so one download per process at a
    /// time and never two threads racing the same generation.
    catalog_warm: Arc<std::sync::atomic::AtomicBool>,
    /// The live connections a host notification reaches (`stop`'s
    /// restart/stop broadcast). Registered by the server per connection;
    /// the requester is always skipped by pointer identity.
    clients: crate::dispatch::clients::Clients,
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
            pairing: PairingCodes::new(),
            settings: RwLock::new(None),
            executor: RwLock::new(None),
            fanout: Fanout::new(),
            runtime_view: RwLock::new(None),
            model_picks: std::sync::Mutex::new(std::collections::HashMap::new()),
            scheduler: RwLock::new(None),
            self_ref: std::sync::OnceLock::new(),
            stop_generations: std::sync::Mutex::new(std::collections::HashMap::new()),
            catalog: std::sync::OnceLock::new(),
            catalog_warm: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            clients: crate::dispatch::clients::Clients::new(),
        }
    }

    /// The live-connection roster host notifications walk.
    pub fn clients(&self) -> &crate::dispatch::clients::Clients {
        &self.clients
    }

    /// Installs the catalog (boot only; first call wins).
    pub fn set_catalog(&self, catalog: std::sync::Arc<crate::catalog::CatalogStore>) {
        let _ = self.catalog.set(catalog);
    }

    /// The catalog, once boot has installed it.
    pub fn catalog(&self) -> Option<std::sync::Arc<crate::catalog::CatalogStore>> {
        self.catalog.get().cloned()
    }

    /// Starts one background catalog download when nothing is cached and
    /// none is running — the config screen's "load it" path. Returns `true`
    /// while a download is in flight (started now, or already running).
    ///
    /// Dispatch is synchronous, so the download runs on its own thread with
    /// its own current-thread runtime: no connection waits on the network,
    /// and no caller needs a runtime context.
    pub fn warm_catalog(&self) -> bool {
        let Some(catalog) = self.catalog() else {
            return false;
        };
        if catalog.is_loaded() {
            return false;
        }
        if self.catalog_warm.swap(true, Ordering::AcqRel) {
            return true;
        }
        struct SharedReset(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for SharedReset {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let flag = Arc::clone(&self.catalog_warm);
        std::thread::spawn(move || {
            let _reset = SharedReset(flag);
            let built = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            let outcome = match built {
                Ok(runtime) => runtime
                    .block_on(
                        catalog
                            .get_index(&crate::catalog::IndexOptions::at(crate::ledger::now_ms())),
                    )
                    .map(|_| ()),
                Err(error) => Err(error.to_string()),
            };
            if let Err(error) = outcome {
                eprintln!("openbutler-daemon: the provider catalog could not be loaded: {error}");
            }
        });
        true
    }

    /// The live settings copy, if it has been loaded yet. `None` means
    /// nobody called `reload_settings` — the server boot path always does.
    pub fn settings(&self) -> std::sync::RwLockReadGuard<'_, Option<LoadedSettings>> {
        self.settings
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Refresh the live copy from disk under the same lock every reader
    /// uses. A failed load leaves the previous copy untouched: a broken
    /// file must never evict working settings.
    pub fn reload_settings(&self, data_dir: &Path) -> Result<(), SettingsError> {
        let fresh = load_settings(data_dir)?;
        *self
            .settings
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(fresh);
        Ok(())
    }

    /// The shared pairing state: `pair.create` issues through it, the
    /// admission gate redeems through it.
    pub fn pairing(&self) -> &PairingCodes {
        &self.pairing
    }

    /// Queues a model pick for the session's next turn boundary
    /// (`session.select-model` — accepted while a run is going; it
    /// never lands under the running request).
    pub fn queue_model_pick(&self, session_id: &str, provider_id: String, model_id: String) {
        self.model_picks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session_id.to_owned(), (provider_id, model_id));
    }

    /// The pick waiting for the session's next turn, if any. Read by
    /// `prepare_send`, which must not consume it — the runner takes it
    /// at the boundary.
    pub fn peek_model_pick(&self, session_id: &str) -> Option<(String, String)> {
        self.model_picks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .cloned()
    }

    /// Consumes the pick exactly once, at the turn boundary (legacy
    /// `selectModel` adopting the session's state).
    pub fn take_model_pick(&self, session_id: &str) -> Option<(String, String)> {
        self.model_picks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(session_id)
    }

    /// The event roster runners publish to.
    pub fn fanout(&self) -> &Fanout {
        &self.fanout
    }

    /// The runtime view published frames carry, until the full
    /// `toRuntimeView` port lives with `session.attach`.
    pub fn runtime_view(&self) -> Option<RuntimeViewData> {
        self.runtime_view
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Sets the runtime view a prepared turn publishes with (built from
    /// the settings `session.send` resolved).
    pub fn set_runtime_view(&self, view: RuntimeViewData) {
        *self
            .runtime_view
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(view);
    }

    /// Install the turn executor (boot wiring or a test double).
    pub fn set_executor(&self, executor: Arc<dyn TurnExecutor>) {
        *self
            .executor
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(executor);
    }

    /// Points the host at its own `Arc` (boot wiring): the handle the
    /// pump spawn upgrades.
    pub fn set_self_ref(&self, host: &Arc<Host>) {
        let _ = self.self_ref.set(Arc::downgrade(host));
    }

    /// The source's `stopGenerations.set(key, (get ?? 0) + 1)` — a stop
    /// without `keepQueue` moves the lane's generation so a firing
    /// captured before it refuses to run.
    pub fn bump_stop_generation(&self, lane_key: &str) {
        let mut map = self
            .stop_generations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *map.entry(lane_key.to_owned()).or_insert(0) += 1;
    }

    /// The current generation counter for a lane (missing = 0).
    pub fn stop_generation(&self, lane_key: &str) -> u32 {
        self.stop_generations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(lane_key)
            .copied()
            .unwrap_or(0)
    }

    /// Runs one schedule-pump pass on the runtime (the source's
    /// `pumpEvents` at startup and after every queued event). Safe to
    /// call before `set_self_ref`: without the handle it does nothing.
    pub fn spawn_schedule_pump(&self) {
        let Some(host) = self.self_ref.get().and_then(std::sync::Weak::upgrade) else {
            return;
        };
        let Some(runtime) = tokio::runtime::Handle::try_current().ok() else {
            return;
        };
        runtime.spawn(async move {
            if let Err(error) =
                crate::schedule::pump::pump_once(&host, &crate::config::data_dir()).await
            {
                eprintln!("openbutler-daemon: schedule pump: {error}");
            }
        });
    }

    /// The installed executor, if any.
    pub fn executor(&self) -> Option<Arc<dyn TurnExecutor>> {
        self.executor
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Install the schedule runner (boot wiring or a test).
    pub fn set_scheduler(&self, scheduler: crate::schedule::ScheduleRunner) {
        *self
            .scheduler
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(scheduler);
    }

    /// The installed runner (the CRUD reloads it, `list` reads its
    /// statuses). `None` before boot wiring: adapter states then read
    /// `idle`, which is the source's fallback for an unknown id.
    pub fn scheduler(
        &self,
    ) -> std::sync::RwLockReadGuard<'_, Option<crate::schedule::ScheduleRunner>> {
        self.scheduler
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The current status view (`status` action).
    pub fn status(&self) -> DaemonStatus {
        let connected_clients = self.connected_clients.load(Ordering::SeqCst);
        // The status screen, composed here: the rows the source printed,
        // label-padded to fourteen columns, with the optional rows only when
        // the host actually reports them. A client prints it.
        let lines = vec![
            ScreenLine::plain(format!("{:<14} {}", "pid:", std::process::id())),
            ScreenLine::plain(format!("{:<14} {}", "started:", self.started_at)),
            ScreenLine::plain(format!("{:<14} 0", "workspaces:")),
            ScreenLine::plain(format!("{:<14} {connected_clients}", "clients:")),
            // A source run reports an explicit null (contract: null = source
            // run, absent = older host), and the screen's row says `source`.
            ScreenLine::plain(format!("{:<14} source", "build:")),
        ];
        DaemonStatus {
            pid: std::process::id(),
            started_at: self.started_at.clone(),
            // Workspace runtimes are not implemented yet: an honest zero,
            // not a fabricated count.
            workspace_count: 0,
            connected_clients,
            schedules: None,
            telegram: None,
            unresponsive: None,
            entry: None,
            // A source run reports an explicit null (contract: null = source
            // run, absent = older host).
            build_id: Some(None),
            lines,
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
