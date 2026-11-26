//! `sessions.prune`: deleting saved session records older than an explicit
//! cutoff, with a preview that runs the very same selection.
//!
//! Source (read 2026-11-26): `cli-main.ts` `runSessionsCommand` (L1465-1526),
//! `retention.ts` `selectFilesToPrune`, `session-store.ts` `listSessionFiles`
//! / `removeSavedSession` and `cli/ui.ts` `bytes`. Three things moved:
//!
//!   * the source ran this in the CLI process because the CLI wrote the
//!     session files; the host owns them now, so the work lives here — and the
//!     source's "stop the background host first" rule becomes unnecessary,
//!     because selection is by modification time and a session being written
//!     right now can never be older than the cutoff;
//!   * deletion is a hard delete, exactly the source's `rmSync`, because
//!     pruning exists to reclaim disk: routing the tree through the platform
//!     trash would keep the bytes the owner asked to reclaim. The safety net
//!     is the preview plus the explicit `apply` flag;
//!   * the answer carries **tones per line**, not glyphs: the host decides
//!     which sentence is said and how it reads; the client owns the palette
//!     (the same split `usage.report` makes with finished lines).
//!
//! The derived session index does not exist in this build (`sessions index`
//! waits on the local model runtime). When it lands, pruning must drop the
//! removed ids there too, as the source does.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use openbutler_protocol::DaemonRequest;
use serde_json::{Value, json};

use crate::ledger::{now_ms, parse_duration};
use crate::shape::{format_bytes, screen_line as line};
use crate::state::{CUSTODIAN_SESSION_ID, is_session_id, record_path, sessions_directory};

/// One selected record: the file it was seen at, the exact modification time
/// and size the identity check re-reads before deleting.
struct Candidate {
    id: String,
    path: PathBuf,
    modified: SystemTime,
    modified_ms: i64,
    size_bytes: u64,
}

/// `sessions.prune`: `{olderThan: "30d", apply?: bool}`.
pub fn prune(data_dir: &Path, request: &DaemonRequest) -> Result<Value, String> {
    let older = request
        .get("olderThan")
        .and_then(Value::as_str)
        .unwrap_or("");
    let Some(milliseconds) = parse_duration(older) else {
        return Err(format!(
            "Invalid duration \"{older}\". Use a value like 30d, 12h, 90m, or 2w."
        ));
    };
    let apply = match request.get("apply") {
        None => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return Err("apply must be a boolean.".to_owned()),
    };

    let cutoff_ms = now_ms().saturating_sub(milliseconds);
    let selection = select(data_dir, cutoff_ms)?;
    let total_bytes: u64 = selection.iter().map(|file| file.size_bytes).sum();
    let mut lines: Vec<Value> = selection.iter().map(row).collect();

    if selection.is_empty() {
        lines.push(line(
            "info",
            format!("No session files are older than {older}; nothing to prune."),
        ));
        return Ok(json!({ "count": 0, "totalBytes": 0, "lines": lines }));
    }

    if !apply {
        lines.push(line(
            "info",
            format!(
                "{} session file(s), {} total. Nothing was deleted; re-run with --yes to delete them.",
                selection.len(),
                format_bytes(total_bytes)
            ),
        ));
        return Ok(json!({
            "count": selection.len(),
            "totalBytes": total_bytes,
            "lines": lines
        }));
    }

    let mut deleted = 0u64;
    let mut failed = 0u64;
    for file in &selection {
        if !still_the_same(data_dir, file) {
            failed += 1;
            continue;
        }
        // The record's own directory holds the transcript and its files: one
        // lifetime, one removal (the source removed the same tree).
        match file.path.parent().map(fs::remove_dir_all) {
            Some(Ok(())) => deleted += 1,
            _ => failed += 1,
        }
    }
    lines.push(line(
        "success",
        format!(
            "Deleted {deleted} session file(s) ({}).",
            format_bytes(total_bytes)
        ),
    ));
    if failed > 0 {
        lines.push(line(
            "warn",
            format!("{failed} file(s) could not be deleted."),
        ));
    }
    Ok(json!({
        "count": selection.len(),
        "totalBytes": total_bytes,
        "deleted": deleted,
        "failed": failed,
        "lines": lines
    }))
}

/// The selection: `sessions/<id>/<id>.json` records whose modification time
/// is strictly older than the cutoff, custodian excluded, oldest first and
/// id-ascending on ties (the source's `listSessionFiles` order).
fn select(data_dir: &Path, cutoff_ms: i64) -> Result<Vec<Candidate>, String> {
    let entries = match fs::read_dir(sessions_directory(data_dir)) {
        Ok(entries) => entries,
        // A missing sessions directory simply has no sessions.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    let mut selection = Vec::new();
    for entry in entries.flatten() {
        let id = entry.file_name().to_string_lossy().into_owned();
        // Retention never deletes the machine's own session: it is not a chat,
        // and nothing recreates it except the host starting with an empty
        // record.
        if !is_session_id(&id) || id == CUSTODIAN_SESSION_ID {
            continue;
        }
        // The record is the only file the layout puts in a session directory
        // that counts; a damaged or partial directory is skipped whole
        // instead of half-deleted.
        let Some(path) = record_path(data_dir, &id) else {
            continue;
        };
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let Some(modified_ms) = millis_of(modified) else {
            continue;
        };
        if modified_ms >= cutoff_ms {
            continue;
        }
        selection.push(Candidate {
            id,
            path,
            modified,
            modified_ms,
            size_bytes: metadata.len(),
        });
    }
    selection.sort_by(|left, right| {
        left.modified_ms
            .cmp(&right.modified_ms)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(selection)
}

/// The row the source prints: two spaces, the record's **UTC** date, the id
/// and the byte count.
fn row(file: &Candidate) -> Value {
    let date = chrono::DateTime::from_timestamp_millis(file.modified_ms)
        .map(|moment| moment.format("%Y-%m-%d").to_string())
        .unwrap_or_default();
    line(
        "plain",
        format!("  {date}  {}  {}", file.id, format_bytes(file.size_bytes)),
    )
}

/// The identity re-check (source L1500): the derived path, the recorded
/// modification time and the recorded size must all still match, so a record
/// rewritten between selection and deletion is left alone.
fn still_the_same(data_dir: &Path, file: &Candidate) -> bool {
    let Some(path) = record_path(data_dir, &file.id) else {
        return false;
    };
    if path != file.path {
        return false;
    }
    let Ok(metadata) = fs::metadata(&path) else {
        return false;
    };
    metadata.len() == file.size_bytes && metadata.modified().ok() == Some(file.modified)
}

fn millis_of(time: SystemTime) -> Option<i64> {
    let elapsed = time.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(elapsed.as_millis()).ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// A record that changed after selection must be skipped, not deleted.
    #[test]
    fn a_record_that_changed_after_selection_is_left_alone() {
        let root = std::env::temp_dir().join(format!("prune-identity-{}", std::process::id()));
        let id = "33333333-3333-4333-8333-333333333333";
        let folder = root.join("sessions").join(id);
        fs::create_dir_all(&folder).unwrap();
        let path = folder.join(format!("{id}.json"));
        fs::write(&path, b"body").unwrap();

        let selection = select(&root, i64::MAX).unwrap();
        assert_eq!(selection.len(), 1);
        let candidate = &selection[0];
        assert!(still_the_same(&root, candidate));

        // Rewrite the file: the size stays, the modification time moves.
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(&path, b"body").unwrap();
        assert!(!still_the_same(&root, candidate));

        fs::remove_dir_all(&root).unwrap();
    }

    /// The size alone is enough to refuse: a rewrite that keeps a timestamp
    /// but changes the bytes is still a different record.
    #[test]
    fn a_record_whose_size_changed_is_left_alone() {
        let root = std::env::temp_dir().join(format!("prune-size-{}", std::process::id()));
        let id = "44444444-4444-4444-8444-444444444444";
        let folder = root.join("sessions").join(id);
        fs::create_dir_all(&folder).unwrap();
        let path = folder.join(format!("{id}.json"));
        fs::write(&path, b"body").unwrap();

        let candidate = &select(&root, i64::MAX).unwrap()[0];
        let recorded = candidate.modified;
        fs::write(&path, b"longer body").unwrap();
        let handle = fs::OpenOptions::new().write(true).open(&path).unwrap();
        handle.set_modified(recorded).unwrap();
        assert!(!still_the_same(&root, candidate));

        fs::remove_dir_all(&root).unwrap();
    }
}
