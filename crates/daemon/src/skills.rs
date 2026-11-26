//! Agent Skill discovery — the port of `skills.ts` (read 2026-11-26).
//!
//! Metadata only: the skill's instructions stay unloaded until a turn
//! needs them. The roots are the source's seven, in its order (bundled,
//! user-level, workspace), later roots overriding duplicate names, and
//! the result is ordered by scope rank — bundled, workspace, global — then
//! by name, exactly as `discoverSkills` sorts.
//!
//! Kept out of the action: the discovery cache. The source cached for 5 s
//! to keep the agent loop from re-scanning on every call; this build
//! scans when asked (the CLI asks once), and a daemon-side cache can land
//! with the tool surface that needs it.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::shape::js_len;

/// The source's per-scope ceiling (`MAX_SKILLS_PER_SCOPE`).
pub const MAX_SKILLS_PER_SCOPE: usize = 128;
/// Frontmatter is read from at most this many bytes (`readSkillMetadata`).
const MAX_SKILL_FRONTMATTER_BYTES: u64 = 8 * 1024;
/// A description longer than this is not a skill the source accepts.
const MAX_DESCRIPTION_CHARS: usize = 1_024;
/// `isSkillName`: lowercase words joined by single dashes.
const MAX_NAME_CHARS: usize = 64;

/// Where a skill came from — the source's `AgentSkill['scope']`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SkillScope {
    /// Shipped next to the program.
    Builtin,
    /// `<workspace>/.claude/skills` and `<workspace>/.agents/skills`.
    Workspace,
    /// User-level roots under the home directory and the data dir.
    Global,
}

impl SkillScope {
    /// The scope as the wire and the table spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Global => "global",
            Self::Workspace => "workspace",
        }
    }

    /// The source's `scopeRank`: bundled, then workspace, then global.
    fn rank(self) -> u8 {
        match self {
            Self::Builtin => 0,
            Self::Workspace => 1,
            Self::Global => 2,
        }
    }
}

/// One discovery root and the scope its skills carry.
#[derive(Debug, Clone)]
pub struct SkillRoot {
    pub path: PathBuf,
    pub scope: SkillScope,
}

/// One discovered skill — metadata only (`AgentSkill`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSkill {
    pub name: String,
    pub description: String,
    pub file_path: String,
    pub scope: SkillScope,
}

/// The source's seven roots, in its order (`getSkillRoots`).
pub fn skill_roots(workspace_root: &str, data_dir: &Path) -> Vec<SkillRoot> {
    let home = crate::state::home_dir().to_string_lossy().into_owned();
    let codex_home = std::env::var("CODEX_HOME").unwrap_or_else(|_| format!("{home}/.codex"));
    let workspace = Path::new(workspace_root);
    vec![
        SkillRoot {
            path: builtin_skill_directory(),
            scope: SkillScope::Builtin,
        },
        SkillRoot {
            path: PathBuf::from(&home).join(".claude").join("skills"),
            scope: SkillScope::Global,
        },
        SkillRoot {
            path: PathBuf::from(&home).join(".agents").join("skills"),
            scope: SkillScope::Global,
        },
        SkillRoot {
            path: PathBuf::from(&codex_home).join("skills"),
            scope: SkillScope::Global,
        },
        SkillRoot {
            path: data_dir.join("skills"),
            scope: SkillScope::Global,
        },
        SkillRoot {
            path: workspace.join(".claude").join("skills"),
            scope: SkillScope::Workspace,
        },
        SkillRoot {
            path: workspace.join(".agents").join("skills"),
            scope: SkillScope::Workspace,
        },
    ]
}

/// The bundled skills directory: `<exe dir>/builtin-skills` when it
/// exists, else `<exe dir>/../skills` — the source's packaged-or-source
/// resolution against the module directory.
fn builtin_skill_directory() -> PathBuf {
    let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    else {
        return PathBuf::from("skills");
    };
    let packaged = exe_dir.join("builtin-skills");
    if packaged.is_dir() {
        return packaged;
    }
    exe_dir.join("skills")
}

/// Discovers the skills under the source's roots for `workspace_root`.
pub fn discover_skills(workspace_root: &str, data_dir: &Path) -> Vec<AgentSkill> {
    discover_from_roots(&skill_roots(workspace_root, data_dir))
}

/// Discovery over explicit roots — the testable half of `discoverSkills`:
/// per root, entries are visited in case-sensitive name order, a directory
/// whose `SKILL.md` frontmatter names it exactly adds (or overrides) the
/// skill, each scope stops at [`MAX_SKILLS_PER_SCOPE`], and the result is
/// ordered by scope rank then name.
pub fn discover_from_roots(roots: &[SkillRoot]) -> Vec<AgentSkill> {
    let mut skills: BTreeMap<String, AgentSkill> = BTreeMap::new();
    let mut scope_counts = [0usize; 3];
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root.path) else {
            // A missing or unreadable optional root is simply no skills.
            continue;
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        names.sort();
        for name in names {
            if !is_skill_name(&name) {
                continue;
            }
            if !skills.contains_key(&name)
                && scope_counts[root.scope.rank() as usize] >= MAX_SKILLS_PER_SCOPE
            {
                continue;
            }
            let file = root.path.join(&name).join("SKILL.md");
            let Some((meta_name, description)) = read_skill_metadata(&file) else {
                continue;
            };
            if meta_name != name {
                continue;
            }
            if !skills.contains_key(&name) {
                scope_counts[root.scope.rank() as usize] += 1;
            }
            skills.insert(
                meta_name,
                AgentSkill {
                    description,
                    file_path: file.display().to_string(),
                    scope: root.scope,
                    name,
                },
            );
        }
    }
    let mut discovered: Vec<AgentSkill> = skills.into_values().collect();
    discovered.sort_by(|left, right| {
        left.scope
            .rank()
            .cmp(&right.scope.rank())
            .then_with(|| left.name.cmp(&right.name))
    });
    discovered
}

/// `readSkillMetadata`: the file must be a regular file (never a symlink),
/// its first 8 KiB must open with a `---` frontmatter block, and the block
/// must name the skill with a non-empty description of at most 1024 chars.
fn read_skill_metadata(path: &Path) -> Option<(String, String)> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let mut buffer = Vec::new();
    std::io::Read::take(file, MAX_SKILL_FRONTMATTER_BYTES)
        .read_to_end(&mut buffer)
        .ok()?;
    let text = String::from_utf8_lossy(&buffer);
    let block = frontmatter_block(&text)?;
    let parsed: serde_json::Value = serde_saphyr::from_str_with_options(
        block,
        serde_saphyr::options! { strict_booleans: true },
    )
    .ok()?;
    let name = parsed.get("name")?.as_str()?.to_owned();
    let description = parsed.get("description")?.as_str()?.trim().to_owned();
    if !is_skill_name(&name)
        || description.is_empty()
        || js_len(&description) > MAX_DESCRIPTION_CHARS
    {
        return None;
    }
    Some((name, description))
}

/// The YAML between the opening and closing `---` fences (`^---\r?\n…`).
fn frontmatter_block(text: &str) -> Option<&str> {
    let rest = text
        .strip_prefix("---\r\n")
        .or_else(|| text.strip_prefix("---\n"))?;
    let mut consumed = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.strip_suffix('\n').unwrap_or(line);
        let trimmed = trimmed.strip_suffix('\r').unwrap_or(trimmed);
        if trimmed == "---" {
            return rest.get(..consumed);
        }
        consumed += line.len();
    }
    None
}

/// `isSkillName`: at most 64 chars of lowercase words joined by dashes.
fn is_skill_name(name: &str) -> bool {
    if name.is_empty() || js_len(name) > MAX_NAME_CHARS {
        return false;
    }
    name.split('-').all(|word| {
        !word.is_empty()
            && word
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    })
}
