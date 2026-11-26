//! `compact()` (legacy `agent-session.ts` L2737-2856): the session
//! hooks around the engine — busy deferral, the running row, the
//! archive swap, and the refusal notices. The engine owns the budget
//! arithmetic and the stream; this module owns the transcript.
//!
//! Two source facts shape the code: the row lands from the first
//! moment the stream can begin (the reader sees "compacting" instead
//! of a silent pause), and every refusal is a notice with its exact
//! fix — never a dead end.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use serde_json::Value;

use openbutler_protocol::SessionEntryMeta;
use openbutler_protocol::entry::{CompactionStatus, EntryInternal, NoticeTone, SessionEntry};
use openbutler_protocol::sidecar::SidecarProvider;

use super::context::{build_turn_system, compaction_budget, resolve_turn_context};
use super::entry::{
    finish_reasoning_block, grow_compaction_text, grow_reasoning, push_scoped_notice,
};
use super::{SessionState, TurnRequest, TurnRunner};
use crate::compaction::{
    CompactOutcome, CompactionRequest, CompactionResult, EngineHooks, ImpossibleReason,
    compact_conversation,
};
use crate::ledger::{LedgerWriter, StepUsageReport, TokenPrices, UsageInput};
use crate::state;
use crate::tool_runtime::catalog::{native_definition, sidecar_tool};

/// The deferral notice, byte-exact (L2745).
const QUEUED_NOTICE: &str =
    "Compaction is queued; it runs at the next step boundary or when the run finishes.";

/// The recovery notice written beside the running row (L2802).
const RECOVERY_NOTICE: &str = "Recovering compaction: earlier reasoning is omitted from this one request; dialogue and tool data are preserved. The full transcript remains archived.";

/// The empty-conversation notice (L2754).
const EMPTY_NOTICE: &str = "There is no conversation to compact.";

/// The streamed-entry cells shared by the engine hooks: one entry
/// grows for the whole summarisation with the compaction provenance
/// (the source's `streamedAssistantEntry` + the `internal` merge
/// guard — here separate index cells make the guard structural).
/// Std mutexes: every section is short and never spans an await.
#[derive(Default)]
struct CompactionStream {
    assistant: std::sync::Mutex<Option<usize>>,
    reasoning: std::sync::Mutex<Option<usize>>,
    reasoning_started_at: std::sync::Mutex<Option<i64>>,
}

impl CompactionStream {
    /// The thought block ends at settle (source `flushStreamedEntries`
    /// → `finishStreamedReasoning`): stamp the duration once.
    fn finish_reasoning(&self, state: &mut SessionState) {
        let mut index = self
            .reasoning
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut started = self
            .reasoning_started_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        finish_reasoning_block(state, &mut index, &mut started);
    }
}

/// What both paths share when settling: whose transcript the row and
/// the notices belong to, which provider/model the refusal notice
/// names, the run state the publish carries, and the stream cells to
/// close.
struct SettleContext<'a> {
    row_turn: Option<String>,
    notice_turn: Option<&'a str>,
    provider_id: &'a str,
    model_id: &'a str,
    busy: bool,
    stream: &'a CompactionStream,
}

impl TurnRunner {
    /// `session.compact` on the runner: a busy run defers the request
    /// with the source's notice, an empty conversation notices, and
    /// otherwise the engine runs now against the record. Setup failures
    /// return `Err` for the executor to notice (`Could not compact the
    /// session: …`, L3518); engine failures are notices here, like the
    /// source's inner catch (`Compaction failed: …`).
    pub async fn compact(self: Arc<Self>, recovery: bool) -> Result<(), String> {
        // The queue is stored under the `active_current` lock: a store
        // that observed a busy run happens-before the run's clear, so
        // the drain's take (after the clear) can never miss it — the
        // TOCTOU window between a separate check and store would
        // orphan the request after the run ends.
        let deferred = {
            let active = self
                .active_current
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if active.is_some() {
                *self
                    .pending_compaction
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(recovery);
                true
            } else {
                false
            }
        };
        if deferred {
            self.defer_compaction().await;
            return Ok(());
        }
        self.compact_standalone(recovery).await
    }

    /// The queued compaction, taken by the run that can now serve it
    /// (one-shot; the store and this take share one lock).
    pub(super) fn take_pending_compaction(&self) -> Option<bool> {
        self.pending_compaction
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    /// The deferral: the source's `addNotice` (info stays unmarked)
    /// stamped with the running turn, then a publish — the roster
    /// learns the queue at once (L2745-2746).
    async fn defer_compaction(&self) {
        let active_turn = self
            .active_turn_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let mut state = self.state.lock().await;
        push_scoped_notice(
            &mut state,
            active_turn.as_deref(),
            QUEUED_NOTICE,
            None,
            None,
        );
        let _ = self.save(&mut state);
        self.publish_now(&state, active_turn.as_deref(), None, true);
    }

    /// One standalone compaction attempt (the arm path: settings and
    /// the prompt rebuilt here — the source builds them too when no
    /// run supplies them).
    async fn compact_standalone(&self, recovery: bool) -> Result<(), String> {
        // Snapshot under the lock: the engine must never hold it across
        // the await (the door stream can take minutes).
        let (conversation, compaction_turn, meta) = {
            let state = self.state.lock().await;
            (
                state.record.conversation.clone(),
                last_transcript_turn(&state),
                state.record.meta.clone(),
            )
        };
        if conversation.is_empty() {
            // No active turn here, so the notice carries no turn id —
            // `addEntry` stamps `activeTurnId`, which is undefined.
            let mut state = self.state.lock().await;
            push_scoped_notice(&mut state, None, EMPTY_NOTICE, None, None);
            let _ = self.save(&mut state);
            self.publish_now(&state, None, None, false);
            return Ok(());
        }

        // The settings half: the same resolution the turn uses (one
        // read authority) and the shared prompt assembly.
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| "the host has dropped".to_owned())?;
        let context = resolve_turn_context(&host, &self.data_root, &meta)?;
        let (window, output) = compaction_budget(&host, &meta);
        let system = build_turn_system(
            &self.data_root,
            context.workspace_root.as_deref(),
            None,
            context.team.clone(),
        )?
        .ok_or_else(|| "the agent system prompt is missing".to_owned())?;
        let tools = compaction_tools(
            &self.data_root,
            context.workspace_root.as_deref(),
            context.command_shell.as_deref(),
        );
        let request = CompactionRequest {
            call_tag: "compact".to_owned(),
            provider: SidecarProvider {
                key: context.provider_key,
                url: context.provider_url,
                api_key: context.provider_api_key,
            },
            model: meta.model_id.clone(),
            // Reasoning options are not ported yet (documented in
            // agent-loop.md): the request rides without them.
            provider_options: None,
            system,
            messages: conversation,
            transcript: None,
            tools,
            archive_path: state::record_path(&self.data_root, &self.session_id)
                .map(|path| path.display().to_string()),
            context_window: window,
            output_limit: output,
            previous_context: meta.context_tokens,
            previous_context_estimate: meta.context_estimate_tokens,
            // Manual compaction: skip the trigger and the no-reduction
            // guard (source `force: true`).
            force: true,
            recovery,
        };

        let on_start =
            self.on_start_hook(compaction_turn.clone(), None, meta.context_tokens, recovery);
        // No active turn here: the streamed entry carries no turn id
        // (source addEntry stamps activeTurnId, undefined).
        let stream = CompactionStream::default();
        let entry_turn: Option<String> = None;
        let text_hook = self.stream_text_hook(&stream, &entry_turn);
        let reasoning_hook = self.stream_reasoning_hook(&stream, &entry_turn);
        // One ledger row per summarisation request (`onStepUsage` →
        // `recordUsage`): best effort — a failed write logs and the
        // compaction continues, exactly as the source's sink does
        // (`if (!sink) return` when the writer cannot open).
        let usage_ledger = Mutex::new(LedgerWriter::open(&self.data_root).ok());
        let usage_session = self.session_id.clone();
        let usage_workspace = meta.workspace_root.clone();
        let usage_provider = meta.provider_id.clone();
        let usage_model = meta.model_id.clone();
        let usage_prices = self.usage_prices(&usage_provider, &usage_model);
        let on_usage = {
            let usage_ledger = &usage_ledger;
            move |report: &StepUsageReport| {
                let input = UsageInput {
                    session_id: usage_session.clone(),
                    event_id: None,
                    run_kind: "compaction",
                    workspace_root: usage_workspace.clone(),
                    provider_id: usage_provider.clone(),
                    model_id: usage_model.clone(),
                    prices: usage_prices.clone(),
                    report: report.clone(),
                };
                let mut guard = usage_ledger
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(writer) = guard.as_mut()
                    && let Err(error) = writer.record_usage(&input.row())
                {
                    eprintln!("Usage accounting failed: {error}");
                }
            }
        };
        let hooks = EngineHooks {
            on_start: Some(&on_start),
            on_text: Some(&text_hook),
            on_reasoning: Some(&reasoning_hook),
            on_usage: Some(&on_usage),
        };
        match compact_conversation(&self.door, request, hooks).await {
            Ok(outcome) => {
                self.settle_outcome(
                    outcome,
                    SettleContext {
                        row_turn: compaction_turn,
                        notice_turn: None,
                        provider_id: &meta.provider_id,
                        model_id: &meta.model_id,
                        busy: false,
                        stream: &stream,
                    },
                )
                .await;
            }
            Err(error) => {
                self.fail_compaction(
                    None,
                    format!("Compaction failed: {error}"),
                    NoticeTone::Error,
                    false,
                    &stream,
                )
                .await;
            }
        }
        Ok(())
    }

    /// The step-boundary compaction (agent-session L2327, agent.ts
    /// L426): every model request asks once — forced when a queued
    /// request waited, automatic otherwise (the engine skips below its
    /// threshold with zero calls). The caller has released the state
    /// guard, so the row hook's `try_lock` is free by construction.
    /// The run's own context decides the request (the source passes
    /// the step context straight in). Every failure is a notice here:
    /// a summarisation must never cost the turn (source catch
    /// L2394-2401 — the warn text with the continue suffix).
    /// The catalog's real per-million prices for this model (owner rule
    /// 2026-11-24: an estimate comes from real prices or not at all).
    /// `None` without a boot-installed catalog or for an unpriceable
    /// model — the row then keeps `cost_source: none`.
    pub(super) fn usage_prices(&self, provider_id: &str, model_id: &str) -> Option<TokenPrices> {
        let catalog = self.host.upgrade()?.catalog()?;
        let cost = catalog.get_token_price(provider_id, model_id)?;
        Some(TokenPrices {
            input: cost.input,
            output: cost.output,
            reasoning: cost.reasoning,
            cache_read: cost.cache_read,
            cache_write: cost.cache_write,
        })
    }

    pub(super) async fn compact_at_boundary(&self, run: &TurnRequest, pending: Option<bool>) {
        let (conversation, compaction_turn, active_turn, meta) = {
            let state = self.state.lock().await;
            let active_turn = self
                .active_turn_id
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            (
                state.record.conversation.clone(),
                last_transcript_turn(&state),
                active_turn,
                state.record.meta.clone(),
            )
        };
        let (window, output) = self
            .host
            .upgrade()
            .map(|host| compaction_budget(&host, &meta))
            .unwrap_or((meta.context_window_tokens, None));
        let recovery = pending.unwrap_or(false);
        let request = CompactionRequest {
            call_tag: "compact".to_owned(),
            provider: SidecarProvider {
                key: run.provider_key.clone(),
                url: run.provider_url.clone(),
                api_key: run.provider_api_key.clone(),
            },
            model: run.model.clone(),
            provider_options: None,
            // The run's own system: the request about to go out and
            // the summarisation share the prefix.
            system: run.system.clone().unwrap_or_default(),
            messages: conversation,
            transcript: None,
            tools: compaction_tools(
                &self.data_root,
                run.workspace_root.as_deref(),
                run.command_shell.as_deref(),
            ),
            archive_path: state::record_path(&self.data_root, &self.session_id)
                .map(|path| path.display().to_string()),
            context_window: window,
            output_limit: output,
            previous_context: meta.context_tokens,
            previous_context_estimate: meta.context_estimate_tokens,
            // Queued request → forced; the automatic half lets the
            // engine's own trigger decide.
            force: pending.is_some(),
            recovery,
        };

        // The row names the run's turn while a run is live (source
        // `compactionTurn()`: activeTurnId ?? compactionTurnId).
        let row_turn = active_turn.clone().or(compaction_turn);
        let on_start = self.on_start_hook(
            row_turn.clone(),
            active_turn.clone(),
            meta.context_tokens,
            recovery,
        );
        let notice_turn = active_turn.as_deref();
        let stream = CompactionStream::default();
        let entry_turn = active_turn.clone();
        let text_hook = self.stream_text_hook(&stream, &entry_turn);
        let reasoning_hook = self.stream_reasoning_hook(&stream, &entry_turn);
        // One ledger row per summarisation request (`onStepUsage` →
        // `recordUsage`): best effort — a failed write logs and the
        // compaction continues, exactly as the source's sink does
        // (`if (!sink) return` when the writer cannot open).
        let usage_ledger = Mutex::new(LedgerWriter::open(&self.data_root).ok());
        let usage_session = self.session_id.clone();
        let usage_workspace = meta.workspace_root.clone();
        let usage_provider = meta.provider_id.clone();
        let usage_model = meta.model_id.clone();
        let usage_prices = self.usage_prices(&usage_provider, &usage_model);
        let on_usage = {
            let usage_ledger = &usage_ledger;
            move |report: &StepUsageReport| {
                let input = UsageInput {
                    session_id: usage_session.clone(),
                    event_id: None,
                    run_kind: "compaction",
                    workspace_root: usage_workspace.clone(),
                    provider_id: usage_provider.clone(),
                    model_id: usage_model.clone(),
                    prices: usage_prices.clone(),
                    report: report.clone(),
                };
                let mut guard = usage_ledger
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(writer) = guard.as_mut()
                    && let Err(error) = writer.record_usage(&input.row())
                {
                    eprintln!("Usage accounting failed: {error}");
                }
            }
        };
        let hooks = EngineHooks {
            on_start: Some(&on_start),
            on_text: Some(&text_hook),
            on_reasoning: Some(&reasoning_hook),
            on_usage: Some(&on_usage),
        };
        match compact_conversation(&self.door, request, hooks).await {
            Ok(outcome) => {
                self.settle_outcome(
                    outcome,
                    SettleContext {
                        row_turn,
                        notice_turn,
                        provider_id: &meta.provider_id,
                        model_id: &meta.model_id,
                        busy: true,
                        stream: &stream,
                    },
                )
                .await;
            }
            Err(error) => {
                self.fail_compaction(
                    notice_turn,
                    format!(
                        "Compaction failed: {error} The turn continues with the original context."
                    ),
                    NoticeTone::Warn,
                    true,
                    &stream,
                )
                .await;
            }
        }
    }

    /// The row hook shared by both paths: the row lands from the
    /// engine's `on_start` (after the budget arithmetic), naming the
    /// turn; the recovery notice rides beside it.
    fn on_start_hook(
        &self,
        row_turn: Option<String>,
        notice_turn: Option<String>,
        before: Option<i64>,
        recovery: bool,
    ) -> impl Fn() + Send + Sync + '_ {
        move || {
            // Sync hook over the async state lock: both callers have
            // released the guard before the engine runs, so the lock
            // is free by construction; `try_lock` keeps the hook sync
            // (a tokio guard cannot be taken here) and a contended
            // lock degrades to the settle path appending its own row
            // (source `else addEntry`).
            let Ok(mut state) = self.state.try_lock() else {
                return;
            };
            state.record.entries.push(SessionEntry::Compaction {
                meta: SessionEntryMeta {
                    at: Some(Utc::now().timestamp_millis()),
                    turn_id: row_turn.clone(),
                    ..SessionEntryMeta::default()
                },
                status: CompactionStatus::Running,
                before_tokens: before,
                after_tokens: None,
                duration_ms: None,
            });
            if recovery {
                push_scoped_notice(
                    &mut state,
                    notice_turn.as_deref(),
                    RECOVERY_NOTICE,
                    None,
                    None,
                );
            }
            state.dirty = true;
            let _ = self.save(&mut state);
            self.publish_now(&state, notice_turn.as_deref(), None, true);
        }
    }

    /// The streamed-text hook: one entry grows with the compaction
    /// provenance while the engine streams (source
    /// `queueStreamDelta('assistant', …, 'compaction')` — the empty
    /// guard and the per-delta publish included).
    fn stream_text_hook<'a>(
        &'a self,
        stream: &'a CompactionStream,
        entry_turn: &'a Option<String>,
    ) -> impl Fn(&str) + Send + Sync + 'a {
        move |delta: &str| {
            if delta.is_empty() {
                return;
            }
            let Ok(mut state) = self.state.try_lock() else {
                return;
            };
            let mut index = stream
                .assistant
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            grow_compaction_text(&mut state, &mut index, delta, entry_turn.as_deref());
            let live = *index;
            drop(index);
            self.publish_now(&state, entry_turn.as_deref(), live, true);
        }
    }

    /// The streamed-thought hook (source
    /// `queueStreamDelta('reasoning', …, 'compaction')`), spliced
    /// before the streamed assistant entry.
    fn stream_reasoning_hook<'a>(
        &'a self,
        stream: &'a CompactionStream,
        entry_turn: &'a Option<String>,
    ) -> impl Fn(&str) + Send + Sync + 'a {
        move |delta: &str| {
            if delta.is_empty() {
                return;
            }
            let Ok(mut state) = self.state.try_lock() else {
                return;
            };
            let mut reasoning = stream
                .reasoning
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut started = stream
                .reasoning_started_at
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut assistant = stream
                .assistant
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            grow_reasoning(
                &mut state,
                &mut reasoning,
                &mut started,
                &mut assistant,
                delta,
                entry_turn.as_deref(),
                Some(EntryInternal::Compaction),
            );
            let live = *assistant;
            drop(reasoning);
            drop(started);
            drop(assistant);
            self.publish_now(&state, entry_turn.as_deref(), live, true);
        }
    }

    /// The outcome arms shared by both paths (source identical at
    /// L2818 and L2374): compacted swaps, impossible refuses as data,
    /// skipped stays silent.
    async fn settle_outcome(&self, outcome: CompactOutcome, ctx: SettleContext<'_>) {
        let SettleContext {
            row_turn,
            notice_turn,
            provider_id,
            model_id,
            busy,
            stream,
        } = ctx;
        match outcome {
            CompactOutcome::Compacted { result } => {
                let mut state = self.state.lock().await;
                stream.finish_reasoning(&mut state);
                apply_compaction(&mut state, &result, row_turn);
                let _ = self.save(&mut state);
                self.publish_now(&state, notice_turn, None, busy);
            }
            CompactOutcome::Impossible {
                reason,
                window,
                estimated_tokens,
                shortfall_tokens,
                cause,
            } => {
                let mut state = self.state.lock().await;
                stream.finish_reasoning(&mut state);
                fail_running_compaction(&mut state);
                let notice = refusal_notice(
                    reason,
                    window,
                    estimated_tokens,
                    shortfall_tokens,
                    cause.as_deref(),
                    provider_id,
                    model_id,
                );
                push_scoped_notice(
                    &mut state,
                    notice_turn,
                    &notice,
                    Some(NoticeTone::Warn),
                    Some(&format!("compaction:{}", reason.as_str())),
                );
                let _ = self.save(&mut state);
                self.publish_now(&state, notice_turn, None, busy);
            }
            CompactOutcome::Skipped { .. } => {
                // The guards return before `on_start` (an automatic
                // boundary below its threshold lands here): no row, no
                // notice — the source falls through silently too.
            }
        }
    }

    /// The failure arm shared by both paths: the row (if any) fails
    /// in place and the caller's text lands as a notice.
    async fn fail_compaction(
        &self,
        notice_turn: Option<&str>,
        text: String,
        tone: NoticeTone,
        busy: bool,
        stream: &CompactionStream,
    ) {
        let mut state = self.state.lock().await;
        stream.finish_reasoning(&mut state);
        fail_running_compaction(&mut state);
        push_scoped_notice(&mut state, notice_turn, &text, Some(tone), None);
        let _ = self.save(&mut state);
        self.publish_now(&state, notice_turn, None, busy);
    }
}

/// The turn the transcript ends in (source `lastTranscriptTurnId`,
/// L1996): the last user entry's id, whatever it is.
fn last_transcript_turn(state: &SessionState) -> Option<String> {
    state
        .record
        .entries
        .iter()
        .rev()
        .find_map(|entry| match entry {
            SessionEntry::User { meta, .. } => Some(meta.turn_id.clone()),
            _ => None,
        })
        .flatten()
}

/// The rewrite: archive once, swap the conversation, kill the measured
/// prefix, and settle the row in place (source `applyCompaction`,
/// L2849-2883).
fn apply_compaction(
    state: &mut SessionState,
    result: &CompactionResult,
    compaction_turn: Option<String>,
) {
    if state.record.conversation_archive.is_none() {
        state.record.conversation_archive = Some(state.record.conversation.clone());
    }
    state.record.conversation = result.messages.clone();
    state.record.meta.context_tokens = None;
    state.record.meta.context_estimate_tokens = None;
    // One event that changed state: the running row is replaced, not
    // followed by an unrelated notice (L2874-2881).
    let finished = SessionEntry::Compaction {
        meta: SessionEntryMeta {
            // No `at`: the replacement keeps the running row's origin
            // (source `replaceEntry`).
            at: None,
            turn_id: compaction_turn,
            ..SessionEntryMeta::default()
        },
        status: CompactionStatus::Done,
        before_tokens: Some(result.before_tokens),
        after_tokens: Some(result.after_tokens),
        duration_ms: None,
    };
    settle_running_compaction(state, finished);
}

/// The running row becomes `failed` in place, keeping its start size;
/// a later attempt appends its own row (source `failRunningCompaction`,
/// L1011-1022).
fn fail_running_compaction(state: &mut SessionState) {
    let Some(index) = state.record.entries.iter().rposition(|entry| {
        matches!(
            entry,
            SessionEntry::Compaction {
                status: CompactionStatus::Running,
                ..
            }
        )
    }) else {
        return;
    };
    let (at, turn_id, before_tokens) = match &state.record.entries[index] {
        SessionEntry::Compaction {
            meta,
            before_tokens,
            ..
        } => (meta.at, meta.turn_id.clone(), *before_tokens),
        _ => unreachable!("rposition matched a compaction row"),
    };
    state.record.entries[index] = SessionEntry::Compaction {
        meta: SessionEntryMeta {
            at,
            turn_id,
            ..SessionEntryMeta::default()
        },
        status: CompactionStatus::Failed,
        before_tokens,
        after_tokens: None,
        duration_ms: None,
    };
    state.dirty = true;
}

/// Replace the last running row in place, keeping its origin (`at` and
/// `turnId` follow the previous row when the next shape lacks them —
/// source `replaceEntry`, L754-764); with no running row the finished
/// shape appends (source's `else addEntry`).
fn settle_running_compaction(state: &mut SessionState, mut next: SessionEntry) {
    let Some(index) = state.record.entries.iter().rposition(|entry| {
        matches!(
            entry,
            SessionEntry::Compaction {
                status: CompactionStatus::Running,
                ..
            }
        )
    }) else {
        state.record.entries.push(next);
        state.dirty = true;
        return;
    };
    if let SessionEntry::Compaction { meta: previous, .. } = &state.record.entries[index] {
        let (previous_at, previous_turn) = (previous.at, previous.turn_id.clone());
        if let SessionEntry::Compaction { meta, .. } = &mut next {
            if meta.at.is_none() {
                meta.at = previous_at;
            }
            if meta.turn_id.is_none() {
                meta.turn_id = previous_turn;
            }
        }
    }
    state.record.entries[index] = next;
    state.dirty = true;
}

/// An actionable transcript line for a refusal — numbers and the exact
/// fix, never a dead end (source `compactionRefusalNotice`, L1024-1037).
/// A `null` window renders as JS template it: `null`, not `None`.
fn refusal_notice(
    reason: ImpossibleReason,
    window: Option<i64>,
    estimated_tokens: i64,
    shortfall_tokens: i64,
    cause: Option<&str>,
    provider_id: &str,
    model_id: &str,
) -> String {
    if reason == ImpossibleReason::NoReduction {
        return "Compaction was skipped: the checkpoint would not shrink the active context, so it was not applied. The turn continues with the original context.".to_owned();
    }
    let window_display = window
        .map(|value| value.to_string())
        .unwrap_or_else(|| "null".to_owned());
    let mut notice = format!(
        "Compaction does not fit the budget: the conversation needs about {estimated_tokens} tokens but the configured window is {window_display} tokens, so the summarization request itself would overflow by {shortfall_tokens} tokens. "
    );
    notice.push_str("Raise providers.");
    notice.push_str(provider_id);
    notice.push_str(".modelSpecs.");
    notice.push_str(model_id);
    notice.push_str(
        ".context in settings above the conversation size, or remove it to use the catalog value, or start a new chat with /new. ",
    );
    if let Some(cause) = cause {
        notice.push_str(&format!(
            "The sliced fallback tried first and stopped: {cause}. "
        ));
    }
    notice.push_str("The turn continues with the original context.");
    notice
}

/// The schemas the summariser sees: the same catalog the turn sends —
/// the engine strips executors for every estimate and sends schemas
/// only (source `createModelTools` with a deny-all approve, L2780).
fn compaction_tools(
    data_root: &std::path::Path,
    workspace_root: Option<&str>,
    command_shell: Option<&str>,
) -> Value {
    // The turn's own fallback: its data root (agent/mod.rs L336-339).
    let workspace = workspace_root
        .map(PathBuf::from)
        .unwrap_or_else(|| data_root.to_path_buf());
    let mut tools = serde_json::Map::new();
    for name in ["execute_command", "question"] {
        let Some(definition) = native_definition(name, &workspace, command_shell) else {
            continue;
        };
        let Ok(tool) = sidecar_tool(&definition) else {
            continue;
        };
        let mut shape = serde_json::Map::new();
        if let Some(description) = tool.description {
            shape.insert("description".to_owned(), Value::String(description));
        }
        shape.insert("parameters".to_owned(), tool.parameters);
        tools.insert(tool.name, Value::Object(shape));
    }
    Value::Object(tools)
}
