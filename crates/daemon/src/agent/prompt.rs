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
