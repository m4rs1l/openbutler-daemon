//! The send queue: what steers by default, why a queued message may
//! (or may not) join a live run, and the queue's unit type.
//!
//! Sources: legacy `agent-session.ts` — `QueuedTurn` (L2114),
//! `enqueueMessage` (L2062-2136), `manageQueuedMessage` (L2041-2060),
//! `steeringRefusal` (L2022-2035), admission (L2413-2441). The project
//! clause of `steeringRefusal` is absent: this build carries no project
//! binding to compare (documented in agent-loop.md).

use openbutler_protocol::entry::UserEntrySource;

use crate::agent::TurnRequest;

/// The `queueAction` of `session.queue`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueAction {
    Steer,
    Remove,
}

/// One queued send awaiting its turn (legacy `QueuedTurn`, L2114 — the
/// promise fields have no counterpart: sends are fire-and-forget).
#[derive(Debug)]
pub(crate) struct QueuedTurn {
    pub id: String,
    pub request: TurnRequest,
    /// A held message whose entry already sits in the transcript — the
    /// roster's `!entryAdded` filter keeps it from counting as queued.
    /// Owner sends never set it in this build; the flag keeps the
    /// snapshot rule intact.
    pub entry_added: bool,
    /// A live-run admission candidate: steered on arrival (the default
    /// sources below) or explicitly promoted through `session.queue`.
    pub steering: bool,
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
