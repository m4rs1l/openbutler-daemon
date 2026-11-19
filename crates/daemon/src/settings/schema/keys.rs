//! Moved from settings/schema.rs by the split (see dispatch-style refactor notes).
/// Top-level keys a settings document may carry (20, insertion order).
pub const KNOWN_KEYS: [&str; 20] = [
    "providers",
    "mcpServers",
    "providerOrder",
    "defaultProvider",
    "defaultModel",
    "reasoning",
    "showReasoning",
    "theme",
    "approvalMode",
    "autoReviewTimeoutSeconds",
    "autoReviewProvider",
    "autoReviewModel",
    "roles",
    "approvalTimeoutSeconds",
    "commandShell",
    "defaultWorkspaceRoot",
    "daemon",
    "telegram",
    "team",
    "custodian",
];

/// Fields one provider entry may carry (10).
pub const KNOWN_PROVIDER_KEYS: [&str; 10] = [
    "enabled",
    "keys",
    "models",
    "url",
    "npm",
    "options",
    "routing",
    "modelRouting",
    "payload",
    "modelSpecs",
];

/// Top-level keys `config.yaml` may carry — secrets only, nothing else.
pub const KNOWN_SECRET_KEYS: [&str; 3] = ["providers", "teamTicket", "modal"];

/// Every accepted non-path value of `commandShell`.
pub const SHELL_PRESETS: [&str; 9] = [
    "auto",
    "pwsh",
    "powershell",
    "cmd",
    "git-bash",
    "wsl",
    "bash",
    "zsh",
    "sh",
];

pub(super) const KNOWN_PROVIDER_SECRET_KEYS: [&str; 1] = ["keys"];
pub(super) const CUSTODIAN_KEYS: [&str; 6] = [
    "provider",
    "model",
    "contextLimitTokens",
    "reportLine",
    "reportTurnGapSeconds",
    "workerCaps",
];
pub(super) const CUSTODIAN_WORKER_CAP_KEYS: [&str; 4] =
    ["perSession", "perWorkerParent", "perMachine", "depth"];
pub(super) const TEAM_KEYS: [&str; 7] = [
    "role",
    "teamName",
    "memberName",
    "hubHost",
    "hubPort",
    "hubFingerprint",
    "teamId",
];
pub(super) const TELEGRAM_KEYS: [&str; 5] =
    ["enabled", "tokenEnv", "workspaceRoot", "chatId", "userId"];
pub(super) const MODEL_SPEC_KEYS: [&str; 5] = ["name", "context", "output", "reasoning", "payload"];

pub(super) const CUSTODIAN_CONTEXT_LIMIT_RANGE: (u64, u64) = (10_000, 2_000_000);
pub(super) const CUSTODIAN_REPORT_GAP_RANGE: (u64, u64) = (0, 3_600);

// --- Value helpers -----------------------------------------------------------

pub(super) fn known(keys: &[&str]) -> String {
    keys.join(", ")
}
