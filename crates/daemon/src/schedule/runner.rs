//! `ScheduleRunner` — trigger lifetimes (`tools/schedule.ts` L69-415,
//! read 2026-11-18). Schedules, instructions, and model runs remain owned
//! by the host; this type owns only timers, statuses, and the emit that
//! publishes observed events.
//!
//! Deliberate divergences (documented in `docs/schedule-engine.md`):
//! - The source's adapter registry (`Map` of starters) is a `match` over
//!   the same closed set — `validateScheduleTrigger` guarantees the name
//!   before dispatch.
//! - Timers are tokio tasks stopped by abort (`clearTimeout`'s role); a
//!   synchronous emit cannot interleave with stop the way an awaited one
//!   could, so a due-state write may land after `stop()` in a window the
//!   source excludes. The value is rewritten on the next reload.
//! - A start-time arm failure in `daily` becomes a transient error status
//!   that reload's `active` overwrites — exactly the source's ordering;
//!   an in-task re-arm failure sticks as the error (the source does the
//!   same).
//! - Filesystem event names map Node's `rename`/`change` split onto
//!   notify's kinds: Create/Remove/Rename -> `rename`, everything else
//!   -> `change`. OS error texts come from Rust, not Node's ENOENT shape.
//! - `sysinfo`'s system-wide physical pair stands in for the source's
//!   `os.totalmem`/`os.freemem` (a per-process crate would measure the
//!   daemon, not the machine); when refresh yields nothing the sample is
//!   0 (no pressure), which the source's `totalBytes > 0` branch also does.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use notify::event::ModifyKind;
use notify::{EventKind, RecursiveMode, Watcher};
use serde_json::{Map, Value, json};

use crate::dispatch::schedule::validate_schedule_trigger;
use crate::ledger::{EventInput, LedgerWriter};
use crate::state::schedules::ScheduleState;

use super::describe::parse_local_iso;
use super::runtime::{read_trigger_due, trigger_state_key};
use super::wall::{next_daily_occurrence, weekday_numbers};

/// `setTimeout`'s 32-bit ceiling (the source clamps every arm to it).
const MAX_TIMER_MS: i64 = 2_147_483_647;
/// One-shot adapters retry a failed emit after this delay.
const RETRY_MS: i64 = 30_000;
/// `shutdown` gives a pending reload this long before refusing
/// (`withDeadline(..., 10_000, 'Schedule reload')`).
const RELOAD_DEADLINE_MS: u64 = 10_000;

/// A published event lands wherever the host's `receiveEvent` takes it;
/// the default wiring (with the pump) is the ledger's `insertEvent`.
pub type Publish = Arc<dyn Fn(EventInput) -> Result<(), String> + Send + Sync>;
/// The source's `options.log` — a message in, nothing out.
pub type Log = Arc<dyn Fn(String) + Send + Sync>;
/// One schedule's emit: `kind`, `facts`, dedupe `key` — the wrapper adds
/// source/dedupeKey/workspaceRoot/payload exactly as the source's reload
/// closure does.
type Emit = Arc<dyn Fn(&str, Value, &str) -> Result<(), String> + Send + Sync>;

/// `ScheduleStatus` — one entry per schedule after a reload.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
/// One adapter's state as `list` reports it.
pub struct ScheduleStatus {
    pub id: String,
    pub state: TriggerState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `ScheduleStatus['state']`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
/// What the adapter knows about its next firing.
pub enum TriggerState {
    Active,
    Disabled,
    Completed,
    Error,
}

/// What one adapter start returns: its stop handle and an optional
/// initial state (reload defaults it to `active`).
struct AdapterOutcome {
    stop: Box<dyn FnOnce() + Send>,
    state: Option<TriggerState>,
}

/// Owns trigger lifetimes. One per host; `reload` is serialized.
pub struct ScheduleRunner {
    writer: Arc<Mutex<LedgerWriter>>,
    publish: Publish,
    log: Log,
    stops: Mutex<HashMap<String, Box<dyn FnOnce() + Send>>>,
    statuses: Arc<Mutex<Vec<ScheduleStatus>>>,
    /// Reload serialization (the source's promise chain). Sync — the
    /// reload body never awaits, and dispatch is a sync surface.
    reload_lock: Mutex<()>,
    shutting_down: AtomicBool,
    /// The runtime that owns every adapter task. Captured at construction
    /// (boot or a test), so a sync `reload` can spawn from any thread.
    runtime: tokio::runtime::Handle,
}

/// Epoch milliseconds — the daemon-wide clock, re-exported so the trigger
/// code below reads one name.
pub(crate) use crate::ledger::now_ms;

/// `new Date(ms).toISOString()` — UTC with milliseconds.
fn to_iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|instant| instant.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_else(|| "Invalid Date".to_owned())
}

/// The source's arm delay: `Math.min(2_147_483_647, Math.max(0, due - now))`.
fn delay_ms(target: i64, now: i64) -> u64 {
    target.saturating_sub(now).clamp(0, MAX_TIMER_MS) as u64
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Map-set semantics for statuses: replace in place, keep the position.
fn set_status(statuses: &Mutex<Vec<ScheduleStatus>>, id: &str, status: ScheduleStatus) {
    let mut list = lock(statuses);
    match list.iter_mut().find(|entry| entry.id == id) {
        Some(slot) => *slot = status,
        None => list.push(status),
    }
}

fn option_i64(schedule: &ScheduleState, key: &str) -> Option<i64> {
    schedule.trigger.options.get(key).and_then(Value::as_i64)
}

/// The stored option as a string (validation guarantees the shape for
/// every adapter that reads one; a missing key reads `""` and fails in
/// the parser instead).
fn option_str<'a>(schedule: &'a ScheduleState, key: &str) -> &'a str {
    schedule
        .trigger
        .options
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
}

impl ScheduleRunner {
    /// Builds a runner over the host writer with injected publish/log.
    pub fn new(writer: Arc<Mutex<LedgerWriter>>, publish: Publish, log: Log) -> Self {
        ScheduleRunner {
            writer,
            publish,
            log,
            stops: Mutex::new(HashMap::new()),
            statuses: Arc::new(Mutex::new(Vec::new())),
            reload_lock: Mutex::new(()),
            shutting_down: AtomicBool::new(false),
            runtime: tokio::runtime::Handle::current(),
        }
    }

    /// The source's serialized `reload`: close every previous trigger,
    /// then (re)start each schedule — disabled reads `disabled`, an
    /// invalid trigger or a failing start reads `error` with a log line.
    /// Never rejects; per-schedule failures are statuses, not errors.
    pub fn reload(&self, schedules: Vec<ScheduleState>) {
        let _queued = self
            .reload_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        self.close();
        for schedule in schedules {
            let id = schedule.id.clone();
            if !schedule.enabled {
                set_status(
                    &self.statuses,
                    &id,
                    ScheduleStatus {
                        id: id.clone(),
                        state: TriggerState::Disabled,
                        error: None,
                    },
                );
                continue;
            }
            if let Err(message) = validate_schedule_trigger(&schedule.trigger) {
                set_status(
                    &self.statuses,
                    &id,
                    ScheduleStatus {
                        id: id.clone(),
                        state: TriggerState::Error,
                        error: Some(message.clone()),
                    },
                );
                (self.log)(format!("Schedule {id}: {message}"));
                continue;
            }
            let emit = self.emit_for(&schedule);
            let started: Result<AdapterOutcome, String> = match schedule.trigger.adapter.as_str() {
                "manual" => Ok(AdapterOutcome {
                    stop: Box::new(|| {}),
                    state: None,
                }),
                "interval" => self.start_interval(&schedule, emit),
                "at" => self.start_at(&schedule, emit),
                "window" => self.start_window(&schedule, emit),
                "daily" => self.start_daily(&schedule, emit),
                "filesystem" => self.start_filesystem(&schedule, emit),
                _ => self.start_memory_pressure(&schedule, emit),
            };
            match started {
                Ok(outcome) => {
                    lock(&self.stops).insert(id.clone(), outcome.stop);
                    let status = ScheduleStatus {
                        id: id.clone(),
                        state: outcome.state.unwrap_or(TriggerState::Active),
                        error: None,
                    };
                    set_status(&self.statuses, &id, status);
                }
                Err(message) => {
                    set_status(
                        &self.statuses,
                        &id,
                        ScheduleStatus {
                            id: id.clone(),
                            state: TriggerState::Error,
                            error: Some(message.clone()),
                        },
                    );
                    (self.log)(format!("Schedule {id}: {message}"));
                }
            }
        }
    }

    /// The stored schedules' adapter states (reload order preserved).
    pub fn list_statuses(&self) -> Vec<ScheduleStatus> {
        lock(&self.statuses).clone()
    }

    /// Stop every trigger and forget the statuses (the source's `close`).
    pub fn close(&self) {
        let stops = std::mem::take(&mut *lock(&self.stops));
        for (_, stop) in stops {
            stop();
        }
        lock(&self.statuses).clear();
    }

    /// Mark shutdown and wait for a reload already in flight, up to the
    /// source's 10-second deadline. On timeout the error surfaces and
    /// `close` does NOT run — exactly `withDeadline` + `shutdown`. The
    /// reload body never awaits, so a stuck lock here means a panic or a
    /// wedged thread; polling with the deadline mirrors the race.
    pub fn shutdown(&self) -> Result<(), String> {
        self.shutting_down.store(true, Ordering::SeqCst);
        let deadline = std::time::Instant::now() + Duration::from_millis(RELOAD_DEADLINE_MS);
        loop {
            match self.reload_lock.try_lock() {
                Ok(_pending) => {
                    self.close();
                    return Ok(());
                }
                Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                    drop(poisoned.into_inner());
                    self.close();
                    return Ok(());
                }
                Err(std::sync::TryLockError::WouldBlock) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(format!(
                            "Schedule reload did not finish within {RELOAD_DEADLINE_MS} ms."
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }

    /// The source's reload closure: source/dedupeKey/workspaceRoot/payload
    /// around whatever `kind`/`facts`/`key` the adapter supplies.
    fn emit_for(&self, schedule: &ScheduleState) -> Emit {
        let publish = Arc::clone(&self.publish);
        let id = schedule.id.clone();
        let adapter = schedule.trigger.adapter.clone();
        let workspace_root = schedule.workspace_root.clone();
        Arc::new(move |kind: &str, facts: Value, key: &str| {
            publish(EventInput {
                source: format!("schedule:{adapter}"),
                kind: kind.to_owned(),
                dedupe_key: format!("schedule:{id}:{key}"),
                workspace_root: Some(workspace_root.clone()),
                occurred_at: None,
                payload: json!({ "scheduleId": id, "facts": facts }),
            })
        })
    }

    // ------------------------------------------------------------- interval

    fn start_interval(
        &self,
        schedule: &ScheduleState,
        emit: Emit,
    ) -> Result<AdapterOutcome, String> {
        let seconds = option_i64(schedule, "everySeconds")
            .ok_or_else(|| "everySeconds must be an integer between 10 and 31536000.".to_owned())?;
        let state_key = trigger_state_key(schedule, "due");
        let writer = Arc::clone(&self.writer);
        let stored = {
            let writer = lock(&writer);
            read_trigger_due(schedule, |key| writer.state_get(key).ok().flatten())
        };
        let mut due = stored.unwrap_or_else(|| now_ms() + seconds * 1000);
        lock(&writer)
            .set_state(&state_key, &due.to_string())
            .map_err(|error| error.to_string())?;

        let id = schedule.id.clone();
        let log = Arc::clone(&self.log);
        let handle = self.runtime.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(delay_ms(due, now_ms()))).await;
                if due > now_ms() {
                    continue;
                }
                let facts = json!({ "scheduledAt": to_iso(due) });
                match emit("schedule.interval", facts, &due.to_string()) {
                    Ok(()) => {
                        // Missed intervals coalesce into one wakeup after a
                        // restart: the next due is computed from now.
                        due = now_ms() + seconds * 1000;
                        if let Err(error) = lock(&writer).set_state(&state_key, &due.to_string()) {
                            (log)(format!("Schedule {id}: {error}"));
                            due = now_ms() + seconds.min(30) * 1000;
                        }
                    }
                    Err(error) => {
                        (log)(format!("Schedule {id}: {error}"));
                        due = now_ms() + seconds.min(30) * 1000;
                    }
                }
            }
        });
        Ok(AdapterOutcome {
            stop: Box::new(move || handle.abort()),
            state: None,
        })
    }

    // ----------------------------------------------------------- at / window

    /// The one-shot prior-event check shared by `at` and `window`:
    /// delivered/failed reads completed, interrupted retries back to
    /// pending, pending/processing is already scheduled, anything else
    /// (including no prior event) arms fresh.
    fn one_shot_prior(
        &self,
        schedule: &ScheduleState,
        dedupe_key: &str,
    ) -> Result<Option<AdapterOutcome>, String> {
        let writer = Arc::clone(&self.writer);
        let writer = lock(&writer);
        let prior = writer
            .schedule_events(&schedule.id, 25)
            .map_err(|error| error.to_string())?
            .into_iter()
            .find(|event| event.dedupe_key == dedupe_key);
        let Some(prior) = prior else {
            return Ok(None);
        };
        match prior.status.as_str() {
            "delivered" | "failed" => Ok(Some(AdapterOutcome {
                stop: Box::new(|| {}),
                state: Some(TriggerState::Completed),
            })),
            "interrupted" => {
                writer
                    .retry_event(&prior.id)
                    .map_err(|error| error.to_string())?;
                Ok(Some(AdapterOutcome {
                    stop: Box::new(|| {}),
                    state: Some(TriggerState::Active),
                }))
            }
            "pending" | "processing" => Ok(Some(AdapterOutcome {
                stop: Box::new(|| {}),
                state: Some(TriggerState::Active),
            })),
            _ => Ok(None),
        }
    }

    fn start_at(&self, schedule: &ScheduleState, emit: Emit) -> Result<AdapterOutcome, String> {
        let due = parse_local_iso(option_str(schedule, "at")).ok_or_else(|| {
            "at must be a server-local ISO timestamp (an explicit offset is converted at creation)."
                .to_owned()
        })?;
        let key = format!("at:{due}");
        let dedupe_key = format!("schedule:{}:{key}", schedule.id);
        if let Some(outcome) = self.one_shot_prior(schedule, &dedupe_key)? {
            return Ok(outcome);
        }
        let id = schedule.id.clone();
        let log = Arc::clone(&self.log);
        let handle = self.runtime.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(delay_ms(due, now_ms()))).await;
                if due > now_ms() {
                    continue;
                }
                let facts = json!({ "scheduledAt": to_iso(due) });
                match emit("schedule.at", facts, &key) {
                    Ok(()) => break,
                    Err(error) => {
                        (log)(format!("Schedule {id}: {error}"));
                        tokio::time::sleep(Duration::from_millis(RETRY_MS as u64)).await;
                    }
                }
            }
        });
        Ok(AdapterOutcome {
            stop: Box::new(move || handle.abort()),
            state: None,
        })
    }

    fn start_window(&self, schedule: &ScheduleState, emit: Emit) -> Result<AdapterOutcome, String> {
        let from = parse_local_iso(option_str(schedule, "from"))
            .ok_or_else(|| "window.from must be a server-local ISO timestamp.".to_owned())?;
        let key = format!("window:{from}");
        let dedupe_key = format!("schedule:{}:{key}", schedule.id);
        if let Some(outcome) = self.one_shot_prior(schedule, &dedupe_key)? {
            return Ok(outcome);
        }
        let id = schedule.id.clone();
        let log = Arc::clone(&self.log);
        let handle = self.runtime.spawn(async move {
            loop {
                if from > now_ms() {
                    tokio::time::sleep(Duration::from_millis(delay_ms(from, now_ms()))).await;
                    continue;
                }
                // Past or present: fire now. A window fully in the past
                // fires immediately as late; the LATE notice is computed
                // at wake time.
                let facts = json!({ "scheduledAt": to_iso(from) });
                match emit("schedule.window", facts, &key) {
                    Ok(()) => break,
                    Err(error) => {
                        (log)(format!("Schedule {id}: {error}"));
                        tokio::time::sleep(Duration::from_millis(RETRY_MS as u64)).await;
                    }
                }
            }
        });
        Ok(AdapterOutcome {
            stop: Box::new(move || handle.abort()),
            state: None,
        })
    }

    // ---------------------------------------------------------------- daily

    fn start_daily(&self, schedule: &ScheduleState, emit: Emit) -> Result<AdapterOutcome, String> {
        let at = option_str(schedule, "at").to_owned();
        let days = weekday_numbers(schedule.trigger.options.get("days"));
        // The source arms synchronously here; an arm failure becomes a
        // transient error status that reload's `active` overwrites.
        let first = match next_daily_occurrence(now_ms(), &at, days.as_deref()) {
            Ok(occurrence) => occurrence,
            Err(message) => {
                set_status(
                    &self.statuses,
                    &schedule.id,
                    ScheduleStatus {
                        id: schedule.id.clone(),
                        state: TriggerState::Error,
                        error: Some(message),
                    },
                );
                return Ok(AdapterOutcome {
                    stop: Box::new(|| {}),
                    state: None,
                });
            }
        };
        let id = schedule.id.clone();
        let log = Arc::clone(&self.log);
        let statuses = Arc::clone(&self.statuses);
        let handle = self.runtime.spawn(async move {
            let mut due = first.at_ms;
            let mut shifted = first.shifted;
            // Each arm recomputes the next occurrence from the wall clock,
            // exactly as the source's `arm()`.
            macro_rules! arm {
                () => {
                    match next_daily_occurrence(now_ms(), &at, days.as_deref()) {
                        Ok(occurrence) => {
                            due = occurrence.at_ms;
                            shifted = occurrence.shifted;
                        }
                        Err(message) => {
                            set_status(
                                &statuses,
                                &id,
                                ScheduleStatus {
                                    id: id.clone(),
                                    state: TriggerState::Error,
                                    error: Some(message),
                                },
                            );
                            return;
                        }
                    }
                };
            }
            loop {
                tokio::time::sleep(Duration::from_millis(delay_ms(due, now_ms()))).await;
                if due > now_ms() {
                    arm!();
                    continue;
                }
                let mut facts = Map::new();
                facts.insert("scheduledAt".to_owned(), Value::String(to_iso(due)));
                if shifted {
                    facts.insert("clockShift".to_owned(), Value::Bool(true));
                }
                match emit(
                    "schedule.daily",
                    Value::Object(facts),
                    &format!("daily:{due}"),
                ) {
                    Ok(()) => arm!(),
                    Err(error) => {
                        (log)(format!("Schedule {id}: {error}"));
                        tokio::time::sleep(Duration::from_millis(RETRY_MS as u64)).await;
                    }
                }
            }
        });
        Ok(AdapterOutcome {
            stop: Box::new(move || handle.abort()),
            state: None,
        })
    }

    // ------------------------------------------------------------ filesystem

    fn start_filesystem(
        &self,
        schedule: &ScheduleState,
        emit: Emit,
    ) -> Result<AdapterOutcome, String> {
        let raw = option_str(schedule, "path");
        let path = std::fs::canonicalize(raw).map_err(|error| error.to_string())?;
        let is_directory = std::fs::metadata(&path)
            .map_err(|error| error.to_string())?
            .is_dir();
        // `recursive !== false`: absent or any non-false value watches a
        // directory recursively (validation keeps it boolean).
        let recursive =
            is_directory && schedule.trigger.options.get("recursive") != Some(&Value::Bool(false));
        let debounce_ms = option_i64(schedule, "debounceSeconds").unwrap_or(5) * 1000;

        let changes: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let overflow = Arc::new(AtomicBool::new(false));
        let signal = Arc::new(tokio::sync::Notify::new());
        let statuses = Arc::clone(&self.statuses);
        let log = Arc::clone(&self.log);
        let watcher_id = schedule.id.clone();

        let mut watcher = notify::recommended_watcher({
            let changes = Arc::clone(&changes);
            let overflow = Arc::clone(&overflow);
            let signal = Arc::clone(&signal);
            let statuses = Arc::clone(&statuses);
            let log = Arc::clone(&log);
            let watcher_id = watcher_id.clone();
            move |result: Result<notify::Event, notify::Error>| match result {
                Ok(event) => {
                    let name = match event.kind {
                        EventKind::Create(_)
                        | EventKind::Remove(_)
                        | EventKind::Modify(ModifyKind::Name(_)) => "rename",
                        _ => "change",
                    };
                    {
                        let mut list = lock(&changes);
                        for changed in &event.paths {
                            let path = changed.to_string_lossy().into_owned();
                            let position = list.iter().position(|(seen, _)| *seen == path);
                            match position {
                                Some(index) => list[index].1 = name.to_owned(),
                                None if list.len() < 100 => {
                                    list.push((path, name.to_owned()));
                                }
                                None => overflow.store(true, Ordering::SeqCst),
                            }
                        }
                    }
                    signal.notify_one();
                }
                Err(error) => {
                    // The source's `watcher.on('error')`: status + log.
                    let message = error.to_string();
                    set_status(
                        &statuses,
                        &watcher_id,
                        ScheduleStatus {
                            id: watcher_id.clone(),
                            state: TriggerState::Error,
                            error: Some(message.clone()),
                        },
                    );
                    (log)(format!("Schedule {watcher_id}: {message}"));
                }
            }
        })
        .map_err(|error| error.to_string())?;
        watcher
            .watch(
                &path,
                if recursive {
                    RecursiveMode::Recursive
                } else {
                    RecursiveMode::NonRecursive
                },
            )
            .map_err(|error| error.to_string())?;

        let root = path.to_string_lossy().into_owned();
        let id = schedule.id.clone();
        let handle = self.runtime.spawn(async move {
            loop {
                if lock(&changes).is_empty() {
                    signal.notified().await;
                }
                if lock(&changes).is_empty() {
                    // A stale permit (the change it announced already
                    // flushed): wait for a real one.
                    continue;
                }
                // One debounce window from the first event — the source's
                // timer is set once, not slid by later changes.
                let deadline =
                    tokio::time::Instant::now() + Duration::from_millis(debounce_ms as u64);
                while tokio::time::timeout_at(deadline, signal.notified())
                    .await
                    .is_ok()
                {}
                let (collected, overflowed) = {
                    let mut list = lock(&changes);
                    let entries: Vec<Value> = list
                        .iter()
                        .map(|(path, name)| json!({ "event": name, "path": path }))
                        .collect();
                    let flag = overflow.swap(false, Ordering::SeqCst);
                    list.clear();
                    (entries, flag)
                };
                if collected.is_empty() {
                    continue;
                }
                let facts = json!({
                    "root": root,
                    "changes": collected,
                    "overflow": overflowed,
                });
                let key = uuid::Uuid::new_v4().to_string();
                if let Err(error) = emit("filesystem.changed", facts, &key) {
                    (log)(format!("Schedule {id}: {error}"));
                }
            }
        });
        Ok(AdapterOutcome {
            stop: Box::new(move || {
                drop(watcher);
                handle.abort();
            }),
            state: None,
        })
    }

    // ------------------------------------------------------- memory_pressure

    fn start_memory_pressure(
        &self,
        schedule: &ScheduleState,
        emit: Emit,
    ) -> Result<AdapterOutcome, String> {
        let threshold = option_i64(schedule, "usedPercent")
            .ok_or_else(|| "usedPercent must be an integer between 50 and 99.".to_owned())?;
        let sample_ms = option_i64(schedule, "sampleSeconds").unwrap_or(30) * 1000;
        let duration_ms = option_i64(schedule, "durationSeconds").unwrap_or(60) * 1000;
        let log = Arc::clone(&self.log);
        let mut system = sysinfo::System::new().map_err(|error| error.to_string())?;
        let handle = self.runtime.spawn(async move {
            let mut exceeded_at: Option<i64> = None;
            let mut incident: Option<String> = None;
            loop {
                tokio::time::sleep(Duration::from_millis(sample_ms as u64)).await;
                system.refresh_memory();
                let total_bytes = i64::try_from(system.total_memory()).unwrap_or(0);
                let free_bytes = i64::try_from(system.available_memory()).unwrap_or(0);
                let used_percent = if total_bytes > 0 {
                    (1.0 - (free_bytes as f64 / total_bytes as f64)) * 100.0
                } else {
                    0.0
                };
                if used_percent >= threshold as f64 {
                    if exceeded_at.is_none() {
                        exceeded_at = Some(now_ms());
                    }
                    if incident.is_none()
                        && now_ms() - exceeded_at.unwrap_or_default() >= duration_ms
                    {
                        let key = uuid::Uuid::new_v4().to_string();
                        let facts = json!({
                            "totalBytes": total_bytes,
                            "freeBytes": free_bytes,
                            "usedPercent": used_percent,
                            "threshold": threshold,
                        });
                        if let Err(error) = emit("system.memory_pressure", facts, &key) {
                            (log)(error);
                        }
                        incident = Some(key);
                    }
                } else if used_percent < (threshold - 5) as f64 {
                    exceeded_at = None;
                    if let Some(recovered) = incident.take() {
                        let facts = json!({
                            "totalBytes": total_bytes,
                            "freeBytes": free_bytes,
                            "usedPercent": used_percent,
                            "threshold": threshold,
                        });
                        if let Err(error) = emit(
                            "system.memory_recovered",
                            facts,
                            &format!("{recovered}:recovered"),
                        ) {
                            (log)(error);
                        }
                    }
                }
            }
        });
        Ok(AdapterOutcome {
            stop: Box::new(move || handle.abort()),
            state: None,
        })
    }
}
