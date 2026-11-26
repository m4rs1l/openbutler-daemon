//! `skills.list`: the Agent Skills this machine can offer.
//!
//! The workspace resolves the way bare launches do — the stored default,
//! else the custodian directory — and an explicit `workspace` (a path the
//! caller names) canonicalizes through the single normalizer. The answer
//! carries the discovered skills (metadata only) and the roots that were
//! scanned, so the empty screen can name them.
//!
//! Source: `cli-main.ts` `runSkills` (L580-597), `skills.ts`
//! `discoverSkills`/`getSkillRoots`, read 2026-11-26.

use std::path::Path;

use openbutler_protocol::DaemonRequest;
use serde_json::{Value, json};

use super::Host;
use crate::shape::{kv_line, screen_line, table_lines};
use crate::skills::{discover_skills, skill_roots};

/// Answers the skill table's rows and the scanned roots.
pub fn list(host: &Host, data_dir: &Path, request: &DaemonRequest) -> Result<Value, String> {
    let workspace = match request.get("workspace") {
        None | Some(Value::Null) => default_workspace(host, data_dir),
        Some(_) => {
            let path = super::require(
                request.get("workspace"),
                "workspace",
                super::MAX_WORKSPACE_PATH_BYTES,
            )?;
            crate::state::canonical_scope_root(&path)?
        }
    };
    let skills = discover_skills(&workspace, data_dir);
    let roots = skill_roots(&workspace, data_dir);
    // The screen, composed here: the table when skills were found, otherwise
    // the empty note, every root, and the hint. A client prints it.
    let mut lines: Vec<Value> = if skills.is_empty() {
        let mut lines = vec![screen_line("info", "No Agent Skills found.")];
        for root in &roots {
            lines.push(kv_line(
                root.scope.as_str(),
                &root.path.display().to_string(),
            ));
        }
        lines.push(screen_line(
            "info",
            "Add a skill-name/SKILL.md with Agent Skills frontmatter.",
        ));
        lines
    } else {
        let rows: Vec<Vec<String>> = skills
            .iter()
            .map(|skill| {
                vec![
                    skill.name.clone(),
                    skill.scope.as_str().to_owned(),
                    skill.description.clone(),
                    skill.file_path.clone(),
                ]
            })
            .collect();
        table_lines(&["NAME", "SCOPE", "DESCRIPTION", "SKILL FILE"], &rows)
    };
    if lines.is_empty() {
        lines.push(screen_line("info", "No Agent Skills found."));
    }
    Ok(json!({
        "workspace": workspace,
        "lines": lines,
        "skills": skills
            .iter()
            .map(|skill| json!({
                "name": skill.name,
                "scope": skill.scope.as_str(),
                "description": skill.description,
                "filePath": skill.file_path,
            }))
            .collect::<Vec<_>>(),
        "roots": roots
            .iter()
            .map(|root| json!({
                "scope": root.scope.as_str(),
                "path": root.path.display().to_string(),
            }))
            .collect::<Vec<_>>(),
    }))
}

/// `loadSettings().defaultWorkspaceRoot ?? getDataPath('custodian')`.
fn default_workspace(host: &Host, data_dir: &Path) -> String {
    host.settings()
        .as_ref()
        .and_then(|loaded| loaded.store.default_workspace_root.clone())
        .unwrap_or_else(|| data_dir.join("custodian").display().to_string())
}
