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

pub mod context;
pub mod events;
pub mod executor;
pub mod prompt;
pub mod providers;
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

use crate::ledger::LedgerWriter;
use crate::sidecar::{DoorError, Lane, SidecarDoor, sidecar_call};
use crate::state::{self, SaveOutcome, SessionRecord, StateError, save_session};
use crate::tool_runtime::catalog::native_definition;
use crate::tool_runtime::format::format_tool_result;
use crate::tool_runtime::pipeline::{
    ApprovalOutcome, AskFuture, AskPort, GateDecision, ToolCallContext, assess, proposal_failure,
    run_tool_call, stop_result,
};
use events::PublishSnapshot;

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
pub struct QueuedImage {
    pub base64: String,
    pub media_type: String,
}

/// What a settled turn did — the same stamps `finishTurn` writes.
#[derive(Debug, Clone)]
pub struct TurnReport {
    pub turn_id: String,
    pub outcome: TurnOutcome,
}

/// Failures that stop a turn before or instead of a provider answer. A
/// provider failure is not an error value: it settles the turn `failed`
/// with a notice, exactly like the legacy `runError` path.
#[derive(Debug, thiserror::Error)]
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

/// One drained step: the outcome, the streamed reply, and the tool
/// calls collected for the host to execute between steps.
struct DrainResult {
    outcome: TurnOutcome,
    reply: String,
    calls: Vec<CollectedCall>,
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

    /// The pending approval for visibility surfaces (c3): its id — the
    /// `approvalId` clients answer — and the gate reason.
    pub fn pending_approval(&self) -> Option<(String, &'static str)> {
        self.approval
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(|pending| (pending.request_id.clone(), pending.gate_reason))
    }

    /// Queues one model question and waits for the owner's answer
    /// (`requestQuestion`, L1850) — the shared queue path the
    /// `question` tool's port also runs.
    pub async fn ask(&self, text: String, options: Option<Vec<String>>) -> Option<String> {
        self.ask_channel().queue_and_wait(text, options).await
    }

    /// The channel a `question` tool call holds: the shared queue, the
    /// stop flag, and the roster wakeup. Cloned into the tool's
    /// `'static` future, so the port never borrows the runner.
    fn ask_channel(&self) -> AskChannel {
        AskChannel {
            question: Arc::clone(&self.question),
            stop: Arc::clone(&self.stop),
            publish_notify: Arc::clone(&self.publish_notify),
        }
    }

    /// `session.answer`: id-guarded resolve (`answerQuestion`, L1224 —
    /// a stale id no-ops).
    pub fn answer_question(&self, question_id: &str, answer: Option<String>) -> bool {
        resolve_question(&self.question, question_id, answer)
    }

    /// The waiting question's id (visibility surfaces and tests).
    pub fn pending_question_id(&self) -> Option<String> {
        self.question
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(|pending| pending.question_id.clone())
    }

    /// `session.approve`: id-guarded resolve (`approveToolCall`, L1201 —
    /// a stale id no-ops), and the send happens under the same lock the
    /// timeout reads, so a decision can never be lost to the clock.
    pub fn resolve_approval(&self, approval_id: &str, approved: bool) -> bool {
        let mut slot = self
            .approval
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot
            .as_ref()
            .is_some_and(|pending| pending.request_id == approval_id)
            && let Some(pending) = slot.take()
        {
            let _ = pending.sender.send(approved);
            return true;
        }
        false
    }

    /// Queues one manual approval and waits out the decision:
    /// `allow`/`always` approve (the source's `decision !== 'deny'`), a
    /// decline declines, the owner's stop denies the pending approval
    /// (the source's stop handler, L1260 — and consumes the flag), and
    /// silence produces the timeout outcome the registry formats.
    async fn request_manual_approval(
        &self,
        state: &mut SessionState,
        turn_id: &str,
        request: openbutler_protocol::session::ToolCallRequest,
        gate_reason: &'static str,
        timeout_seconds: u64,
    ) -> ApprovalOutcome {
        let request_id = request.request_id.clone();
        let (sender, mut receiver) = tokio::sync::oneshot::channel();
        *self
            .approval
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(PendingApproval {
            request_id: request_id.clone(),
            request,
            gate_reason,
            sender,
        });
        // The queue becomes visible the moment it exists (the source's
        // `requestManualApproval` publishes before it resolves).
        self.publish_now(state, turn_id, None, true);
        let mut stop_poll = tokio::time::interval(Duration::from_millis(100));
        stop_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tokio::select! {
            // Biased: a decision landing in the same tick as the clock
            // wins — the client's answer is never discarded as a timeout.
            biased;
            approved = &mut receiver => {
                if approved.unwrap_or(false) {
                    ApprovalOutcome::Approved
                } else {
                    ApprovalOutcome::Declined
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(timeout_seconds)) => {
                self.approval
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take();
                ApprovalOutcome::TimedOut { seconds: timeout_seconds }
            }
            _ = stop_poll.tick(), if self.stop.load(Ordering::SeqCst) => {
                self.stop.store(false, Ordering::SeqCst);
                self.approval
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take();
                ApprovalOutcome::Declined
            }
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
            .session_workspace
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            state.record.meta.workspace_root.clone();
        push_user_entry(&mut state, &request, &turn_id);
        // Admission save: the message on disk before any provider call.
        self.save(&mut state)?;
        self.publish_now(&state, &turn_id, None, true);

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
        let mut steps = 0usize;
        let (outcome, final_reply) = loop {
            steps += 1;
            // The prepareStep admission (L2413): steered messages enter
            // before this request is built.
            self.admit_steered(&mut state, &turn_id, &request);
            let mut step_call = call.clone();
            step_call.messages = Value::Array(state.record.conversation.clone());
            let drained = self.drain(step_call, &mut state, &turn_id).await;
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
                                                self.publish_now(&state, &turn_id, None, true);
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
        self.publish_now(&state, &turn_id, None, false);

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
        let mut stream = match self.door.call(Lane::Interactive, call).await {
            Ok(stream) => stream,
            Err(error) => {
                push_notice(state, turn_id, &error.to_string(), Some(NoticeTone::Error));
                return DrainResult {
                    outcome: TurnOutcome::Failed,
                    reply: String::new(),
                    calls: Vec::new(),
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
                            self.publish_now(state, turn_id, assistant_index, true);
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
                                turn_id,
                            );
                            self.publish_now(state, turn_id, assistant_index, true);
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
                            self.publish_now(state, turn_id, assistant_index, true);
                        }
                        SidecarEvent::Usage { .. } => {}
                        SidecarEvent::Finish { .. } => break,
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
        self.publish_now(state, turn_id, None, true);
        DrainResult {
            outcome,
            reply,
            calls,
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
        turn_id: &str,
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
                running_turn_id: Some(turn_id.to_owned()),
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

// --- send queue ------------------------------------------------------------

impl TurnRunner {
    /// Queues a send and starts the drain when idle (legacy
    /// `enqueueMessage`: a busy run keeps it, an idle one runs it).
    pub fn enqueue(self: &Arc<Self>, request: TurnRequest) {
        let turn = queue::QueuedTurn {
            id: Uuid::new_v4().to_string(),
            steering: queue::steering_default(request.source),
            entry_added: false,
            request,
        };
        {
            let mut scheduler = self
                .scheduler
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            scheduler.turns.push_back(turn);
        }
        // The source publishes the moment a message joins a busy run
        // (L2130): the roster shows the queue before any pop can take
        // it back out.
        self.publish_queue_now();
        // The flip decides the duty: false→true starts the loop (a
        // live loop pops the new arrival itself).
        if !self.drain_active.swap(true, Ordering::SeqCst) {
            let runner = Arc::clone(self);
            tokio::spawn(async move {
                runner.drain_queue().await;
            });
        }
    }

    /// Runs queued sends one after another until the queue is empty;
    /// the flag flips under the same lock, so a push can never race the
    /// idle check (legacy `runQueuedMessages`, L2177).
    async fn drain_queue(self: Arc<Self>) {
        loop {
            // The queue only leaves the roster when its turn can
            // actually start: a running turn holds the session (the
            // source checks `busy` before shifting, L2178).
            while self
                .active_current
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let next = {
                let mut scheduler = self
                    .scheduler
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                match scheduler.turns.pop_front() {
                    Some(turn) => Some(turn),
                    None => {
                        // Duty ends under the queue lock, so an enqueue
                        // either lands before the pop (seen) or after
                        // the flip (starts the next loop).
                        self.drain_active.store(false, Ordering::SeqCst);
                        None
                    }
                }
            };
            let Some(turn) = next else {
                return;
            };
            if let Err(error) = self.send(turn.request).await {
                crate::agent::executor::report_turn_error(
                    &self.data_root,
                    &self.session_id,
                    &error.to_string(),
                );
            }
        }
    }

    /// Whether a turn currently owns the session (the `session.queue`
    /// busy check's input — exposed for tests and visibility
    /// surfaces).
    pub fn is_turn_active(&self) -> bool {
        self.active_current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }

    /// `session.queue`: steer or remove one queued message
    /// (`manageQueuedMessage`, L2041-2060 — the source's refusal
    /// sentences included).
    pub fn manage_queue(&self, message_id: &str, action: QueueAction) -> Result<(), String> {
        let mut scheduler = self
            .scheduler
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let busy = self
            .active_current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some();
        if action == QueueAction::Steer && !busy {
            return Err("There is no running task to steer.".to_owned());
        }
        let Some(index) = scheduler
            .turns
            .iter()
            .position(|turn| turn.id == message_id && !turn.entry_added)
        else {
            return Err("This message is no longer queued.".to_owned());
        };
        if action == QueueAction::Steer && scheduler.turns[index].steering {
            return Ok(());
        }
        if action == QueueAction::Steer {
            let current = self
                .active_current
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            if let Some(current) = current.as_ref() {
                let session_workspace = self
                    .session_workspace
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                let runtime_workspace = self
                    .host
                    .upgrade()
                    .and_then(|host| host.runtime_view())
                    .map(|view| view.workspace_root)
                    .unwrap_or_default();
                let refusal = queue::steering_refusal(
                    &scheduler.turns[index].request,
                    session_workspace.as_deref(),
                    current,
                    &runtime_workspace,
                );
                if !refusal.is_empty() {
                    return Err(refusal.to_owned());
                }
            }
        }
        let Some(turn) = scheduler.turns.remove(index) else {
            return Ok(());
        };
        if action == QueueAction::Steer {
            // Preserve the order of explicit steering while moving it
            // ahead of later work (L2055-2056).
            let turn = queue::QueuedTurn {
                steering: true,
                ..turn
            };
            let waiting = scheduler
                .turns
                .iter()
                .position(|queued| !queued.steering)
                .unwrap_or(scheduler.turns.len());
            scheduler.turns.insert(waiting, turn);
        }
        drop(scheduler);
        // The state lock lives inside a running turn; the queue's own
        // diff publishes from the last frame instead (the record has
        // not changed under a manage call).
        self.publish_queue_now();
        Ok(())
    }

    /// The prepareStep admission (L2413-2441): every steered message
    /// that passes the refusal joins the transcript before the next
    /// request is built. Incompatible steering is skipped, never a
    /// blocker for later eligible messages.
    fn admit_steered(
        &self,
        state: &mut SessionState,
        turn_id: &str,
        current: &TurnRequest,
    ) -> usize {
        let session_workspace = state.record.meta.workspace_root.clone();
        let runtime_workspace = self
            .host
            .upgrade()
            .and_then(|host| host.runtime_view())
            .map(|view| view.workspace_root)
            .unwrap_or_default();
        let mut admitted = 0;
        loop {
            let candidate = {
                let mut scheduler = self
                    .scheduler
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                scheduler
                    .turns
                    .iter()
                    .position(|turn| {
                        turn.steering
                            && !turn.entry_added
                            && queue::steering_refusal(
                                &turn.request,
                                session_workspace.as_deref(),
                                current,
                                &runtime_workspace,
                            )
                            .is_empty()
                    })
                    .and_then(|index| scheduler.turns.remove(index))
            };
            let Some(turn) = candidate else {
                break;
            };
            push_user_entry(state, &turn.request, turn_id);
            admitted += 1;
        }
        admitted
    }

    /// Publishes the queue's state from the last frame (the record has
    /// not changed, so only the run's queue fields move).
    fn publish_queue_now(&self) {
        let Some(host) = self.host.upgrade() else {
            return;
        };
        let Some(runtime) = host.runtime_view() else {
            return;
        };
        let previous = self
            .last_published
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(previous) = previous else {
            return;
        };
        let mut next = previous.clone();
        fill_queue(&mut next.run, &self.scheduler);
        host.fanout()
            .publish(&self.session_id, &previous, &next, &runtime);
        *self
            .last_published
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(next);
    }
}

/// Fills the run's queue fields from the live queue (the source's
/// `makeSnapshot`, L614-639: `!entryAdded` counts, the brief first
/// message, the full list).
fn fill_queue(
    run: &mut openbutler_protocol::session::RunState,
    scheduler: &std::sync::Mutex<Scheduler>,
) {
    let scheduler = scheduler
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let pending: Vec<&queue::QueuedTurn> = scheduler
        .turns
        .iter()
        .filter(|turn| !turn.entry_added)
        .collect();
    run.queued_message_count = u32::try_from(pending.len()).unwrap_or(u32::MAX);
    run.first_queued_message =
        pending
            .first()
            .map(|turn| openbutler_protocol::session::QueuedMessageBrief {
                text: turn.request.text.clone(),
                image_count: u32::try_from(turn.request.images.len()).unwrap_or(0),
            });
    run.queued_messages = pending
        .iter()
        .map(|turn| openbutler_protocol::session::QueuedMessage {
            id: Some(turn.id.clone()),
            text: turn.request.text.clone(),
            image_count: u32::try_from(turn.request.images.len()).unwrap_or(0),
            editable: Some(true),
            steering: Some(turn.steering),
        })
        .collect();
}

/// The user entry plus its model message (`addUserEntry`), shared by
/// turn admission and steering admission.
fn push_user_entry(state: &mut SessionState, request: &TurnRequest, turn_id: &str) {
    let now = Utc::now().timestamp_millis();
    state.record.entries.push(SessionEntry::User {
        meta: SessionEntryMeta {
            at: Some(now),
            turn_id: Some(turn_id.to_owned()),
            ..SessionEntryMeta::default()
        },
        text: request.text.clone(),
        workspace_root: request.workspace_root.clone(),
        image_count: {
            let images = request
                .attachments
                .iter()
                .filter(|attachment| attachment.media_type.starts_with("image/"))
                .count();
            u32::try_from(images).ok().filter(|count| *count > 0)
        },
        attachments: (!request.attachments.is_empty()).then(|| request.attachments.clone()),
        source: Some(request.source),
        event_id: None,
    });
    state.record.conversation.push(user_model_message(request));
    state.dirty = true;
}

// --- question channel --------------------------------------------------------

/// The question channel a `question` tool holds: the shared queue, the
/// stop flag, and the roster wakeup. `Clone` by `Arc`, so the tool's
/// future is `'static` (definitions are plain function pointers).
#[derive(Clone)]
pub(crate) struct AskChannel {
    question: Arc<std::sync::Mutex<Option<QueuedQuestion>>>,
    stop: Arc<AtomicBool>,
    publish_notify: Arc<tokio::sync::Notify>,
}

impl AskChannel {
    /// The queue path behind both the runner's `ask` and the tool port:
    /// queue → wake the turn's publish select → wait; a stop that wins
    /// the race clears the entry (the source's stop handler, L1259).
    async fn queue_and_wait(&self, text: String, options: Option<Vec<String>>) -> Option<String> {
        let question_id = format!(
            "question-{}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_millis())
                .unwrap_or(0),
            &Uuid::new_v4().to_string()[..6]
        );
        let (sender, receiver) = tokio::sync::oneshot::channel();
        *self
            .question
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(QueuedQuestion {
            question_id: question_id.clone(),
            text,
            options,
            sender,
        });
        // The roster frame rides the turn owner's select: this code runs
        // under the held `state` lock, where a re-entrant publish would
        // deadlock. The wakeup keeps one permit, so it is never lost.
        self.publish_notify.notify_one();
        let answer = wait_question(receiver, &self.stop).await;
        if answer.is_none() {
            // A stop that won the race: clear the queue entry so the
            // roster stops showing a question nobody can answer.
            let _ = resolve_question(&self.question, &question_id, None);
        }
        answer
    }
}

impl AskPort for AskChannel {
    fn ask(&self, text: String, options: Option<Vec<String>>) -> AskFuture {
        let channel = self.clone();
        Box::pin(async move { channel.queue_and_wait(text, options).await })
    }
}

/// Debug prints the channel, not the queue it holds (the oneshot
/// sender has no meaningful display).
impl std::fmt::Debug for AskChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AskChannel").finish_non_exhaustive()
    }
}

/// The id-guarded resolve both the seam and the dismissal path share
/// (`answerQuestion`, L1224 — a stale id no-ops).
fn resolve_question(
    queue: &std::sync::Mutex<Option<QueuedQuestion>>,
    question_id: &str,
    answer: Option<String>,
) -> bool {
    let mut slot = queue
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot
        .as_ref()
        .is_some_and(|pending| pending.question_id == question_id)
        && let Some(pending) = slot.take()
    {
        let _ = pending.sender.send(answer);
        return true;
    }
    false
}

// --- transcript mapping -------------------------------------------------------

/// Waits a queued question: the answer wins immediately, the owner's
/// stop flag returns `None` (and consumes it, like the approval wait).
async fn wait_question(
    mut receiver: tokio::sync::oneshot::Receiver<Option<String>>,
    stop: &Arc<AtomicBool>,
) -> Option<String> {
    loop {
        tokio::select! {
            biased;
            answer = &mut receiver => return answer.unwrap_or(None),
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                if stop.swap(false, Ordering::SeqCst) {
                    return None;
                }
            }
        }
    }
}

/// The model-visible user message: host-context labels in front of the raw
/// text, parts array (`queuedUserMessageToModelMessage`). Labels never enter
/// the transcript — only this message carries them.
fn user_model_message(request: &TurnRequest) -> Value {
    let source_label = match request.source {
        UserEntrySource::Telegram => "[Host context: owner message via Telegram]",
        UserEntrySource::Terminal => "[Host context: owner message via terminal]",
        UserEntrySource::Web => "[Host context: owner message via web]",
        UserEntrySource::Inbox
        | UserEntrySource::OwnerInbox
        | UserEntrySource::Team
        | UserEntrySource::Worker => "[Host context: inbox notification]",
        UserEntrySource::Schedule | UserEntrySource::Background => "",
    };
    // The label travels only to the model; newlines in a path would break
    // the line structure, so the source spaces them out.
    let workspace_label = match request.workspace_root.as_deref() {
        Some(root) => format!(
            "[Host context: active workspace {}]",
            root.replace(['\r', '\n'], " ")
        ),
        None => String::new(),
    };
    // A Telegram turn has to be answered in Telegram: text kept in this
    // session is invisible to the owner.
    let delivery_hint = match request.source {
        UserEntrySource::Telegram => {
            "[Host context: reply through send_message_to_owner; text written only in this session never reaches the owner]"
        }
        _ => "",
    };
    let text = [
        source_label,
        workspace_label.as_str(),
        delivery_hint,
        request.text.as_str(),
    ]
    .iter()
    .filter(|part| !part.is_empty())
    .copied()
    .collect::<Vec<_>>()
    .join("\n");
    let content = if text.is_empty() {
        Vec::new()
    } else {
        vec![json!({ "type": "text", "text": text })]
    };
    let mut content = content;
    if !request.attachments.is_empty() {
        content.push(json!({
            "type": "text",
            "text": attachment_references(&request.attachments)
        }));
    }
    json!({ "role": "user", "content": content })
}

/// `attachmentReferences`: `[image#N="path"]` / `[file#N="path"]`,
/// image and file counters kept apart, joined by newlines.
fn attachment_references(attachments: &[openbutler_protocol::entry::FileAttachment]) -> String {
    let mut images = 0usize;
    let mut files = 0usize;
    attachments
        .iter()
        .map(|attachment| {
            let label = if attachment.media_type.starts_with("image/") {
                images += 1;
                format!("image#{images}")
            } else {
                files += 1;
                format!("file#{files}")
            };
            let path = serde_json::to_string(&attachment.path).unwrap_or_else(|_| "\"\"".into());
            format!("[{label}={path}]")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `finishTurn`: stamp the turn's outcome onto the user entry that opened
/// it, in place — the row keeps its position and timing.
fn finish_user_entry(record: &mut SessionRecord, turn_id: &str, outcome: TurnOutcome) {
    let ended_at = Utc::now().timestamp_millis();
    for entry in &mut record.entries {
        if let SessionEntry::User { meta, .. } = entry
            && meta.turn_id.as_deref() == Some(turn_id)
        {
            meta.turn_outcome = Some(outcome);
            meta.turn_ended_at = Some(ended_at);
        }
    }
}

fn push_notice(state: &mut SessionState, turn_id: &str, text: &str, tone: Option<NoticeTone>) {
    state.record.entries.push(SessionEntry::Notice {
        meta: SessionEntryMeta {
            at: Some(Utc::now().timestamp_millis()),
            turn_id: Some(turn_id.to_owned()),
            ..SessionEntryMeta::default()
        },
        text: text.to_owned(),
        detail: None,
        tone,
        cause: None,
    });
    state.dirty = true;
}

/// The settled tool entry: written after execution with `running: false`,
/// the display text and the duration — `onToolResult` (L1576-1593). The
/// running-start row belongs to the streaming surface (c3); persistence
/// sees the settled state.
fn settle_tool_entry(
    state: &mut SessionState,
    turn_id: &str,
    name: &str,
    arguments: &Value,
    text: &str,
    at: i64,
    duration_ms: i64,
) {
    state.record.entries.push(SessionEntry::Tool {
        meta: SessionEntryMeta {
            at: Some(at),
            turn_id: Some(turn_id.to_owned()),
            ..SessionEntryMeta::default()
        },
        command: None,
        input: arguments.as_object().cloned(),
        result: text.to_owned(),
        running: Some(false),
        preparing: None,
        tool: Some(name.to_owned()),
        tool_title: None,
        session: None,
        duration_ms: Some(duration_ms),
        outcome: None,
    });
    state.dirty = true;
}

/// One streamed reply grows **one** assistant entry (`appendStreamedText`).
fn grow_assistant(
    state: &mut SessionState,
    assistant_index: &mut Option<usize>,
    delta: &str,
    turn_id: &str,
    reply: &mut String,
) {
    match *assistant_index {
        Some(index) => {
            if let SessionEntry::Assistant { text, .. } = &mut state.record.entries[index] {
                text.push_str(delta);
            }
        }
        None => {
            state.record.entries.push(SessionEntry::Assistant {
                meta: SessionEntryMeta {
                    at: Some(Utc::now().timestamp_millis()),
                    turn_id: Some(turn_id.to_owned()),
                    ..SessionEntryMeta::default()
                },
                text: delta.to_owned(),
            });
            *assistant_index = Some(state.record.entries.len() - 1);
        }
    }
    reply.push_str(delta);
    state.dirty = true;
}

/// Thinking grows one reasoning entry, spliced **before** the streamed
/// assistant entry when one exists (`appendStreamedReasoning`); otherwise it
/// appends at the end, and the assistant that follows lands after it.
fn grow_reasoning(
    state: &mut SessionState,
    reasoning_index: &mut Option<usize>,
    reasoning_started_at: &mut Option<i64>,
    assistant_index: &mut Option<usize>,
    delta: &str,
    turn_id: &str,
) {
    if let Some(index) = *reasoning_index {
        if let SessionEntry::Reasoning { text, .. } = &mut state.record.entries[index] {
            text.push_str(delta);
        }
        state.dirty = true;
        return;
    }
    let entry = SessionEntry::Reasoning {
        meta: SessionEntryMeta {
            at: Some(Utc::now().timestamp_millis()),
            turn_id: Some(turn_id.to_owned()),
            ..SessionEntryMeta::default()
        },
        text: delta.to_owned(),
        duration_ms: None,
    };
    let position = match *assistant_index {
        Some(index) => index,
        None => state.record.entries.len(),
    };
    state.record.entries.insert(position, entry);
    *reasoning_index = Some(position);
    *reasoning_started_at = Some(Utc::now().timestamp_millis());
    if let Some(assistant) = assistant_index {
        // The splice shifted the assistant one row down.
        *assistant += 1;
    }
    state.dirty = true;
}

/// The block ends at the next assistant token, a tool call, or the stream's
/// end: stamp `durationMs` and clear the handle (`finishStreamedReasoning`).
fn finish_reasoning_block(
    state: &mut SessionState,
    reasoning_index: &mut Option<usize>,
    reasoning_started_at: &mut Option<i64>,
) {
    let (Some(index), Some(started_at)) = (*reasoning_index, *reasoning_started_at) else {
        *reasoning_index = None;
        *reasoning_started_at = None;
        return;
    };
    if let SessionEntry::Reasoning { duration_ms, .. } = &mut state.record.entries[index]
        && duration_ms.is_none()
    {
        *duration_ms = Some((Utc::now().timestamp_millis() - started_at).max(0));
        state.dirty = true;
    }
    *reasoning_index = None;
    *reasoning_started_at = None;
}
