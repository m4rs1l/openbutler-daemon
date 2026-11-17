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
//! - **No system prompt, no tools, no workspace yet**: the frozen system
//!   prompt, tool execution/second batch (c2) and workspace labels belong
//!   to their own slices and are recorded in `docs/agent-loop.md`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::Utc;
use openbutler_protocol::SessionEntryMeta;
use openbutler_protocol::entry::{NoticeTone, SessionEntry, TurnOutcome, UserEntrySource};
use openbutler_protocol::sidecar::SidecarEvent;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::sidecar::{DoorError, Lane, SidecarDoor, sidecar_call};
use crate::state::{self, SaveOutcome, SessionRecord, StateError, save_session};

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
    state: Mutex<SessionState>,
    stop: Arc<AtomicBool>,
}

impl TurnRunner {
    /// Opens an existing session record for turns (bounded, id-verified
    /// read). The door is shared: every session of the host rides one
    /// sidecar process.
    pub async fn open(
        data_root: &Path,
        session_id: &str,
        door: SidecarDoor,
    ) -> Result<TurnRunner, TurnError> {
        let path = state::record_path(data_root, session_id).ok_or(TurnError::NotFound)?;
        let record = state::read_session_record(&path, session_id).ok_or(TurnError::Unreadable)?;
        Ok(TurnRunner {
            door,
            data_root: data_root.to_owned(),
            state: Mutex::new(SessionState {
                record,
                dirty: false,
            }),
            stop: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Cancels the running turn (the owner's stop). A stop that arrives
    /// between turns is drained at the next admission.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Runs one turn to settlement: the user entry lands and is saved
    /// before the model is asked, the stream maps into transcript entries,
    /// and the user entry is stamped `completed`/`cancelled`/`failed`.
    pub async fn send(&self, request: TurnRequest) -> Result<TurnReport, TurnError> {
        let mut state = self.state.lock().await;
        self.stop.store(false, Ordering::SeqCst);
        let turn_id = Uuid::new_v4().to_string();
        let now = Utc::now().timestamp_millis();

        state.record.entries.push(SessionEntry::User {
            meta: SessionEntryMeta {
                at: Some(now),
                turn_id: Some(turn_id.clone()),
                ..SessionEntryMeta::default()
            },
            text: request.text.clone(),
            workspace_root: None,
            image_count: None,
            attachments: None,
            source: Some(request.source),
            event_id: None,
        });
        state.record.conversation.push(user_model_message(&request));
        state.dirty = true;
        // Admission save: the message on disk before any provider call.
        self.save(&mut state)?;

        let mut call = sidecar_call(&turn_id, &request.provider_key);
        call.provider.url = request.provider_url.clone();
        call.provider.api_key = request.provider_api_key.clone();
        call.model = request.model.clone();
        call.messages = Value::Array(state.record.conversation.clone());

        let outcome = self.drain(call, &mut state, &turn_id).await;

        finish_user_entry(&mut state.record, &turn_id, outcome);
        if outcome == TurnOutcome::Completed
            && let Some(reply) = merged_assistant_text(&state.record, &turn_id)
        {
            state.record.conversation.push(json!({
                "role": "assistant",
                "content": [{ "type": "text", "text": reply }]
            }));
        }
        state.dirty = true;
        self.save(&mut state)?;

        Ok(TurnReport { turn_id, outcome })
    }

    /// Drains one door stream into the transcript and returns the outcome.
    async fn drain(
        &self,
        call: openbutler_protocol::sidecar::SidecarCall,
        state: &mut SessionState,
        turn_id: &str,
    ) -> TurnOutcome {
        let mut stream = match self.door.call(Lane::Interactive, call).await {
            Ok(stream) => stream,
            Err(error) => {
                push_notice(state, turn_id, &error.to_string(), Some(NoticeTone::Error));
                return TurnOutcome::Failed;
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
                        }
                        SidecarEvent::ToolCall { name, arguments, .. } => {
                            finish_reasoning_block(
                                state,
                                &mut reasoning_index,
                                &mut reasoning_started_at,
                            );
                            push_tool_entry(state, turn_id, &name, &arguments);
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

        if cancelled {
            push_notice(state, turn_id, "Run cancelled.", None);
            TurnOutcome::Cancelled
        } else if let Some(error) = run_error {
            push_notice(state, turn_id, &error, Some(NoticeTone::Error));
            TurnOutcome::Failed
        } else {
            TurnOutcome::Completed
        }
    }

    fn save(&self, state: &mut SessionState) -> Result<(), TurnError> {
        match save_session(&self.data_root, &state.record, false)? {
            SaveOutcome::Written(_) => {
                state.dirty = false;
                Ok(())
            }
            // Unreachable after admission (the user entry anchors the
            // record), but a skip is not a failure.
            SaveOutcome::Skipped => Ok(()),
        }
    }
}

// --- transcript mapping -------------------------------------------------------

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
    let text = [source_label, request.text.as_str()]
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
    json!({ "role": "user", "content": content })
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

/// The settled tool entry: the call is recorded, and without the executor
/// (c2) it carries the legacy settle text for a call that never ran.
fn push_tool_entry(state: &mut SessionState, turn_id: &str, name: &str, arguments: &Value) {
    state.record.entries.push(SessionEntry::Tool {
        meta: SessionEntryMeta {
            at: Some(Utc::now().timestamp_millis()),
            turn_id: Some(turn_id.to_owned()),
            ..SessionEntryMeta::default()
        },
        command: None,
        input: arguments.as_object().cloned(),
        result: "The tool call was not executed.".to_owned(),
        running: None,
        preparing: None,
        tool: Some(name.to_owned()),
        tool_title: None,
        session: None,
        duration_ms: None,
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

/// The settled turn's model-visible reply — the merged assistant text of
/// this turn, if any.
fn merged_assistant_text(record: &SessionRecord, turn_id: &str) -> Option<String> {
    let mut reply = String::new();
    let mut found = false;
    for entry in &record.entries {
        if let SessionEntry::Assistant { meta, text } = entry
            && meta.turn_id.as_deref() == Some(turn_id)
        {
            reply.push_str(text);
            found = true;
        }
    }
    found.then_some(reply)
}
