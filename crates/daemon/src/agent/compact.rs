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
use std::sync::Arc;

use chrono::Utc;
use serde_json::Value;

use openbutler_protocol::SessionEntryMeta;
use openbutler_protocol::entry::{CompactionStatus, NoticeTone, SessionEntry};
use openbutler_protocol::sidecar::SidecarProvider;

use super::context::{build_turn_system, resolve_turn_context};
use super::entry::push_scoped_notice;
use super::{SessionState, TurnRunner};
use crate::compaction::{
    CompactOutcome, CompactionRequest, CompactionResult, ImpossibleReason, compact_conversation,
};
use crate::state;
use crate::tool_runtime::catalog::{native_definition, sidecar_tool};

/// The deferral notice, byte-exact (L2745).
const QUEUED_NOTICE: &str =
    "Compaction is queued; it runs at the next step boundary or when the run finishes.";

/// The recovery notice written beside the running row (L2802).
const RECOVERY_NOTICE: &str = "Recovering compaction: earlier reasoning is omitted from this one request; dialogue and tool data are preserved. The full transcript remains archived.";

/// The empty-conversation notice (L2754).
const EMPTY_NOTICE: &str = "There is no conversation to compact.";

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

    /// One standalone compaction attempt.
    async fn compact_standalone(&self, recovery: bool) -> Result<(), String> {
        // Snapshot under the lock: the engine must never hold it across
        // the await (the door stream can take minutes).
        let (conversation, compaction_turn, meta) = {
            let state = self.state.lock().await;
            // The turn the transcript ends in (L2761): the last user
            // entry's id, whatever it is.
            let compaction_turn = state
                .record
                .entries
                .iter()
                .rev()
                .find_map(|entry| match entry {
                    SessionEntry::User { meta, .. } => Some(meta.turn_id.clone()),
                    _ => None,
                })
                .flatten();
            (
                state.record.conversation.clone(),
                compaction_turn,
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
                key: meta.provider_id.clone(),
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
            context_window: context.context_window,
            output_limit: context.output_limit.map(|value| value as i64),
            previous_context: meta.context_tokens,
            previous_context_estimate: meta.context_estimate_tokens,
            // Manual compaction: skip the trigger and the no-reduction
            // guard (source `force: true`).
            force: true,
            recovery,
        };

        // The row lands inside `on_start`: budget arithmetic passed, so
        // this is the first moment the stream can begin (L2793-2806).
        // The lock is brief and never spans the engine's await.
        let row_turn = compaction_turn.clone();
        let before = meta.context_tokens;
        let on_start = || {
            // The engine calls this synchronously while no turn runs,
            // so the runner's state lock is free by construction —
            // `try_lock` keeps the closure sync (a tokio guard cannot
            // be taken here), and a contended lock degrades to the
            // settle path appending its own row (source `else addEntry`).
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
                // Info stays unmarked and the standalone compaction has
                // no active turn, so no turn id (source `addEntry`).
                push_scoped_notice(&mut state, None, RECOVERY_NOTICE, None, None);
            }
            state.dirty = true;
            let _ = self.save(&mut state);
            self.publish_now(&state, None, None, true);
        };

        match compact_conversation(&self.door, request, Some(&on_start)).await {
            Ok(CompactOutcome::Compacted { result }) => {
                let mut state = self.state.lock().await;
                apply_compaction(&mut state, &result, compaction_turn);
                let _ = self.save(&mut state);
                self.publish_now(&state, None, None, false);
            }
            Ok(CompactOutcome::Impossible {
                reason,
                window,
                estimated_tokens,
                shortfall_tokens,
                cause,
            }) => {
                let mut state = self.state.lock().await;
                fail_running_compaction(&mut state);
                let notice = refusal_notice(
                    reason,
                    window,
                    estimated_tokens,
                    shortfall_tokens,
                    cause.as_deref(),
                    &meta.provider_id,
                    &meta.model_id,
                );
                push_scoped_notice(
                    &mut state,
                    None,
                    &notice,
                    Some(NoticeTone::Warn),
                    Some(&format!("compaction:{}", reason.as_str())),
                );
                let _ = self.save(&mut state);
                self.publish_now(&state, None, None, false);
            }
            Ok(CompactOutcome::Skipped { .. }) => {
                // Unreachable under `force: true`: both guard returns
                // fire before `on_start`, so no row exists and the
                // source falls through these outcomes silently too.
            }
            Err(error) => {
                let mut state = self.state.lock().await;
                fail_running_compaction(&mut state);
                push_scoped_notice(
                    &mut state,
                    None,
                    &format!("Compaction failed: {error}"),
                    Some(NoticeTone::Error),
                    None,
                );
                let _ = self.save(&mut state);
                self.publish_now(&state, None, None, false);
            }
        }
        Ok(())
    }
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
