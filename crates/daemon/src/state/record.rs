//! Where a saved record lives and how it is read whole.
//!
//! One authority for the single on-disk layout (`sessions/<id>/<id>.json`),
//! for parsing a complete record with its id verified against the file's own
//! contents, and for deriving the denormalised fields a record predating them
//! lacks. The windowed front-of-file read lives in `header`; the picker query
//! in `list`.

use std::fs;
use std::path::{Path, PathBuf};

use openbutler_protocol::SessionView;
use openbutler_protocol::entry::SessionEntry;
use openbutler_protocol::session::SessionMeta;
use serde_json::Value;

use super::paths::session_directory;

/// A record on disk: header fields plus the transcript. The `conversation`
/// arrays are part of the record — a load-then-save round trip must carry   /// them through untouched, so they are parsed (and only parsed; the door   /// treats their contents as an opaque AI-SDK payload).
#[derive(Debug, serde::Deserialize)]
/// One saved session: meta, transcript entries, and conversation.
pub struct SessionRecord {
    #[serde(flatten)]
    pub meta: SessionMeta,
    #[serde(default)]
    pub entries: Vec<SessionEntry>,
    #[serde(default)]
    pub conversation: Vec<Value>,
    #[serde(default, rename = "conversationArchive")]
    pub conversation_archive: Option<Vec<Value>>,
}

/// Bound on a full read; a larger record is treated as unreadable rather
/// than allocated wholesale (the commit-pressure question stays open).
const FULL_READ_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

/// Parses a record body — partial or whole. Callers verify the id.
pub fn parse_record(text: &str) -> Option<SessionRecord> {
    serde_json::from_str(text).ok()
}

/// Fills the four denormalised fields from `entries` when the record
/// predates them — the preview derivation, in one place for the header
/// read and the full view alike.
pub(crate) fn derive_meta(meta: SessionMeta, entries: &[SessionEntry]) -> SessionMeta {
    let has_user_message = meta
        .has_user_message
        .unwrap_or_else(|| entries.iter().any(is_user_entry));
    let preview = meta
        .preview
        .clone()
        .unwrap_or_else(|| entry_preview(entries));
    let first_user_message = meta
        .first_user_message
        .clone()
        .unwrap_or_else(|| first_user_text(entries));
    let last_assistant_message = meta
        .last_assistant_message
        .clone()
        .unwrap_or_else(|| last_assistant_text(entries));
    SessionMeta {
        has_user_message: Some(has_user_message),
        preview: Some(preview),
        first_user_message: Some(first_user_message),
        last_assistant_message: Some(last_assistant_message),
        ..meta
    }
}

/// Reads one record whole: bounded read, parse, verify the id against the
/// file's own contents. `None` means unreadable, oversized, malformed, or
/// belonging to another id — the agent loop's load path, which needs the
/// conversation arrays the view drops.
pub fn read_session_record(path: &Path, id: &str) -> Option<SessionRecord> {
    let metadata = fs::metadata(path).ok()?;
    if metadata.len() > FULL_READ_LIMIT_BYTES {
        return None;
    }
    let text = fs::read_to_string(path).ok()?;
    let record = parse_record(&text)?;
    (record.meta.id == id).then_some(record)
}

/// Reads one record whole: bounded read, parse, verify the id against the
/// file's own contents, derive what the record predates. `None` means
/// unreadable, oversized, malformed, or belonging to another id.
pub fn read_session_view(path: &Path, id: &str) -> Option<SessionView> {
    let metadata = fs::metadata(path).ok()?;
    if metadata.len() > FULL_READ_LIMIT_BYTES {
        return None;
    }
    let text = fs::read_to_string(path).ok()?;
    let record = parse_record(&text)?;
    if record.meta.id != id {
        return None;
    }
    let SessionRecord { meta, entries, .. } = record;
    Some(SessionView {
        meta: derive_meta(meta, &entries),
        entries,
    })
}

/// The record's file for an id — `None` for an invalid id, a missing file,
/// or a symlink (the escape rule). One layout, one rule: the record lives at
/// `sessions/<id>/<id>.json` and nowhere else.
pub fn record_path(data_root: &Path, id: &str) -> Option<PathBuf> {
    let current = session_directory(data_root, id)
        .ok()?
        .join(format!("{id}.json"));
    is_regular_file(&current).then_some(current)
}

/// Regular files only: symlinks are refused (the escape rule).
fn is_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
}

// --- Denormalised-field derivation (the preview helpers) ---------------------

fn is_user_entry(entry: &SessionEntry) -> bool {
    matches!(entry, SessionEntry::User { .. })
}

fn user_parts(entry: &SessionEntry) -> Option<(&str, u32)> {
    match entry {
        SessionEntry::User {
            text, image_count, ..
        } => Some((text, image_count.unwrap_or(0))),
        _ => None,
    }
}

/// Collapse whitespace, trim, slice — the one preview normalisation rule.
fn normalize_preview(raw: &str, limit: usize) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    slice_utf16(&collapsed, limit)
}

/// Slices by UTF-16 units so the limit matches the `String.slice`
/// boundary without splitting an astral character.
pub(crate) fn slice_utf16(value: &str, units: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in value.chars() {
        let width = ch.len_utf16();
        if used + width > units {
            break;
        }
        used += width;
        out.push(ch);
    }
    out
}

pub(crate) fn entry_preview(entries: &[SessionEntry]) -> String {
    let last_user = entries.iter().rev().find_map(user_parts);
    let Some((text, image_count)) = last_user else {
        return String::new();
    };
    let normalized = normalize_preview(text, 120);
    if normalized.is_empty() && image_count > 0 {
        "[Image attached #1]".into()
    } else {
        normalized
    }
}

pub(crate) fn first_user_text(entries: &[SessionEntry]) -> String {
    let Some((text, image_count)) = entries.iter().find_map(user_parts) else {
        return String::new();
    };
    let normalized = normalize_preview(text, 240);
    if normalized.is_empty() && image_count > 0 {
        "[Image attached]".into()
    } else {
        normalized
    }
}

pub(crate) fn last_assistant_text(entries: &[SessionEntry]) -> String {
    entries
        .iter()
        .rev()
        .find_map(|entry| match entry {
            SessionEntry::Assistant { text, .. } => Some(normalize_preview(text, 240)),
            _ => None,
        })
        .unwrap_or_default()
}
