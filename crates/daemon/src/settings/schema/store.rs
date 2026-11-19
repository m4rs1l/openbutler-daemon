//! Moved from settings/schema.rs by the split (see dispatch-style refactor notes).
use std::collections::BTreeMap;

use super::{
    ApprovalMode, CustodianSettings, McpServerState, ProviderState, RoleBinding, TeamSettings,
    TelegramSettings, Theme,
};
/// The persisted settings document — the store every load/save moves.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsStore {
    pub providers: BTreeMap<String, ProviderState>,
    pub mcp_servers: BTreeMap<String, McpServerState>,
    pub provider_order: Vec<String>,
    pub default_provider: String,
    pub default_model: String,
    pub reasoning: Option<String>,
    pub show_reasoning: Option<bool>,
    pub theme: Option<Theme>,
    pub approval_mode: ApprovalMode,
    pub auto_review_timeout_seconds: Option<u64>,
    pub auto_review_provider: Option<String>,
    pub auto_review_model: Option<String>,
    pub roles: Option<BTreeMap<String, RoleBinding>>,
    pub approval_timeout_seconds: Option<u64>,
    pub command_shell: Option<String>,
    pub default_workspace_root: Option<String>,
    pub daemon: Option<DaemonSettings>,
    pub telegram: Option<TelegramSettings>,
    pub team: Option<TeamSettings>,
    pub custodian: Option<CustodianSettings>,
}

/// The saved remote-listener preference, validated by the same rule the
/// daemon binds by.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
/// The `settings.yaml` file shape (daemon section plus the store).
pub struct DaemonSettings {
    pub listen: String,
}

/// The empty machine: no providers, no MCP servers, folder-scoped approval.
pub fn default_settings() -> SettingsStore {
    SettingsStore {
        providers: BTreeMap::new(),
        mcp_servers: BTreeMap::new(),
        provider_order: Vec::new(),
        default_provider: String::new(),
        default_model: String::new(),
        reasoning: None,
        show_reasoning: None,
        theme: None,
        approval_mode: ApprovalMode::Workspace,
        auto_review_timeout_seconds: None,
        auto_review_provider: None,
        auto_review_model: None,
        roles: None,
        approval_timeout_seconds: None,
        command_shell: None,
        default_workspace_root: None,
        daemon: None,
        telegram: None,
        team: None,
        custodian: None,
    }
}
