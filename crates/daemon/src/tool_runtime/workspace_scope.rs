//! Workspace scope classification: which paths the owner has already
//! trusted for this turn, resolved missing-tolerantly so a trusted root
//! that does not exist yet still contains its future children.
//!
//! Source: legacy `src/workspace-scope.ts`, read 2026-11-17.

use std::path::{Path, PathBuf};

/// Where a target sits relative to the trusted roots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Where a path sits relative to the trusted roots.
pub enum WorkspaceScope {
    Workspace,
    Outside,
    Unknown,
}

/// `resolveThroughExistingParents`: lexical resolution that walks up
/// through missing components, resolving symlinks only where the path
/// actually exists. `None` when a real component cannot be read.
pub fn resolve_through_existing_parents(path: &Path) -> Option<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut current = absolute;
    loop {
        match std::fs::symlink_metadata(&current) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(current.file_name()?.to_os_string());
                let parent = current.parent()?.to_path_buf();
                if parent == current {
                    return None;
                }
                current = parent;
            }
            Err(_) => return None,
        }
    }
    let mut resolved = std::fs::canonicalize(&current).ok()?;
    for component in missing.iter().rev() {
        resolved.push(component);
    }
    Some(resolved)
}

/// Case-folds the way the source does on Windows before comparing.
fn normalize(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    #[cfg(windows)]
    {
        PathBuf::from(absolute.to_string_lossy().to_lowercase())
    }
    #[cfg(not(windows))]
    {
        absolute
    }
}

/// True when the resolved target stays inside the resolved parent.
fn is_within(target: &Path, parent: &Path) -> bool {
    let target = normalize(target);
    let parent = normalize(parent);
    target.strip_prefix(&parent).is_ok()
}

/// Classifies against every root the owner has already trusted: the
/// turn's workspace and the agent's own scratch area. Any single match
/// keeps the operation inside the approved boundary.
pub fn classify_trusted_path(path: &Path, roots: &[PathBuf]) -> WorkspaceScope {
    let Some(target) = resolve_through_existing_parents(path) else {
        return WorkspaceScope::Unknown;
    };
    let mut resolved_any = false;
    for root in roots {
        let Some(resolved_root) = resolve_through_existing_parents(root) else {
            continue;
        };
        resolved_any = true;
        if is_within(&target, &resolved_root) {
            return WorkspaceScope::Workspace;
        }
    }
    if resolved_any {
        WorkspaceScope::Outside
    } else {
        WorkspaceScope::Unknown
    }
}
