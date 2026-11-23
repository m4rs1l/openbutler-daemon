//! The agent turn loop: admission → model stream → transcript → settlement.
//!
//! Source: legacy `src/agent-session.ts` (`runQueuedMessages`, `addUserEntry`,
//! `finishTurn`, the streamed-entry helpers). What lives here and what does
//! not:
//!
//! - **One turn at a time.** `send` serialises on the session state; the
//!   visible queue, steering admission and compaction arrive with the
//!   runtime slice.
//! - **Persistence has two forced points** — the admission save (a user
//!   message must survive a crash while the model is still thinking) and
//!   the settlement save — plus a best-effort 1 s dirty save during the
//!   stream (the legacy debounce's role).
//! - **The system prompt and its context are landed** (`prompt`,
//!   `providers`, `context`); the batch loop executes tools host-side
//!   between steps (`docs/tool-pipeline.md`), while the visible queue
//!   and the approval API remain with their own slices and are recorded
//!   in `docs/agent-loop.md`.

mod approval;
mod compact;
pub mod context;
mod entry;
pub mod events;
pub mod executor;
pub mod prompt;
pub mod providers;
mod question;
pub mod queue;

pub use executor::TurnExecutor;
pub use queue::QueueAction;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::Utc;
use openbutler_protocol::SessionEntryMeta;
use openbutler_protocol::entry::{NoticeTone, SessionEntry, TurnOutcome, UserEntrySource};
use openbutler_protocol::session::{
    RunState, SessionView, ToolCallRequest, ToolChange, ToolData, ToolEffect, ToolNetwork,
    ToolOutbound, ToolReversible,
};
use openbutler_protocol::sidecar::SidecarEvent;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::ledger::{LedgerWriter, StepUsageReport, UsageInput, UsageTokens};
use crate::sidecar::{DoorError, Lane, SidecarDoor, sidecar_call};
use crate::state::{self, SaveOutcome, SessionRecord, StateError, save_session};
use crate::tool_runtime::catalog::native_definition;
use crate::tool_runtime::format::format_tool_result;
use crate::tool_runtime::pipeline::{
    ApprovalOutcome, AskFuture, AskPort, GateDecision, ToolCallContext, assess, proposal_failure,
    run_tool_call, stop_result,
};
use entry::{
    finish_reasoning_block, finish_user_entry, grow_assistant, grow_reasoning, push_notice,
    settle_tool_entry,
};
use events::PublishSnapshot;
use queue::{fill_queue, push_user_entry};

/// Host-authored stop: the source has no counter because its loop is
/// SDK-owned; a runaway tool loop must not hold the turn (and the
/// session lock) forever. At 24 steps the turn fails with a notice.
const MAX_TOOL_STEPS: usize = 24;

/// The manual-approval wait when settings do not pin one (legacy
/// `DEFAULT_APPROVAL_TIMEOUT_SECONDS`, L328 — not the tool's 60 s
/// execution default).
const DEFAULT_APPROVAL_TIMEOUT_SECONDS: u64 = 120;

/// One turn's input. The provider block mirrors what the runtime resolves
/// from settings; the transcript keeps only `text` and `source`.
#[derive(Debug, Clone)]
/// One prepared turn: message, provider binding, prompt, and the gate
/// settings read at the turn boundary.
pub struct TurnRequest {
    pub text: String,
    pub source: UserEntrySource,
    pub provider_key: String,
    pub provider_url: Option<String>,
    pub provider_api_key: Option<String>,
    pub model: String,
    /// The assembled system prompt for this turn; `None` sends none.
    pub system: Option<String>,
    /// The chat's workspace — model-only label, never the transcript.
    pub workspace_root: Option<String>,
    /// Raw images from the message; the executor decodes and stores them
    /// before the turn runs (the legacy `enqueueMessage` conversion).
    pub images: Vec<QueuedImage>,
    /// Stored files carried into the transcript and the model message as
    /// references.
    pub attachments: Vec<openbutler_protocol::entry::FileAttachment>,
    /// The approval policy in force, captured at the turn boundary from
    /// settings; the queue (`session.approve`) resolves the calls whose
    /// gate says it needs approval.
    pub approval_mode: crate::settings::ApprovalMode,
    /// The shell setting baked into the execute_command description.
    pub command_shell: Option<String>,
    /// The manual-approval wait in seconds (settings
    /// `approvalTimeoutSeconds`); `None` means the source's 120.
    pub approval_timeout_seconds: Option<u64>,
}

/// One validated image riding a turn until it is stored.
#[derive(Debug, Clone)]
/// A base64 image queued with a send (decoded pre-turn).
pub struct QueuedImage {
    pub base64: String,
    pub media_type: String,
}

/// What a settled turn did — the same stamps `finishTurn` writes.
#[derive(Debug, Clone)]
/// What a settled turn produced (report text or the error).
pub struct TurnReport {
    pub turn_id: String,
    pub outcome: TurnOutcome,
}

/// Failures that stop a turn before or instead of a provider answer. A
/// provider failure is not an error value: it settles the turn `failed`
/// with a notice, exactly like the legacy `runError` path.
#[derive(Debug, thiserror::Error)]
/// Why a turn could not start or settle.
pub enum TurnError {
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Door(#[from] DoorError),
    #[error("Session not found.")]
    NotFound,
    #[error("The session record could not be read.")]
    Unreadable,
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

struct SessionState {
    record: SessionRecord,
    /// Set on every mutation; cleared by a successful save so the periodic
    /// tick never rewrites an unchanged record.
    dirty: bool,
}

/// One session's turn loop: queue, approvals, questions, and the
/// record it owns while a turn runs.
pub struct TurnRunner {
    door: SidecarDoor,
    data_root: PathBuf,
    session_id: String,
    state: Mutex<SessionState>,
    stop: Arc<AtomicBool>,
    /// The one pending manual approval while a gate waits (the queue
    /// behind `session.approve`; one at a time, like the source's
    /// `pendingApproval` pair). Std mutex: every critical section here
    /// is short and never spans an await — the wait rides the oneshot.
    approval: std::sync::Mutex<Option<PendingApproval>>,
    /// The one pending model question (the queue behind
    /// `session.answer`; same lock discipline as `approval`). Shared so
    /// the `question` tool's port owns a clone of the same queue.
    question: Arc<std::sync::Mutex<Option<QueuedQuestion>>>,
    /// Wakes the turn owner when a question is queued: `send`'s tool
    /// select publishes the roster frame — the tool waits under the
    /// held `state` lock, where a re-entrant publish would deadlock.
    publish_notify: Arc<tokio::sync::Notify>,
    /// The host this runner publishes live state to (c3). Weak: a runner
    /// must never keep the host alive.
    host: std::sync::Weak<crate::dispatch::Host>,
    /// The last state published to the roster; `None` until the first
    /// publish seeds it (an attach answers with a snapshot of its own).
    /// Std mutex (like `approval`): the sections are short and never
    /// span an await — `publish_now` is synchronous.
    last_published: std::sync::Mutex<Option<PublishSnapshot>>,
    /// The send queue — one lock owns the deque; the drain duty lives
    /// in `drain_active` (an enqueue can never race the idle check).
    scheduler: std::sync::Mutex<Scheduler>,
    /// A drain loop is alive: the enqueue that flips it false→true
    /// starts the loop, so a direct `send` (no loop) still gets its
    /// queued messages drained afterwards.
    drain_active: AtomicBool,
    /// The turn currently running — the admission current for
    /// steering refusals and the busy state `session.queue` reads.
    active_current: std::sync::Mutex<Option<TurnRequest>>,
    /// The chat's workspace binding (the source's
    /// `sessionWorkspaceRoot`): the steering refusal's bypass input.
    session_workspace: std::sync::Mutex<Option<String>>,
    /// A compaction requested while a turn runs: stored under the
    /// `active_current` lock (compact.rs) so the drain's take after
    /// the run can never miss it, and served when the run ends.
    pending_compaction: std::sync::Mutex<Option<bool>>,
    /// The running turn's id — the source's `activeTurnId`, which
    /// stamps notices and roster frames while a run is live.
    active_turn_id: std::sync::Mutex<Option<String>>,
}

/// The queue — the drain duty is the runner's `drain_active` flag.
struct Scheduler {
    turns: std::collections::VecDeque<queue::QueuedTurn>,
}

/// A queued approval awaiting the client's decision.
struct PendingApproval {
    request_id: String,
    /// The wire-shaped request the roster publishes (`pendingApproval`).
    request: openbutler_protocol::session::ToolCallRequest,
    gate_reason: &'static str,
    sender: tokio::sync::oneshot::Sender<bool>,
}

/// A model question waiting for the owner (the `question` tool's queue).
struct QueuedQuestion {
    question_id: String,
    /// The owner-visible text and options the roster publishes.
    text: String,
    options: Option<Vec<String>>,
    sender: tokio::sync::oneshot::Sender<Option<String>>,
}

/// One drained step: the outcome, the streamed reply, the tool
/// calls collected for the host to execute between steps, and the
/// accounting report when the step produced a finished outcome.
struct DrainResult {
    outcome: TurnOutcome,
    reply: String,
    calls: Vec<CollectedCall>,
    usage: Option<StepUsageReport>,
}

/// A model-requested tool call awaiting host execution.
struct CollectedCall {
    call_id: String,
    name: String,
    arguments: Value,
}

impl TurnRunner {
    /// Opens an existing session record for turns (bounded, id-verified
    /// read). The door is shared: every session of the host rides one
    /// sidecar process.
    pub async fn open(
        data_root: &Path,
        session_id: &str,
        door: SidecarDoor,
        host: std::sync::Weak<crate::dispatch::Host>,
    ) -> Result<TurnRunner, TurnError> {
        let path = state::record_path(data_root, session_id).ok_or(TurnError::NotFound)?;
        let record = state::read_session_record(&path, session_id).ok_or(TurnError::Unreadable)?;
        Ok(TurnRunner {
            door,
            data_root: data_root.to_owned(),
            session_id: session_id.to_owned(),
            state: Mutex::new(SessionState {
                record,
                dirty: false,
            }),
            stop: Arc::new(AtomicBool::new(false)),
            approval: std::sync::Mutex::new(None),
            question: Arc::new(std::sync::Mutex::new(None)),
            publish_notify: Arc::new(tokio::sync::Notify::new()),
            host,
            last_published: std::sync::Mutex::new(None),
            scheduler: std::sync::Mutex::new(Scheduler {
                turns: std::collections::VecDeque::new(),
            }),
            drain_active: AtomicBool::new(false),
            active_current: std::sync::Mutex::new(None),
            session_workspace: std::sync::Mutex::new(None),
            pending_compaction: std::sync::Mutex::new(None),
            active_turn_id: std::sync::Mutex::new(None),
        })
    }

    /// Cancels the running turn (the owner's stop). A stop that arrives
    /// between turns is drained at the next admission. A pending
    /// question is dismissed too (the source's stop handler, L1259),
    /// and the queued sends are cancelled unless `keepQueue` was set
    /// (L1256).
    pub fn stop(&self, keep_queue: bool) {
        self.stop.store(true, Ordering::SeqCst);
        let pending = self
            .question
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(pending) = pending {
            let _ = pending.sender.send(None);
        }
        if !keep_queue {
            self.scheduler
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .turns
                .clear();
        }
    }

    /// Runs one turn to settlement: the user entry lands and is saved
    /// before the model is asked, the stream maps into transcript entries,
    /// and the user entry is stamped `completed`/`cancelled`/`failed`.
    pub async fn send(&self, request: TurnRequest) -> Result<TurnReport, TurnError> {
        let mut state = self.state.lock().await;
        // Turn boundary: a queued model pick lands on the record before
        // anything of this turn is built — legacy `selectModel` adopting
        // the session's state; the admission save writes it to disk.
        if let Some(host) = self.host.upgrade()
            && let Some((provider_id, model_id)) = host.take_model_pick(&self.session_id)
        {
            state.record.meta.provider_id = provider_id;
            state.record.meta.model_id = model_id;
            state.dirty = true;
        }
        self.stop.store(false, Ordering::SeqCst);
        let turn_id = Uuid::new_v4().to_string();
        // The running turn is the admission current (steering
        // refusals compare against it) and the chat's workspace
        // binding feeds the refusal bypass.
        *self
            .active_current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(request.clone());
        *self
            .active_turn_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(turn_id.clone());
        *self
            .session_workspace
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            state.record.meta.workspace_root.clone();
        push_user_entry(&mut state, &request, &turn_id);
        // Admission save: the message on disk before any provider call.
        self.save(&mut state)?;
        self.publish_now(&state, Some(&turn_id), None, true);

        let mut call = sidecar_call(&turn_id, &request.provider_key);
        call.provider.url = request.provider_url.clone();
        call.provider.api_key = request.provider_api_key.clone();
        call.model = request.model.clone();
        call.system = request.system.clone();

        // The batch loop: one door call per step, tools executed by the
        // host in between. The SDK's own loop stays off (the door runs
        // `stopWhen: isStepCount(1)`), so a step that ends in tool calls
        // needs a follow-up request carrying the paired messages.
        let tool_workspace = request
            .workspace_root
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.data_root.clone());
        // The catalog the model sees (`sidecar_tool` shapes it with the
        // denotation suffix and the timeout field). Built once per turn
        // from the turn's workspace and shell — the batch loop resolves
        // names per call from the same catalog.
        call.tools = ["execute_command", "question"]
            .iter()
            .filter_map(|name| {
                native_definition(name, &tool_workspace, request.command_shell.as_deref())
            })
            .filter_map(|definition| crate::tool_runtime::catalog::sidecar_tool(&definition).ok())
            .collect();
        // One ledger row per model request (legacy `onStepUsage`): the
        // writer opens once per turn; a database that will not open means
        // no rows — the source's absent sink (`if (!sink) return`).
        let mut usage_ledger = LedgerWriter::open(&self.data_root).ok();
        let mut steps = 0usize;
        let (outcome, final_reply) = loop {
            steps += 1;
            // The prepareStep order (agent.ts L411/L426): steered
            // messages enter first, then the compaction boundary runs
            // — the previous tool batch settled, the next model
            // request not built. The guard releases so the boundary
            // (queued-forced or the automatic threshold check) can
            // take the state, and the request below reads whatever
            // conversation it leaves.
            self.admit_steered(&mut state, &turn_id, &request);
            let pending = self.take_pending_compaction();
            drop(state);
            self.compact_at_boundary(&request, pending).await;
            state = self.state.lock().await;
            let mut step_call = call.clone();
            step_call.messages = Value::Array(state.record.conversation.clone());
            let drained = self.drain(step_call, &mut state, &turn_id).await;
            // The accounting row lands before anyone reads the outcome —
            // the source fires `onStepFinish` while the step is still
            // being processed, failed steps included.
            if let (Some(writer), Some(report)) = (usage_ledger.as_mut(), drained.usage.as_ref()) {
                // The turn's step index is the source's stepCount inside
                // one streamText run (steps already counts this step).
                let mut report = report.clone();
                report.attempt = steps as i64;
                let input = UsageInput {
                    session_id: self.session_id.clone(),
                    event_id: None,
                    run_kind: "chat",
                    workspace_root: request.workspace_root.clone(),
                    provider_id: request.provider_key.clone(),
                    model_id: request.model.clone(),
                    prices: self.usage_prices(&request.provider_key, &request.model),
                    report,
                };
                if let Err(error) = writer.record_usage(&input.row()) {
                    eprintln!("Usage accounting failed: {error}");
                }
            }
            if drained.outcome != TurnOutcome::Completed {
                break (drained.outcome, drained.reply);
            }
            if drained.calls.is_empty() {
                // Legacy order: the final text lands first (L2474),
                // then the admission check reopens the run (L2489-2496)
                // — a queued steer starts a new request in this same
                // turn instead of completing it.
                if !drained.reply.is_empty() {
                    state.record.conversation.push(json!({
                        "role": "assistant",
                        "content": [{ "type": "text", "text": drained.reply }]
                    }));
                    state.dirty = true;
                }
                if self.admit_steered(&mut state, &turn_id, &request) == 0 {
                    break (TurnOutcome::Completed, String::new());
                }
                continue;
            }
            if steps >= MAX_TOOL_STEPS {
                push_notice(
                    &mut state,
                    &turn_id,
                    "The tool call loop exceeded its step limit; this run has stopped.",
                    Some(NoticeTone::Error),
                );
                break (TurnOutcome::Failed, drained.reply);
            }

            // This step's assistant message: streamed text (when any) and
            // the calls it asked for, in the SDK's own shapes.
            let mut content: Vec<Value> = Vec::new();
            if !drained.reply.is_empty() {
                content.push(json!({ "type": "text", "text": drained.reply }));
            }
            for tool_call in &drained.calls {
                content.push(json!({
                    "type": "tool-call",
                    "toolCallId": tool_call.call_id,
                    "toolName": tool_call.name,
                    "input": tool_call.arguments,
                }));
            }
            state
                .record
                .conversation
                .push(json!({ "role": "assistant", "content": content }));
            state.dirty = true;

            let (mut ledger, mut ledger_error) = match LedgerWriter::open(&self.data_root) {
                Ok(writer) => (Some(writer), None),
                Err(error) => (None, Some(error.to_string())),
            };
            let mut stopped = false;
            let mut unknown_error: Option<String> = None;
            for tool_call in &drained.calls {
                let started_at = Utc::now().timestamp_millis();
                let (result, call_stopped) = if stopped {
                    // A prior decline refuses the rest of the batch (L237).
                    (stop_result(), false)
                } else if let Some(error) = ledger_error.take() {
                    // Fail closed like the source's failed write-ahead: the
                    // run stops and the sentence names the failure.
                    (proposal_failure(&error), true)
                } else if let Some(definition) = native_definition(
                    &tool_call.name,
                    &tool_workspace,
                    request.command_shell.as_deref(),
                ) {
                    match ToolCallContext::with_ask(
                        tool_call.arguments.clone(),
                        request.approval_mode,
                        tool_workspace.clone(),
                        crate::config::scratch_root(),
                        self.data_root.clone(),
                        self.session_id.clone(),
                        request.command_shell.clone(),
                        Some(Arc::new(self.ask_channel())),
                    ) {
                        // Invalid host metadata fails before the proposal —
                        // no ledger row, the pair still lands (timeout.ts).
                        Err(message) => (json!({ "status": "error", "error": message }), false),
                        Ok(context) => {
                            let approval = match assess(&definition, &context) {
                                GateDecision::AutoApproved => ApprovalOutcome::Approved,
                                GateDecision::NeedsApproval { gate_reason } => {
                                    let queue_request = ToolCallRequest {
                                        request_id: tool_call.call_id.clone(),
                                        provider_id: request.provider_key.clone(),
                                        name: tool_call.name.clone(),
                                        title: None,
                                        input: tool_call
                                            .arguments
                                            .as_object()
                                            .cloned()
                                            .unwrap_or_default(),
                                        effect: serde_json::from_str(definition.effect_json)
                                            .unwrap_or(ToolEffect {
                                                change: ToolChange::Unknown,
                                                data: ToolData::Unknown,
                                                network: ToolNetwork::Unknown,
                                                reversible: ToolReversible::Unknown,
                                                outbound: ToolOutbound::Unknown,
                                            }),
                                        timeout_seconds: Some(context.timeout_seconds),
                                        gate_reason: Some(gate_reason.to_owned()),
                                    };
                                    let outcome = self
                                        .request_manual_approval(
                                            &mut state,
                                            &turn_id,
                                            queue_request,
                                            gate_reason,
                                            request
                                                .approval_timeout_seconds
                                                .unwrap_or(DEFAULT_APPROVAL_TIMEOUT_SECONDS),
                                        )
                                        .await;
                                    if let ApprovalOutcome::TimedOut { seconds } = outcome {
                                        push_notice(
                                            &mut state,
                                            &turn_id,
                                            &format!(
                                                "No approval decision within {seconds} seconds; the action was not run."
                                            ),
                                            Some(NoticeTone::Warn),
                                        );
                                    }
                                    outcome
                                }
                            };
                            match ledger.as_mut() {
                                None => (proposal_failure("state.db is unavailable"), true),
                                Some(writer) => {
                                    let pending = run_tool_call(
                                        &definition,
                                        &context,
                                        &tool_call.call_id,
                                        None,
                                        approval,
                                        writer,
                                    );
                                    tokio::pin!(pending);
                                    // The tool may queue a question while the
                                    // turn holds `state`; the roster publish
                                    // rides this select — a re-entrant call
                                    // would deadlock on the state lock.
                                    let call_outcome = loop {
                                        tokio::select! {
                                            biased;
                                            outcome = &mut pending => break outcome,
                                            _ = self.publish_notify.notified() => {
                                                self.publish_now(&state, Some(&turn_id), None, true);
                                            }
                                        }
                                    };
                                    (call_outcome.result, call_outcome.run_stopped)
                                }
                            }
                        }
                    }
                } else {
                    let message = format!("Unknown tool: {}.", tool_call.name);
                    unknown_error = Some(message.clone());
                    (json!({ "status": "error", "error": message }), false)
                };
                if call_stopped {
                    stopped = true;
                }

                // The pair lands for every call — denial included — so the
                // next request is valid whatever happened (appendResponse).
                state.record.conversation.push(json!({
                    "role": "tool",
                    "content": [{
                        "type": "tool-result",
                        "toolCallId": tool_call.call_id,
                        "toolName": tool_call.name,
                        // ai v7's ModelMessage shape: the SDK reads
                        // `output.type` while converting (downloadAssets,
                        // ai/dist/index.js:1718); a bare `result` throws
                        // `Cannot read properties of undefined`.
                        "output": { "type": "json", "value": result.clone() },
                    }],
                }));
                let (_, text) = format_tool_result(&tool_call.name, &result);
                let text = if text.is_empty() {
                    "No output".to_owned()
                } else {
                    text
                };
                settle_tool_entry(
                    &mut state,
                    &turn_id,
                    &tool_call.name,
                    &tool_call.arguments,
                    &text,
                    started_at,
                    Utc::now().timestamp_millis() - started_at,
                );
                state.dirty = true;
            }
            // Persist the pairs before the next request — the admission
            // save's twin (a crash must not orphan a tool message).
            let _ = self.save(&mut state);
            if stopped {
                push_notice(
                    &mut state,
                    &turn_id,
                    "The tool call was denied; this run has stopped.",
                    Some(NoticeTone::Warn),
                );
                break (TurnOutcome::Completed, drained.reply);
            }
            if let Some(message) = unknown_error {
                // An unknown name is an authored failure (no source
                // counterpart): the model asked for something the catalog
                // never sent, so the run cannot continue productively.
                push_notice(&mut state, &turn_id, &message, Some(NoticeTone::Error));
                break (TurnOutcome::Failed, drained.reply);
            }
        };

        finish_user_entry(&mut state.record, &turn_id, outcome);
        // The admission current ends with the turn: a later manage
        // call sees no running task (and the next send re-sets it).
        *self
            .active_current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        *self
            .active_turn_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        if outcome == TurnOutcome::Completed && !final_reply.is_empty() {
            state.record.conversation.push(json!({
                "role": "assistant",
                "content": [{ "type": "text", "text": final_reply }]
            }));
        }
        state.dirty = true;
        self.save(&mut state)?;
        // The turn is over: the roster learns `busy: false` with the
        // settled record (the outcome stamp, the final assistant text).
        self.publish_now(&state, Some(&turn_id), None, false);

        Ok(TurnReport { turn_id, outcome })
    }

    /// Drains one door stream into the transcript: the step's outcome,
    /// its streamed reply, and the tool calls collected for the host to
    /// execute between steps.
    async fn drain(
        &self,
        call: openbutler_protocol::sidecar::SidecarCall,
        state: &mut SessionState,
        turn_id: &str,
    ) -> DrainResult {
        // When the provider call began — the source resets this in
        // `prepareStep`, so the row's duration covers the request.
        let started_at_ms = Utc::now().timestamp_millis();
        let mut stream = match self.door.call(Lane::Interactive, call).await {
            Ok(stream) => stream,
            Err(error) => {
                push_notice(state, turn_id, &error.to_string(), Some(NoticeTone::Error));
                return DrainResult {
                    outcome: TurnOutcome::Failed,
                    reply: String::new(),
                    calls: Vec::new(),
                    usage: None,
                };
            }
        };

        // Bookkeeping mirrors the legacy streamed-entry handles: the index
        // of the entry each stream grows, and when the thinking block
        // started (its duration is stamped when the block ends).
        let mut assistant_index: Option<usize> = None;
        let mut reasoning_index: Option<usize> = None;
        let mut reasoning_started_at: Option<i64> = None;
        let mut reply = String::new();
        let mut run_error: Option<String> = None;
        let mut cancelled = false;
        let mut calls: Vec<CollectedCall> = Vec::new();
        // Accounting: one row per finished step (legacy `onStepUsage`).
        let mut usage_tokens: Option<UsageTokens> = None;
        let mut usage_raw: Option<Value> = None;
        let mut provider_metadata: Option<Value> = None;
        let mut response_id: Option<String> = None;
        let mut finish_seen = false;
        let mut finish_reason: Option<String> = None;

        let mut persist = tokio::time::interval(Duration::from_secs(1));
        persist.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut stop_poll = tokio::time::interval(Duration::from_millis(50));
        stop_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                item = stream.next() => match item {
                    None => break,
                    Some(Ok(event)) => match event {
                        SidecarEvent::TextDelta { delta, .. } => {
                            // Assistant text ends the thinking block first —
                            // its duration is stamped at the block end.
                            finish_reasoning_block(
                                state,
                                &mut reasoning_index,
                                &mut reasoning_started_at,
                            );
                            grow_assistant(
                                state,
                                &mut assistant_index,
                                &delta,
                                turn_id,
                                &mut reply,
                            );
                            self.publish_now(state, Some(turn_id), assistant_index, true);
                        }
                        SidecarEvent::ReasoningDelta { delta, .. } => {
                            // A signature-only delta (empty text) is not
                            // transcript content; signatures travel back to
                            // the provider with the message, not the row.
                            if delta.is_empty() {
                                continue;
                            }
                            grow_reasoning(
                                state,
                                &mut reasoning_index,
                                &mut reasoning_started_at,
                                &mut assistant_index,
                                &delta,
                                Some(turn_id),
                                None,
                            );
                            self.publish_now(state, Some(turn_id), assistant_index, true);
                        }
                        SidecarEvent::ToolCall {
                            name,
                            arguments,
                            call_id,
                            ..
                        } => {
                            finish_reasoning_block(
                                state,
                                &mut reasoning_index,
                                &mut reasoning_started_at,
                            );
                            calls.push(CollectedCall {
                                call_id: call_id
                                    .filter(|id| !id.is_empty())
                                    .unwrap_or_else(|| Uuid::new_v4().to_string()),
                                name,
                                arguments,
                            });
                            self.publish_now(state, Some(turn_id), assistant_index, true);
                        }
                        SidecarEvent::Usage {
                            prompt_tokens,
                            completion_tokens,
                            reasoning_tokens,
                            total_tokens,
                            no_cache_tokens,
                            cache_read_tokens,
                            cache_write_tokens,
                            text_tokens,
                            raw,
                            ..
                        } => {
                            usage_raw = raw;
                            usage_tokens = Some(UsageTokens {
                                input_tokens: prompt_tokens.map(|value| value as i64),
                                output_tokens: completion_tokens.map(|value| value as i64),
                                reasoning_tokens: reasoning_tokens.map(|value| value as i64),
                                total_tokens: total_tokens.map(|value| value as i64),
                                no_cache_tokens: no_cache_tokens.map(|value| value as i64),
                                cache_read_tokens: cache_read_tokens.map(|value| value as i64),
                                cache_write_tokens: cache_write_tokens.map(|value| value as i64),
                                text_tokens: text_tokens.map(|value| value as i64),
                            });
                        }
                        SidecarEvent::Finish {
                            finish_reason: reason,
                            provider_metadata: metadata,
                            response_id: response,
                            ..
                        } => {
                            finish_seen = true;
                            finish_reason = reason;
                            provider_metadata = metadata;
                            response_id = response;
                            break;
                        }
                        SidecarEvent::Error { message, .. } => {
                            // The taxonomy always carries a message in
                            // practice; the fallback text is authored here
                            // (not ported) for the impossible empty case.
                            run_error = Some(
                                message.filter(|value| !value.is_empty()).unwrap_or_else(|| {
                                    "The model call failed.".to_owned()
                                }),
                            );
                            break;
                        }
                        SidecarEvent::Ready { .. } => {}
                    },
                    Some(Err(DoorError::Cancelled)) => {
                        cancelled = true;
                        break;
                    }
                    Some(Err(error)) => {
                        run_error = Some(error.to_string());
                        break;
                    }
                },
                _ = persist.tick() => {
                    if state.dirty {
                        // Best effort mid-stream: a failed periodic save must
                        // not end the turn (legacy sets sessionSaveError and
                        // reschedules instead).
                        let _ = self.save(state);
                    }
                }
                _ = stop_poll.tick() => {
                    if self.stop.swap(false, Ordering::SeqCst) {
                        stream.cancel().await;
                    }
                }
            }
        }

        // The thinking block may end at the stream's end (no later token).
        finish_reasoning_block(state, &mut reasoning_index, &mut reasoning_started_at);

        // One ledger row per finished step (legacy `onStepUsage`): a
        // cancelled run has no provider outcome to account for, and a
        // stream that reported neither a finish nor a structured error
        // produced no step result.
        let usage = if (finish_seen || run_error.is_some()) && !cancelled {
            let status = if finish_seen && finish_reason.as_deref() != Some("error") {
                "ok"
            } else {
                "error"
            };
            Some(StepUsageReport {
                started_at_ms,
                ended_at_ms: Utc::now().timestamp_millis(),
                // The caller replaces this with the turn's step index —
                // the source's stepCount inside one streamText run.
                attempt: 1,
                status,
                tokens: usage_tokens.unwrap_or_default(),
                usage_raw,
                provider_metadata,
                response_id,
            })
        } else {
            None
        };

        let outcome = if cancelled {
            push_notice(state, turn_id, "Run cancelled.", None);
            TurnOutcome::Cancelled
        } else if let Some(error) = run_error {
            push_notice(state, turn_id, &error, Some(NoticeTone::Error));
            TurnOutcome::Failed
        } else {
            TurnOutcome::Completed
        };
        // The step's notices and outcome land in one publish; the stream
        // is over, so nothing carries a streaming index any more.
        self.publish_now(state, Some(turn_id), None, true);
        DrainResult {
            outcome,
            reply,
            calls,
            usage,
        }
    }

    fn save(&self, state: &mut SessionState) -> Result<(), TurnError> {
        match save_session(&self.data_root, &state.record, false)? {
            SaveOutcome::Written(_) => {
                state.dirty = false;
                Ok(())
            }
            SaveOutcome::Skipped => Ok(()),
        }
    }

    /// Publishes the current state to the roster (c3): the first call
    /// seeds `last_published`, later calls fan out the diff. The runtime
    /// view comes from the host; until one is set (boot edge) there is
    /// no legal frame to send, so the publish is skipped, not faked.
    fn publish_now(
        &self,
        state: &SessionState,
        turn_id: Option<&str>,
        streaming_index: Option<usize>,
        busy: bool,
    ) {
        let Some(host) = self.host.upgrade() else {
            return;
        };
        let Some(runtime) = host.runtime_view() else {
            return;
        };
        // What the roster shows about the queue: the live request, if
        // one waits (legacy `pendingApproval`/`pendingApprovalId`).
        let queued = self
            .approval
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut next = PublishSnapshot {
            session: SessionView {
                meta: state.record.meta.clone(),
                entries: state.record.entries.clone(),
            },
            run: RunState {
                busy,
                running_turn_id: turn_id.map(str::to_owned),
                pending_approval: queued.as_ref().map(|pending| pending.request.clone()),
                pending_approval_id: queued.as_ref().map(|pending| pending.request_id.clone()),
                pending_question: self
                    .question
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .as_ref()
                    .map(|pending| openbutler_protocol::session::PendingQuestion {
                        id: pending.question_id.clone(),
                        question: pending.text.clone(),
                        options: pending.options.clone(),
                    }),
                queued_message_count: 0,
                first_queued_message: None,
                queued_messages: Vec::new(),
                session_save_error: None,
                streaming_assistant_index: streaming_index.map(|index| index as i64),
            },
        };
        fill_queue(&mut next.run, &self.scheduler);
        drop(queued);
        let mut last = self
            .last_published
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match last.take() {
            // The first state only seeds the baseline — an attaching
            // client answers with a snapshot of its own.
            None => *last = Some(next),
            Some(previous) => {
                host.fanout()
                    .publish(&self.session_id, &previous, &next, &runtime);
                *last = Some(next);
            }
        }
    }
}
