//! The project store: `projects.json` validation, folder canonicalisation
//! and the `sessionProject` builder `session.select-project` binds with.
//!
//! Source: legacy `projects/store.ts` `sessionProject` / `projectFolder` /
//! `readProjects` / `resolveProjectSelection` (read 2026-11-18). Nothing
//! else in the daemon reads `projects.json` or canonicalises a project
//! folder - this file answers both questions and only those.

use std::path::Path;

use openbutler_protocol::entry::SessionProject;

use super::paths::strip_windows_prefix;

/// `projects/store.ts` store limits: how many projects a record file may
/// carry and how many folders one project may lock.
const MAX_PROJECTS: usize = 200;
const MAX_FOLDERS: usize = 16;

/// One stored project - only the fields `resolveProjectSelection` reads.
/// `createdAt` is validated by the reader but never used to resolve a
/// scope, so it is not carried here.
struct StoredProject {
    id: String,
    name: String,
    folders: Vec<String>,
}

/// The context a resolution produces (`ProjectContext` in the source,
/// which also carries the id - the request id is authoritative for the
/// binding, so it is not duplicated here).
struct ProjectContext {
    name: String,
    folders: Vec<String>,
}

/// `projectFolder` - one folder, one verdict: a string that is absolute,
/// NUL-free and short enough, then reachable and a directory, reported
/// like the source's `realpathSync` (the Windows extended prefix never
/// reached the wire there, so it never reaches it here).
pub fn project_folder(path: &str) -> Result<String, String> {
    // JS `path.length` counts UTF-16 units; the port counts the same
    // units so a non-BMP path refuses identically.
    if path.encode_utf16().count() > 32768 || !Path::new(path).is_absolute() || path.contains('\0')
    {
        return Err("Choose an absolute folder path.".to_owned());
    }
    match std::fs::canonicalize(path) {
        Ok(canonical) if canonical.is_dir() => {
            Ok(strip_windows_prefix(&canonical).display().to_string())
        }
        _ => Err(format!("Cannot reach the folder: {path}")),
    }
}

/// `readProjects` - ENOENT is an empty store; every other failure is the
/// source's fixed sentence for a shape it refuses, or the underlying
/// error's own message (`JSON.parse` failures surface this decoder's
/// text where the source surfaced its SyntaxError's).
fn read_projects(data_dir: &Path) -> Result<Vec<StoredProject>, String> {
    let path = data_dir.join("projects.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|error| error.to_string())?;
    let invalid = || "The project record file is invalid.".to_owned();
    let Some(object) = value.as_object() else {
        return Err(invalid());
    };
    if object.get("version") != Some(&serde_json::json!(1)) {
        return Err(invalid());
    }
    let Some(items) = object.get("projects").and_then(|value| value.as_array()) else {
        return Err(invalid());
    };
    if items.len() > MAX_PROJECTS {
        return Err(invalid());
    }
    let mut projects = Vec::with_capacity(items.len());
    for item in items {
        let Some(item) = item.as_object() else {
            return Err(invalid());
        };
        let id = item
            .get("id")
            .and_then(|value| value.as_str())
            .filter(|value| matches_id_pattern(value));
        let name = item
            .get("name")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty() && value.encode_utf16().count() <= 80);
        let created_at = item.get("createdAt").and_then(|value| value.as_str());
        let Some(folders) = item.get("folders").and_then(|value| value.as_array()) else {
            return Err(invalid());
        };
        if folders.is_empty() || folders.len() > MAX_FOLDERS {
            return Err(invalid());
        }
        let mut parsed_folders = Vec::with_capacity(folders.len());
        for folder in folders {
            let Some(text) = folder.as_str().filter(|text| Path::new(text).is_absolute()) else {
                return Err(invalid());
            };
            parsed_folders.push(text.to_owned());
        }
        let (Some(id), Some(name), Some(_created_at)) = (id, name, created_at) else {
            return Err(invalid());
        };
        projects.push(StoredProject {
            id: id.to_owned(),
            name: name.to_owned(),
            folders: parsed_folders,
        });
    }
    Ok(projects)
}

/// `/^[a-f0-9-]{36}$/` (the source's store id pattern), spelled without
/// the regex dependency: exactly 36 characters, every one from
/// `[0-9a-f-]`, matched across the whole string.
fn matches_id_pattern(value: &str) -> bool {
    let mut count = 0;
    for character in value.chars() {
        if !(character.is_ascii_digit() || ('a'..='f').contains(&character) || character == '-') {
            return false;
        }
        count += 1;
    }
    count == 36
}

/// `resolveProjectSelection` - a selected project wins with its first
/// (canonicalised) folder; a null selection runs where the runtime
/// already points, falling back to `<data>/workspace` created here so
/// nothing can resolve to an empty path.
fn resolve_selection(
    id: Option<&str>,
    fallback_root: Option<&str>,
    data_dir: &Path,
) -> Result<(String, Option<ProjectContext>), String> {
    if let Some(id) = id {
        let project = read_projects(data_dir)?
            .into_iter()
            .find(|stored| stored.id == id)
            .ok_or_else(|| "Project not found.".to_owned())?;
        let folders = project
            .folders
            .iter()
            .map(|folder| project_folder(folder))
            .collect::<Result<Vec<_>, _>>()?;
        // `read_projects` guarantees 1..=16 folders, so the first exists;
        // the empty-string belt can never fire and still answers with a
        // source sentence should validation ever loosen.
        let workspace_root = match folders.first() {
            Some(root) => root.clone(),
            None => project_folder("")?,
        };
        return Ok((
            workspace_root,
            Some(ProjectContext {
                name: project.name,
                folders,
            }),
        ));
    }
    if let Some(fallback) = fallback_root.filter(|value| !value.is_empty()) {
        return Ok((project_folder(fallback)?, None));
    }
    let folder = data_dir.join("workspace");
    std::fs::create_dir_all(&folder).map_err(|error| error.to_string())?;
    Ok((project_folder(&folder.display().to_string())?, None))
}

/// `sessionProject` - the binding shape the record carries: the request
/// id passes through untouched, name and folders come from the resolved
/// context, and a fresh binding is never locked.
pub fn session_project(
    id: Option<&str>,
    fallback_root: Option<&str>,
    data_dir: &Path,
) -> Result<SessionProject, String> {
    let (workspace_root, context) = resolve_selection(id, fallback_root, data_dir)?;
    let (name, folders) = match context {
        Some(context) => (Some(context.name), context.folders),
        None => (None, Vec::new()),
    };
    Ok(SessionProject {
        id: id.map(str::to_owned),
        name,
        folders,
        workspace_root,
        locked: false,
    })
}
