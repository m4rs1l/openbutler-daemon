//! Header-only reads: display fields without materialising the transcript.
//!
//! A saved session is pretty-printed with two-space indentation and every
//! header field serialises before `entries`, so the header alone is a few
//! hundred bytes at the front of the file. The read strategy:
//!
//! 1. take an 8 KiB prefix;
//! 2. cut at the structural marker `\n  "entries":` — a JSON string can never
//!    contain a real newline, so the marker cannot appear inside a value, and
//!    the cut is verified by parsing and comparing the record's id;
//! 3. a complete small file without a marker parses whole;
//! 4. everything else falls through to [`record::read_session_view`] (bounded
//!    full read, id verified, denormalised fields derived).
//!
//! Parsing, the bounded full read, and the derivation live in [`record`];
//! this file owns only the windowed front-of-file read.

use std::fs;
use std::path::Path;

use openbutler_protocol::SessionProject;
use openbutler_protocol::session::SessionMeta;

use super::record;

/// Display fields the picker, the session registry, and recent lists show.
#[derive(Debug, Clone, PartialEq)]
/// The on-disk record header (read without the entries).
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

impl SessionHeader {
    /// Projects the display fields out of an already-derived record meta
    /// (the windowed read proves the four denormalised fields present, the
    /// full read derives them in `record::derive_meta`). The fallbacks are
    /// derivation's own empty results — no user message, empty texts — never
    /// a silent guess.
    fn from_meta(meta: SessionMeta) -> Self {
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
            has_user_message: meta.has_user_message.unwrap_or(false),
            preview: meta.preview.unwrap_or_default(),
            first_user_message: meta.first_user_message.unwrap_or_default(),
            last_assistant_message: meta.last_assistant_message.unwrap_or_default(),
        }
    }
}

/// Bytes read from the front of a file before falling back.
const HEADER_WINDOW_BYTES: usize = 8 * 1024;
/// The structural boundary marker (two-space top-level indent).
const ENTRIES_BOUNDARY: &str = "\n  \"entries\":";

/// Reads a session's display fields; `None` means unreadable, malformed, or
/// belonging to another id.
pub fn read_session_header(path: &Path, id: &str) -> Option<SessionHeader> {
    let prefix = read_prefix(path, HEADER_WINDOW_BYTES)?;
    if let Some(boundary) = prefix.text.find(ENTRIES_BOUNDARY) {
        let cut = prefix.text[..boundary].trim_end_matches(|c: char| c.is_whitespace() || c == ',');
        let partial = format!("{cut}\n}}");
        if let Some(header) = partial_header(&partial, id) {
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

/// The windowed path: parse the cut prefix, verify the id, and only trust it
/// when the denormalised fields are already present (there is no transcript
/// here to derive them from).
fn partial_header(text: &str, id: &str) -> Option<SessionHeader> {
    let record = record::parse_record(text)?;
    if record.meta.id != id {
        return None;
    }
    let needs_derivation = record.meta.has_user_message.is_none()
        || record.meta.preview.is_none()
        || record.meta.first_user_message.is_none()
        || record.meta.last_assistant_message.is_none();
    if needs_derivation {
        return None;
    }
    Some(SessionHeader::from_meta(record.meta))
}

/// Full-file path: parse everything, verify the id, derive what the record
/// predates (via the single full-read authority).
fn full_header(path: &Path, id: &str) -> Option<SessionHeader> {
    let view = record::read_session_view(path, id)?;
    Some(SessionHeader::from_meta(view.meta))
}

/// Reads at most `bytes` from the front of a file. `complete` mirrors the
/// read rule: only a short read proves end-of-file (the window is measured
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
