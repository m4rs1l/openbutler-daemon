//! The send queue: what steers by default, why a queued message may
//! (or may not) join a live run, and the queue's unit type.
//!
//! Sources: legacy `agent-session.ts` ÔÇö `QueuedTurn` (L2114),
//! `enqueueMessage` (L2062-2136), `manageQueuedMessage` (L2041-2060),
//! `steeringRefusal` (L2022-2035), admission (L2413-2441). The project
//! clause of `steeringRefusal` is absent: this build carries no project
//! binding to compare (documented in agent-loop.md).
use super::entry::user_model_message;
use super::*;

use openbutler_protocol::entry::UserEntrySource;

use crate::agent::TurnRequest;

/// The `queueAction` of `session.queue`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueAction {
    Steer,
    Remove,
}

/// One queued send awaiting its turn (legacy `QueuedTurn`, L2114 ÔÇö the
/// promise fields have no counterpart: sends are fire-and-forget).
#[derive(Debug)]
pub(crate) struct QueuedTurn {
    pub id: String,
    pub request: TurnRequest,
    /// A held message whose entry already sits in the transcript ÔÇö the
    /// roster's `!entryAdded` filter keeps it from counting as queued.
    /// Owner sends never set it in this build; the flag keeps the
    /// snapshot rule intact.
    pub entry_added: bool,
    /// A live-run admission candidate: steered on arrival (the default
    /// sources below) or explicitly promoted through `session.queue`.
    pub steering: bool,
    /// Set for the schedule pump's awaited send: delivers the drain's
    /// result to `spawn_turn_for_event`'s receiver when this turn ends.
    pub completion: Option<tokio::sync::oneshot::Sender<Result<(), String>>>,
}

/// Which sources steer into a live run on arrival (L2114, owner
/// decision 2026-10-31): terminal and the notification channels do;
/// web waits for the explicit promote. `automatic` wakes do not exist
/// in this build, so there is no automatic arm.
pub fn steering_default(source: UserEntrySource) -> bool {
    matches!(
        source,
        UserEntrySource::Terminal
            | UserEntrySource::Inbox
            | UserEntrySource::OwnerInbox
            | UserEntrySource::Team
            | UserEntrySource::Worker
    )
}

/// Why a queued message cannot join the running task; an empty string
/// means it can (`steeringRefusal`, L2022-2035). The first clause is
/// bypassed when the chat carries no workspace binding
/// (`sessionWorkspaceRoot === null`).
pub fn steering_refusal(
    turn: &TurnRequest,
    session_workspace_root: Option<&str>,
    current: &TurnRequest,
    runtime_workspace_root: &str,
) -> &'static str {
    let source = turn.source;
    let local_owner = matches!(source, UserEntrySource::Terminal | UserEntrySource::Web);
    let notification = matches!(
        source,
        UserEntrySource::Inbox
            | UserEntrySource::OwnerInbox
            | UserEntrySource::Team
            | UserEntrySource::Worker
    );
    if !(session_workspace_root.is_none() || local_owner || notification) {
        return "This message's source cannot steer this task. It stays queued and runs next.";
    }
    let turn_workspace = turn
        .workspace_root
        .as_deref()
        .unwrap_or(runtime_workspace_root);
    let current_workspace = current
        .workspace_root
        .as_deref()
        .unwrap_or(runtime_workspace_root);
    if turn_workspace != current_workspace {
        return "This message targets a different workspace than the running task. It stays queued and runs next.";
    }
    ""
}

// --- send queue ------------------------------------------------------------

impl TurnRunner {
    /// Queues a send and starts the drain when idle (legacy
    /// `enqueueMessage`: a busy run keeps it, an idle one runs it).
    pub fn enqueue(self: &Arc<Self>, request: TurnRequest) {
        self.enqueue_inner(request, None);
    }

    /// `enqueue` with a completion: the schedule pump's awaited
    /// `sendMessage` — the receiver fires with this turn's result.
    pub fn enqueue_for_event(
        self: &Arc<Self>,
        request: TurnRequest,
        completion: tokio::sync::oneshot::Sender<Result<(), String>>,
    ) {
        self.enqueue_inner(request, Some(completion));
    }

    fn enqueue_inner(
        self: &Arc<Self>,
        request: TurnRequest,
        completion: Option<tokio::sync::oneshot::Sender<Result<(), String>>>,
    ) {
        let turn = queue::QueuedTurn {
            id: Uuid::new_v4().to_string(),
            steering: queue::steering_default(request.source),
            entry_added: false,
            request,
            completion,
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
        // The flip decides the duty: falseÔåÆtrue starts the loop (a
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
            let result = self.send(turn.request).await;
            if let Err(error) = &result {
                crate::agent::executor::report_turn_error(
                    &self.data_root,
                    &self.session_id,
                    &error.to_string(),
                );
            }
            // The pump's awaited send settles here — after the error
            // notice, exactly like the source's awaited sendMessage
            // resolving after the controller reported the failure.
            if let Some(completion) = turn.completion {
                let _ = completion.send(result.map(|_| ()).map_err(|error| error.to_string()));
            }
            // A compaction requested while this run was busy runs now:
            // the source serves it at the next step boundary, and the
            // run's end is its fallback when no step follows (the
            // boundary itself arrives with the auto-trigger slice).
            if let Some(recovery) = self.take_pending_compaction() {
                let runner = Arc::clone(&self);
                tokio::spawn(async move {
                    let _ = runner.compact(recovery).await;
                });
            }
        }
    }

    /// Whether a turn currently owns the session (the `session.queue`
    /// busy check's input ÔÇö exposed for tests and visibility
    /// surfaces).
    pub fn is_turn_active(&self) -> bool {
        self.active_current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }

    /// `session.queue`: steer or remove one queued message
    /// (`manageQueuedMessage`, L2041-2060 ÔÇö the source's refusal
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
    pub(super) fn admit_steered(
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
pub(super) fn fill_queue(
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
pub(super) fn push_user_entry(state: &mut SessionState, request: &TurnRequest, turn_id: &str) {
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
