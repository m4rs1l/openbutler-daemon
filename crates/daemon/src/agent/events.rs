//! The publish unit: previous/next snapshots become one `DaemonEvent`.
//!
//! Source: legacy `makeSessionEvent` (daemon-host.ts L3711) and
//! `diffEntries` (L3749) + `sameEntryExceptText` (L3793), read
//! 2026-11-17. Pure functions — the fan-out (which sockets get the
//! frame) belongs to the host, not here.

use openbutler_protocol::envelope::DaemonEvent;
use openbutler_protocol::runtime::RuntimeViewData;
use openbutler_protocol::session::entry::SessionEntry;
use openbutler_protocol::session::{
    IndexedEntry, RunState, SessionEntriesPatch, SessionPatch, SessionSnapshotData, SessionView,
    TextDelta,
};

/// One publishable session state (legacy `AgentSessionSnapshot` reduced to
/// what the frame carries: the transcript view plus the run state).
#[derive(Debug, Clone, PartialEq)]
/// One publishable session state (the immutable event unit).
pub struct PublishSnapshot {
    pub session: SessionView,
    pub run: RunState,
}

/// The wire snapshot of one state (legacy `toSnapshotData`, L3806): full
/// transcript, no window markers — windows belong to `attach`.
pub fn to_snapshot_data(snapshot: &PublishSnapshot) -> SessionSnapshotData {
    SessionSnapshotData {
        session: snapshot.session.clone(),
        run: snapshot.run.clone(),
        entries_from: None,
        total_entries: None,
    }
}

/// The event between two states (legacy `makeSessionEvent`): a session id
/// change forces a full snapshot, otherwise a patch with the transcript
/// diff.
pub fn make_session_event(
    previous: &PublishSnapshot,
    next: &PublishSnapshot,
    runtime: &RuntimeViewData,
    version: u32,
) -> DaemonEvent {
    if previous.session.meta.id != next.session.meta.id {
        return DaemonEvent::Snapshot {
            version,
            snapshot: to_snapshot_data(next),
            runtime: runtime.clone(),
        };
    }
    let entries_patch = diff_entries(&previous.session.entries, &next.session.entries);
    DaemonEvent::Patch {
        version,
        patch: SessionPatch {
            session: next.session.meta.clone(),
            entries_patch,
            run: next.run.clone(),
            runtime: runtime.clone(),
        },
    }
}

/// The transcript diff (legacy `diffEntries`): `None` when every entry is
/// unchanged, `ReplaceAll` when the transcript shrank, otherwise a delta of
/// appends, in-place rewrites and pure text continuations.
pub fn diff_entries(
    previous: &[SessionEntry],
    next: &[SessionEntry],
) -> Option<SessionEntriesPatch> {
    if previous == next {
        return None;
    }
    if next.len() < previous.len() {
        return Some(SessionEntriesPatch::ReplaceAll {
            entries: next.to_vec(),
        });
    }

    let mut appended = false;
    for (index, entry) in previous.iter().enumerate() {
        if *entry != next[index] {
            appended = false;
            break;
        }
        appended = true;
    }
    if previous.is_empty() || appended {
        return Some(SessionEntriesPatch::Delta {
            append: next[previous.len()..].to_vec(),
            replace: vec![],
            append_text: vec![],
        });
    }

    let mut replace = Vec::new();
    let mut append_text = Vec::new();
    for (index, before) in previous.iter().enumerate() {
        let after = &next[index];
        if before == after {
            continue;
        }
        // Only a pure text append can ride the delta. A change to any other
        // field — a stamped thinking duration, for instance — has to replace
        // the entry, or the client keeps a stale copy that never learns the
        // new field.
        if let (Some(before_text), Some(after_text)) = (text_of(before), text_of(after))
            && same_kind(before, after)
            && after_text.starts_with(before_text)
            && same_entry_except_text(before, after)
        {
            append_text.push(TextDelta {
                index: index as i64,
                delta: after_text[before_text.len()..].to_owned(),
            });
            continue;
        }
        replace.push(IndexedEntry {
            index: index as i64,
            entry: after.clone(),
        });
    }
    Some(SessionEntriesPatch::Delta {
        append: vec![],
        replace,
        append_text,
    })
}

/// The transcript text an entry carries, if it is a text-carrying kind
/// (legacy `after.text.startsWith(before.text)` only ever runs for
/// assistant/reasoning entries; a kind mismatch falls to replace).
fn text_of(entry: &SessionEntry) -> Option<&str> {
    match entry {
        SessionEntry::Assistant { text, .. }
        | SessionEntry::Reasoning { text, .. }
        | SessionEntry::Notice { text, .. } => Some(text),
        SessionEntry::User { text, .. } => Some(text),
        _ => None,
    }
}

fn same_kind(before: &SessionEntry, after: &SessionEntry) -> bool {
    std::mem::discriminant(before) == std::mem::discriminant(after)
}

/// Whether two entries differ only in their text, which is the one field a
/// delta patch can carry. Values compare shallowly by kind+text-outside
/// equality: a nested change fails safe toward replacing the entry
/// (legacy compared object identity; value equality is the same answer
/// with no aliasing, documented in the c3 notes).
fn same_entry_except_text(before: &SessionEntry, after: &SessionEntry) -> bool {
    if std::mem::discriminant(before) != std::mem::discriminant(after) {
        return false;
    }
    strip_text(before)
        .zip(strip_text(after))
        .is_some_and(|(left, right)| left == right)
}

/// The entry serialized without its `text` field — the shallow compare of
/// the legacy implementation, expressed over the wire shape so every
/// future field is covered without a hand-maintained list.
fn strip_text(entry: &SessionEntry) -> Option<serde_json::Value> {
    let mut value = serde_json::to_value(entry).ok()?;
    if let Some(object) = value.as_object_mut() {
        object.remove("text");
    }
    Some(value)
}
