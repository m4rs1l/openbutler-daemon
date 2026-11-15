//! Header-only reads: display fields without materialising the transcript.
//!
//! A saved session is pretty-printed with two-space indentation and every
//! header field serialises before `entries`, so the header alone is a few
//! hundred bytes at the front of the file. The read strategy mirrors the
//! legacy `readSessionHeader`:
//!
//! 1. take an 8 KiB prefix;
//! 2. cut at the structural marker `\n  "entries":` — a JSON string can never
//!    contain a real newline, so the marker cannot appear inside a value, and
//!    the cut is verified by parsing and comparing the record's id;
//! 3. a complete small file without a marker parses whole;
//! 4. everything else falls through to a bounded full read that derives the
//!    denormalised fields from `entries` (the legacy path streams here to cap
//!    memory under commit pressure; see the open question in
//!    docs/state-formats.md).

use std::fs;
use std::path::Path;

use openbutler_protocol::SessionProject;
use openbutler_protocol::entry::SessionEntry;
use openbutler_protocol::session::SessionMeta;

/// Display fields the picker, the session registry, and recent lists show.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionHeader {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub last_activity_at: Option<String>,
    pub provider_id: String,
    pub model_id: String,
    pub workspace_root: Option<String>,
    pub project: Option<SessionProject>,
    pub has_user_message: bool,
    pub preview: String,
    pub first_user_message: String,
    pub last_assistant_message: String,
}

/// Bytes read from the front of a file before falling back.
const HEADER_WINDOW_BYTES: usize = 8 * 1024;
/// Bound on the full-read fallback; a larger record is treated as unreadable
/// rather than allocated wholesale.
const FULL_READ_LIMIT_BYTES: u64 = 64 * 1024 * 1024;
/// The structural boundary marker (two-space top-level indent).
const ENTRIES_BOUNDARY: &str = "\n  \"entries\":";

/// A record on disk: header fields plus the two transcript arrays (typed only
/// as JSON — the `conversation` archive stays an AI-SDK concern, M2).
#[derive(Debug, serde::Deserialize)]
struct SessionRecord {
    #[serde(flatten)]
    meta: SessionMeta,
    #[serde(default)]
    entries: Vec<SessionEntry>,
}

/// Reads a session's display fields; `None` means unreadable, malformed, or
/// belonging to another id.
pub fn read_session_header(path: &Path, id: &str) -> Option<SessionHeader> {
    let prefix = read_prefix(path, HEADER_WINDOW_BYTES)?;
    if let Some(boundary) = prefix.text.find(ENTRIES_BOUNDARY) {
        let cut = prefix.text[..boundary].trim_end_matches(|c: char| c.is_whitespace() || c == ',');
        let partial = format!("{cut}\n}}");
        if let Some(header) = header_from_meta_str(&partial, Some(id), false) {
            return Some(header);
        }
        // Boundary hit but the cut failed verification: a wrong id or a
        // pre-denormalisation record — derive from the full file instead.
        return full_header(path, id);
    }
    if prefix.complete {
        return full_header(path, id);
    }
    // Marker beyond the window: the record's header is unusually large.
    full_header(path, id)
}

fn header_from_meta_str(
    text: &str,
    expected_id: Option<&str>,
    derive: bool,
) -> Option<SessionHeader> {
    let record: SessionRecord = serde_json::from_str(text).ok()?;
    let id_ok = expected_id.is_none_or(|expected| record.meta.id == expected);
    if !id_ok {
        return None;
    }
    let needs_derivation = record.meta.has_user_message.is_none()
        || record.meta.preview.is_none()
        || record.meta.first_user_message.is_none()
        || record.meta.last_assistant_message.is_none();
    if needs_derivation && !derive {
        return None;
    }
    Some(header_from_parts(record.meta, &record.entries))
}

/// Full-file path: parse everything, verify the id, derive what the record
/// predates.
fn full_header(path: &Path, id: &str) -> Option<SessionHeader> {
    let metadata = fs::metadata(path).ok()?;
    if metadata.len() > FULL_READ_LIMIT_BYTES {
        return None;
    }
    let text = fs::read_to_string(path).ok()?;
    header_from_meta_str(&text, Some(id), true)
}

fn header_from_parts(meta: SessionMeta, entries: &[SessionEntry]) -> SessionHeader {
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
    SessionHeader {
        id: meta.id,
        title: meta.title,
        created_at: meta.created_at,
        updated_at: meta.updated_at,
        last_activity_at: meta.last_activity_at,
        provider_id: meta.provider_id,
        model_id: meta.model_id,
        workspace_root: meta.workspace_root,
        project: meta.project,
        has_user_message,
        preview,
        first_user_message,
        last_assistant_message,
    }
}

/// Reads at most `bytes` from the front of a file. `complete` mirrors the
/// legacy rule: only a short read proves end-of-file (the window is measured
/// in bytes, the text in characters).
fn read_prefix(path: &Path, bytes: usize) -> Option<Prefix> {
    use std::io::Read;
    let mut file = fs::File::open(path).ok()?;
    let mut buffer = vec![0u8; bytes];
    let mut filled = 0;
    while filled < bytes {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(_) => return None,
        }
    }
    Some(Prefix {
        text: String::from_utf8_lossy(&buffer[..filled]).into_owned(),
        complete: filled < bytes,
    })
}

struct Prefix {
    text: String,
    complete: bool,
}

// --- Denormalised-field derivation (the legacy preview helpers) -------------

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

/// Slices by UTF-16 units so the limit matches the legacy `String.slice`
/// boundary without splitting an astral character.
fn slice_utf16(value: &str, units: usize) -> String {
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

fn entry_preview(entries: &[SessionEntry]) -> String {
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

fn first_user_text(entries: &[SessionEntry]) -> String {
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

fn last_assistant_text(entries: &[SessionEntry]) -> String {
    entries
        .iter()
        .rev()
        .find_map(|entry| match entry {
            SessionEntry::Assistant { text, .. } => Some(normalize_preview(text, 240)),
            _ => None,
        })
        .unwrap_or_default()
}
