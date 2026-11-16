//! The picker query: which sessions exist, in which order, for which scope.

use std::fs;
use std::path::{Path, PathBuf};

use openbutler_protocol::SessionSummary;

use super::StateError;
use super::header::{SessionHeader, read_session_header};
use super::paths::{CUSTODIAN_SESSION_ID, is_session_id, same_path, sessions_directory};
use super::record::record_path;

/// Workspace scoping of the list, mirroring the legacy argument triple:
/// no filter / only sessions without a workspace / exact path match.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum WorkspaceFilter {
    /// No scope: every chat in the data directory.
    #[default]
    Any,
    /// Legacy `null`: only sessions that never declared a workspace.
    Rootless,
    /// Legacy string: path-equal workspace (case-insensitive on Windows).
    Exactly(String),
}

/// Chats the owner can open: the custodian is excluded, drafts without a user
/// message are excluded, unreadable or identity-mismatched files are skipped
/// one by one so a single damaged record never blocks the picker.
pub fn list_sessions(
    data_root: &Path,
    filter: &WorkspaceFilter,
) -> Result<Vec<SessionSummary>, StateError> {
    let mut sessions = Vec::new();
    for candidate in session_file_candidates(data_root)? {
        let id = candidate.id;
        let Some(header) = read_session_header(&candidate.path, &id) else {
            continue;
        };
        if header.id != id || id == CUSTODIAN_SESSION_ID || !header.has_user_message {
            continue;
        }
        if !matches_filter(&header, filter) {
            continue;
        }
        sessions.push(summary_from_header(header));
    }
    // Newest activity first; ties broken by id ascending.
    sessions.sort_by(|left, right| {
        let left_key = left.last_activity_at.as_deref().unwrap_or(&left.updated_at);
        let right_key = right
            .last_activity_at
            .as_deref()
            .unwrap_or(&right.updated_at);
        right_key.cmp(left_key).then_with(|| left.id.cmp(&right.id))
    });
    Ok(sessions)
}

fn matches_filter(header: &SessionHeader, filter: &WorkspaceFilter) -> bool {
    match filter {
        WorkspaceFilter::Any => true,
        WorkspaceFilter::Rootless => header.workspace_root.is_none(),
        WorkspaceFilter::Exactly(target) => header
            .workspace_root
            .as_deref()
            .is_some_and(|root| same_path(root, target)),
    }
}

fn summary_from_header(header: SessionHeader) -> SessionSummary {
    SessionSummary {
        running: None,
        project: header.project,
        id: header.id,
        title: header.title,
        preview: header.preview,
        updated_at: header.updated_at,
        last_activity_at: header.last_activity_at,
        provider_id: header.provider_id,
        model_id: header.model_id,
        workspace_root: header.workspace_root,
    }
}

struct SessionCandidate {
    id: String,
    path: PathBuf,
}

/// Session records on disk: current `sessions/<id>/<id>.json` with the legacy
/// flat `sessions/<id>.json` as read-only fallback. Directory entries are
/// accepted only as real, non-symlink files inside the sessions root.
fn session_file_candidates(data_root: &Path) -> Result<Vec<SessionCandidate>, StateError> {
    let sessions_root = sessions_directory(data_root);
    let entries = match fs::read_dir(&sessions_root) {
        Ok(entries) => entries,
        // A missing sessions directory simply has no sessions; anything else
        // is a real failure and must surface.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(StateError::Io(error)),
    };
    let mut ids = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_session_id(&name) {
            ids.push(name);
        } else if let Some(id) = name.strip_suffix(".json").filter(|id| is_session_id(id)) {
            ids.push(id.to_owned());
        }
    }
    ids.sort();
    ids.dedup();
    let candidates: Vec<SessionCandidate> = ids
        .into_iter()
        .filter_map(|id| {
            let path = record_path(data_root, &id)?;
            Some(SessionCandidate { id, path })
        })
        .collect();
    Ok(candidates)
}
