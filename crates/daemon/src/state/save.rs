//! The single write path for session records.
//!
//! Source: legacy `src/session-store.ts` `prepareSessionSave` (L1048) and
//! `saveSession` (L1101). Two contracts live here and nowhere else:
//!
//! - **Recompute, never trust:** `hasUserMessage` and the three preview
//!   fields are rebuilt from `entries` on every save — a stale value on a
//!   loaded record must not survive a write. `updatedAt` is refreshed on
//!   every save; `lastActivityAt` is content-stamped by the caller and is   never touched here (opening a chat must not rewrite it).
//! - **Header order:** the listing cuts a header from the front of the   file at the structural `entries` boundary, so the flattened meta (which   carries the display fields) must serialise before `entries`, in   pretty two-space JSON with the legacy trailing newline.
//!
//! A record without a user entry is **skipped**, not an error (the
//! title-only save path); `allow_empty` opts in.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use openbutler_protocol::session::SessionMeta;

use super::StateError;
use super::paths::session_directory;
use super::record::{SessionRecord, entry_preview, first_user_text, last_assistant_text};

/// What a save did: the record landed, or (legacy `null`) nothing was
/// written because there was no user entry to anchor the session on.
#[derive(Debug)]
/// What a save did: written to this path, or skipped (no user message).
pub enum SaveOutcome {
    Written(PathBuf),
    Skipped,
}

/// The on-disk shape: the record's meta flattened at the front (display
/// fields included), then `entries`, then the AI-SDK conversation arrays.
/// Field order inside the meta follows the protocol type — JSON objects   are unordered and every reader addresses keys, but the meta-before-   `entries` boundary is structural and enforced by the flatten position.
#[derive(serde::Serialize)]
struct SavedSession<'a> {
    #[serde(flatten)]
    meta: &'a SessionMeta,
    entries: &'a [openbutler_protocol::entry::SessionEntry],
    conversation: &'a [serde_json::Value],
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "conversationArchive"
    )]
    conversation_archive: Option<&'a [serde_json::Value]>,
}

/// Writes `sessions/<id>/<id>.json` atomically: temp file beside the   target, then rename. Ids are validated first, so the path can only ever   be the storage root plus a UUID.
pub fn save_session(
    data_root: &Path,
    record: &SessionRecord,
    allow_empty: bool,
) -> Result<SaveOutcome, StateError> {
    // Validation gate (legacy `isSavedSession` throw — narrowed to the
    // fields that exist today; the full closed validator lands with the   // fields' producers).
    if record.meta.version != 1 || !super::paths::is_session_id(&record.meta.id) {
        return Err(StateError::InvalidRecord);
    }

    let has_user_message = record
        .entries
        .iter()
        .any(|entry| matches!(entry, openbutler_protocol::entry::SessionEntry::User { .. }));
    if !has_user_message && !allow_empty {
        return Ok(SaveOutcome::Skipped);
    }

    let mut meta = record.meta.clone();
    meta.updated_at = chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string();
    meta.has_user_message = Some(has_user_message);
    meta.preview = Some(entry_preview(&record.entries));
    meta.first_user_message = Some(first_user_text(&record.entries));
    meta.last_assistant_message = Some(last_assistant_text(&record.entries));

    let payload = SavedSession {
        meta: &meta,
        entries: &record.entries,
        conversation: &record.conversation,
        conversation_archive: record.conversation_archive.as_deref(),
    };
    let mut body = serde_json::to_string_pretty(&payload)
        .map_err(|error| StateError::Io(std::io::Error::other(error)))?;
    body.push('\n');

    let directory = session_directory(data_root, &meta.id)?;
    fs::create_dir_all(&directory)?;
    let path = directory.join(format!("{}.json", meta.id));
    let temporary = PathBuf::from(format!("{}.tmp", path.display()));

    // Temp beside the target (same filesystem, so the rename is atomic),
    // private from creation, removed on any failure — a crash leaves   // either the old file or the temp, never a half-record.
    let write_result = (|| -> std::io::Result<()> {
        let mut options = fs::OpenOptions::new();
        options.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(body.as_bytes())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(StateError::Io(error));
    }
    if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(&temporary);
        return Err(StateError::Io(error));
    }
    Ok(SaveOutcome::Written(path))
}
