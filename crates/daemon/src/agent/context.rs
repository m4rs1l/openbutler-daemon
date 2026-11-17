//! The runtime half of the system prompt: fills `SystemPromptContext`
//! from disk and settings for one session, then assembles the frozen
//! prompt around it.
//!
//! Sources: legacy `src/agent.ts` L48-L127 (`buildAgentSystem`
//! composition), L144 (`memoryProfileAvailability`), L159
//! (`buildTeamBlock`). Team membership arrives as an input — the settings
//! layer loads it, this module only renders it, so prompt building never
//! performs hidden I/O beyond the reads documented here.

use std::path::PathBuf;

use super::prompt::{
    AGENT_BASE, AgentBase, CUSTODIAN_BASE, PromptRole, SystemPromptContext, WORKER_BASE,
    build_agent_system,
};
use super::providers::{
    MEMORY_USER_CONTEXT_MAX_BYTES, TeamInfo, build_team_block, discover_skills, format_skill_index,
    memory_paths, openbutler_skill_dir, profile_availability, read_memory_file, read_memory_index,
    scratch_root, skill_roots, truncation_notice, workspace_skill_directory,
};

/// Everything the prompt reads from outside its own text. The runtime
/// owns these values; the struct only carries them in.
#[derive(Debug, Clone)]
pub struct PromptEnvironment {
    pub data_root: PathBuf,
    pub workspace_root: PathBuf,
    pub home: PathBuf,
    pub codex_home: Option<PathBuf>,
    pub builtin_skill_dir: Option<PathBuf>,
    pub team: Option<TeamInfo>,
}

/// The role's own base (legacy's `base` argument: one layer owns its
/// instructions instead of layering on another).
fn base_for(role: PromptRole) -> AgentBase {
    match role {
        PromptRole::Agent => AGENT_BASE,
        PromptRole::Custodian => CUSTODIAN_BASE,
        PromptRole::Worker => WORKER_BASE,
    }
}

/// Fills the context struct from disk — the seam the assembly fills.
/// Unexpected I/O while discovering skills bubbles (the source throws);
/// the memory reads load empty on failure by design.
pub fn build_context(
    role: PromptRole,
    env: &PromptEnvironment,
) -> Result<SystemPromptContext, std::io::Error> {
    let paths = memory_paths(&env.data_root, &env.workspace_root);
    let memory_index = read_memory_index(&paths.index_file);

    let preferences = read_memory_file(&paths.preferences_file, MEMORY_USER_CONTEXT_MAX_BYTES);
    let profile = read_memory_file(&paths.user_file, MEMORY_USER_CONTEXT_MAX_BYTES);
    let preferences_availability =
        profile_availability(&paths.preferences_file, &paths.root, Some(&preferences));
    let user_availability = profile_availability(&paths.user_file, &paths.root, Some(&profile));

    // Legacy: filter empty parts, append the notice to a cut part, join
    // with a blank line; nothing left means the section is omitted.
    let loaded_parts: Vec<String> = [preferences, profile]
        .into_iter()
        .filter(|part| !part.text.is_empty())
        .map(|part| {
            if part.truncated {
                format!("{}\n\n{}", part.text, truncation_notice(&part))
            } else {
                part.text
            }
        })
        .collect();
    let loaded_user_context = if loaded_parts.is_empty() {
        None
    } else {
        Some(loaded_parts.join("\n\n"))
    };

    let roots = skill_roots(
        &env.data_root,
        &env.workspace_root,
        &env.home,
        env.codex_home.as_deref(),
        env.builtin_skill_dir.clone(),
    );
    let skills = discover_skills(&roots)?;
    let skill_index = format_skill_index(&skills);
    let workspace_skill_dir = workspace_skill_directory(&roots);

    let team_block = build_team_block(role, env.team.as_ref(), &paths.peers_directory);

    Ok(SystemPromptContext {
        openbutler_skill_dir: openbutler_skill_dir(&env.data_root).display().to_string(),
        workspace_skill_dir: workspace_skill_dir.map(|path| path.display().to_string()),
        memory_root: paths.root.display().to_string(),
        index_file: paths.index_file.display().to_string(),
        preferences_file: paths.preferences_file.display().to_string(),
        preferences_availability,
        user_file: paths.user_file.display().to_string(),
        user_availability,
        topics_directory: paths.topics_directory.display().to_string(),
        areas_directory: paths.areas_directory.display().to_string(),
        workspace_file: paths.workspace_file.display().to_string(),
        workspace_availability: profile_availability(&paths.workspace_file, &paths.root, None),
        workspace_root: env.workspace_root.display().to_string(),
        scratch_root: scratch_root().display().to_string(),
        config_path: crate::settings::config_path(&env.data_root)
            .display()
            .to_string(),
        memory_index: (!memory_index.is_empty()).then_some(memory_index),
        skill_index: (!skill_index.is_empty()).then_some(skill_index),
        loaded_user_context,
        team_block: (!team_block.is_empty()).then_some(team_block),
    })
}

/// The frozen system prompt for one session: the role's instructions
/// wrapped with everything `build_context` read. `Ok(None)` means the
/// assembly refused (marker counts must match — impossible unless a
/// template changed under the code).
pub fn build_system(
    role: PromptRole,
    env: &PromptEnvironment,
) -> Result<Option<String>, std::io::Error> {
    let context = build_context(role, env)?;
    Ok(build_agent_system(base_for(role), &context))
}

/// Builds the turn dispatch hands to the executor: the system assembled
/// from disk, the provider resolved by the caller, and the chat's
/// workspace travelling alongside for the model-only label.
pub struct TurnInputs<'a> {
    pub meta: &'a openbutler_protocol::session::SessionMeta,
    pub workspace_root: Option<String>,
    pub provider_url: Option<String>,
    pub provider_api_key: Option<String>,
    pub team: Option<TeamInfo>,
    pub home: Option<&'a std::path::Path>,
    pub source: openbutler_protocol::entry::UserEntrySource,
    pub text: String,
    pub images: Vec<super::QueuedImage>,
    /// The approval policy and shell setting from settings at the turn
    /// boundary — the tool gate and the catalog description both read
    /// them off the request, never off a hidden load.
    pub approval_mode: crate::settings::ApprovalMode,
    pub command_shell: Option<String>,
}

pub fn prepare_turn(
    data_dir: &std::path::Path,
    inputs: TurnInputs<'_>,
) -> Result<super::TurnRequest, String> {
    let TurnInputs {
        meta,
        workspace_root,
        provider_url,
        provider_api_key,
        team,
        home,
        source,
        text,
        images,
        approval_mode,
        command_shell,
    } = inputs;
    let home = home
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(default_home);
    // The label travels only when a workspace is known; the assembly
    // still needs a root, so it falls back to the process directory.
    let env_workspace = workspace_root.clone().unwrap_or_else(|| {
        std::env::current_dir()
            .map(|path| path.display().to_string())
            .unwrap_or_default()
    });
    let env = PromptEnvironment {
        data_root: data_dir.to_path_buf(),
        workspace_root: PathBuf::from(env_workspace),
        home,
        codex_home: None,
        builtin_skill_dir: None,
        team,
    };
    let system = build_system(PromptRole::Agent, &env).map_err(|error| error.to_string())?;
    Ok(super::TurnRequest {
        text,
        source,
        provider_key: meta.provider_id.clone(),
        provider_url,
        provider_api_key,
        model: meta.model_id.clone(),
        system,
        workspace_root,
        images,
        attachments: Vec::new(),
        approval_mode,
        command_shell,
    })
}

/// The user's home the way the source resolves it: environment first.
fn default_home() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}
