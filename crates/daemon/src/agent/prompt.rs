//! The prompt texts — byte-identical to the legacy TypeScript.
//!
//! Source of record: `src/agent-instructions.ts`, `src/tools/timeout.ts`
//! and `src/memory-document.ts` in the legacy tree, extracted mechanically
//! (a template-literal scanner, never hand transcription) on 2026-11-17:
//! every `${...}` slot was resolved from the same source constants before
//! the bytes landed here. The frozen assembly (`buildAgentSystem`) that
//! wraps these texts with live context is the runtime slice; these are the
//! texts it must never alter.

/// The agent's instructions (`AGENT_INSTRUCTIONS`, guidance inlined).
pub const AGENT_INSTRUCTIONS: &str = include_str!("prompts/agent-instructions.txt");
/// The machine custodian's instructions (`CUSTODIAN_INSTRUCTIONS`).
pub const CUSTODIAN_INSTRUCTIONS: &str = include_str!("prompts/custodian-instructions.txt");
/// A bounded worker's instructions (`WORKER_INSTRUCTIONS`).
pub const WORKER_INSTRUCTIONS: &str = include_str!("prompts/worker-instructions.txt");
/// A saved-schedule run's own note (`BACKGROUND_INSTRUCTIONS`).
pub const BACKGROUND_INSTRUCTIONS: &str = include_str!("prompts/background-instructions.txt");
/// The persistent-memory writing contract (`MEMORY_DOCUMENT_GUIDANCE`).
pub const MEMORY_DOCUMENT_GUIDANCE: &str = include_str!("prompts/memory-document-guidance.txt");
/// The host-owned timeout guidance inlined into tool schemas
/// (`TOOL_TIMEOUT_GUIDANCE`, constants resolved).
pub const TOOL_TIMEOUT_GUIDANCE: &str = include_str!("prompts/tool-timeout-guidance.txt");

/// Which layer reads a prompt (legacy `AgentRole`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptRole {
    Agent,
    Custodian,
    Worker,
}

/// Which prompt a role receives (legacy `AgentBase`).
#[derive(Debug, Clone, Copy)]
pub struct AgentBase {
    pub role: PromptRole,
    pub instructions: &'static str,
}

pub const AGENT_BASE: AgentBase = AgentBase {
    role: PromptRole::Agent,
    instructions: AGENT_INSTRUCTIONS,
};

pub const CUSTODIAN_BASE: AgentBase = AgentBase {
    role: PromptRole::Custodian,
    instructions: CUSTODIAN_INSTRUCTIONS,
};

pub const WORKER_BASE: AgentBase = AgentBase {
    role: PromptRole::Worker,
    instructions: WORKER_INSTRUCTIONS,
};

/// `BACKGROUND_POLICY`: the common policy with the run's own note on top —
/// the exact composition of the legacy template (the separator and the
/// heading are part of the contract).
pub fn background_policy() -> String {
    format!("{AGENT_INSTRUCTIONS}\n\n## This scheduled run\n{BACKGROUND_INSTRUCTIONS}")
}

// --- the frozen assembly (`buildAgentSystem`) --------------------------------

/// The assembly templates, extracted from `agent.ts` with every `${...}`
/// body preserved as a `\0…\0` marker. Markers fill **positionally** —
/// count-checked before a single byte is written, so the expr text never
/// has to be re-typed and a shifted value can never slip through silently.
const ASSEMBLY_INSTRUCTIONS: &str = include_str!("prompts/assembly-instructions.txt");
const ASSEMBLY_REF_MEMORY_INDEX: &str = include_str!("prompts/assembly-ref-memory-index.txt");
const ASSEMBLY_REF_SKILL_INDEX: &str = include_str!("prompts/assembly-ref-skill-index.txt");
const ASSEMBLY_REF_USER_CONTEXT: &str = include_str!("prompts/assembly-ref-user-context.txt");
const ASSEMBLY_WRAP: &str = include_str!("prompts/assembly-wrap.txt");

/// Fallbacks the source applies with `||` at render time.
const NO_MEMORY_INDEX: &str = "(No memory index entries are saved.)";
const NO_SKILL_INDEX: &str =
    "(No skills found in the global or active-workspace skill directories.)";
const NO_SKILL_ROOT: &str = "(unavailable)";

/// Every live value the frozen assembly interpolates. The wrappers supply
/// these from their own slice (memory paths, skill discovery, team block);
/// this struct is the seam — no string surgery exists outside it.
#[derive(Debug, Clone, Default)]
pub struct SystemPromptContext {
    pub openbutler_skill_dir: String,
    /// `None` renders the source's `?? '(unavailable)'`.
    pub workspace_skill_dir: Option<String>,
    pub memory_root: String,
    pub index_file: String,
    pub preferences_file: String,
    pub preferences_availability: String,
    pub user_file: String,
    pub user_availability: String,
    pub topics_directory: String,
    pub areas_directory: String,
    pub workspace_file: String,
    pub workspace_availability: String,
    pub workspace_root: String,
    pub scratch_root: String,
    pub config_path: String,
    /// `None` renders the source's `|| '(No …)'` fallbacks.
    pub memory_index: Option<String>,
    pub skill_index: Option<String>,
    /// The whole loaded user-context section; `None` omits it (the source's
    /// `if (loadedUserContext)` gate).
    pub loaded_user_context: Option<String>,
    /// The role's team block, pushed as the first reference section.
    pub team_block: Option<String>,
}

/// Fills a template's positional markers; the marker count must equal the
/// value count or the assembly is refused (`None`).
fn fill(template: &str, values: &[&str]) -> Option<String> {
    let markers = template.matches('\0').count();
    if markers != values.len() * 2 {
        return None;
    }
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    let mut next = values.iter();
    while let Some(start) = rest.find('\0') {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 1..];
        let end = tail.find('\0')?;
        out.push_str(next.next()?);
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// The frozen system prompt: the role's instructions wrapped with the live
/// context, byte-for-byte as `buildAgentSystem` renders it (the wrapper's
/// join is `\n\n`, sections in the source's order).
pub fn build_agent_system(base: AgentBase, context: &SystemPromptContext) -> Option<String> {
    let workspace_skill_dir = context
        .workspace_skill_dir
        .clone()
        .unwrap_or_else(|| NO_SKILL_ROOT.to_owned());
    let instructions = fill(
        ASSEMBLY_INSTRUCTIONS,
        &[
            base.instructions,
            &context.openbutler_skill_dir,
            &workspace_skill_dir,
            &context.memory_root,
            &context.index_file,
            &context.preferences_file,
            &context.preferences_availability,
            &context.user_file,
            &context.user_availability,
            &context.topics_directory,
            &context.areas_directory,
            &context.workspace_file,
            &context.workspace_availability,
            MEMORY_DOCUMENT_GUIDANCE,
            &context.workspace_root,
            &context.scratch_root,
            &context.config_path,
        ],
    )?;

    let memory_index = context.memory_index.as_deref().unwrap_or(NO_MEMORY_INDEX);
    let skill_index = context.skill_index.as_deref().unwrap_or(NO_SKILL_INDEX);
    let mut reference: Vec<String> = Vec::new();
    if let Some(team_block) = &context.team_block {
        reference.push(team_block.clone());
    }
    reference.push(fill(
        ASSEMBLY_REF_MEMORY_INDEX,
        &[&context.index_file, memory_index],
    )?);
    reference.push(fill(ASSEMBLY_REF_SKILL_INDEX, &[skill_index])?);
    if let Some(user_context) = &context.loaded_user_context {
        reference.push(fill(ASSEMBLY_REF_USER_CONTEXT, &[user_context])?);
    }
    let reference = reference.join("\n\n");

    fill(ASSEMBLY_WRAP, &[&instructions, &reference])
}
