//! Closed schemas: what each settings document may say, and the exact
//! refusal for everything else.
//!
//! A typo that silently fell back to a default is a bug that surfaces days
//! later; here it fails at load with the known set named. Source:
//! `settings.ts` L226-L1150 (read 2026-11-16). Every message is the
//! contract — tests pin them byte for byte.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::SettingsError;
use crate::listen_target::{ListenTargetError, parse_listen_target};
use crate::shape::{as_record, is_absolute_path, is_id_token, js_len};

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

const KNOWN_PROVIDER_SECRET_KEYS: [&str; 1] = ["keys"];
const CUSTODIAN_KEYS: [&str; 6] = [
    "provider",
    "model",
    "contextLimitTokens",
    "reportLine",
    "reportTurnGapSeconds",
    "workerCaps",
];
const CUSTODIAN_WORKER_CAP_KEYS: [&str; 4] =
    ["perSession", "perWorkerParent", "perMachine", "depth"];
const TEAM_KEYS: [&str; 7] = [
    "role",
    "teamName",
    "memberName",
    "hubHost",
    "hubPort",
    "hubFingerprint",
    "teamId",
];
const TELEGRAM_KEYS: [&str; 5] = ["enabled", "tokenEnv", "workspaceRoot", "chatId", "userId"];
const MODEL_SPEC_KEYS: [&str; 5] = ["name", "context", "output", "reasoning", "payload"];

const CUSTODIAN_CONTEXT_LIMIT_RANGE: (u64, u64) = (10_000, 2_000_000);
const CUSTODIAN_REPORT_GAP_RANGE: (u64, u64) = (0, 3_600);

// --- Value helpers -----------------------------------------------------------

fn known(keys: &[&str]) -> String {
    keys.join(", ")
}

/// `Array.isArray` + every item a string — the legacy `stringArray`.
fn string_array(value: &Value, where_: &str) -> Result<Vec<String>, SettingsError> {
    let Some(items) = value.as_array() else {
        return Err(SettingsError(format!("{where_} must be a list of strings")));
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Some(text) = item.as_str() else {
            return Err(SettingsError(format!("{where_} must be a list of strings")));
        };
        out.push(text.to_owned());
    }
    Ok(out)
}

/// The compact JSON size the source measures with `JSON.stringify`.
fn json_size(value: &Value) -> Result<usize, SettingsError> {
    crate::shape::json_size(value).map_err(SettingsError)
}

/// `/^[A-Za-z_][A-Za-z0-9_]*$/` — an environment-variable name.
fn is_environment_name(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// `/^[A-Za-z0-9][A-Za-z0-9_-]{0,58}$/` — MCP server ids (59 max).
fn is_mcp_id(value: &str) -> bool {
    is_id_token(value, 59)
}

/// `/^[!#$%&'*+.^_\`|~0-9A-Za-z-]+$/` — an HTTP header field name.
fn is_header_name(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                        | b'-'
                )
        })
}

/// The opaque-option-bag rule: a mapping of bounded size with no prototype
/// keys, because these reach `import()` arguments and request headers. Array
/// elements keep the source's pass-through (only object keys are walked).
fn opaque_bounded(value: &Value, where_: &str) -> Result<Value, SettingsError> {
    let Some(map) = as_record(value) else {
        return Err(SettingsError(format!(
            "{where_} must be a mapping of option names to values"
        )));
    };
    let size = json_size(value)?;
    if size > 16_000 {
        return Err(SettingsError(format!(
            "{where_} must serialize under 16000 characters (got {size})."
        )));
    }
    reject_prototype_keys(map, where_)?;
    Ok(value.clone())
}

fn reject_prototype_keys(map: &Map<String, Value>, path: &str) -> Result<(), SettingsError> {
    for (key, item) in map {
        if matches!(key.as_str(), "__proto__" | "prototype" | "constructor") {
            return Err(SettingsError(format!(
                "{path}.{key} is not an allowed option name."
            )));
        }
        if let Some(nested) = as_record(item) {
            reject_prototype_keys(nested, &format!("{path}.{key}"))?;
        }
    }
    Ok(())
}

/// `Number.isInteger` + inclusive range, one message for every failure mode.
fn whole_number_in_range(
    value: &Value,
    name: &str,
    min: u64,
    max: u64,
) -> Result<u64, SettingsError> {
    let number = value.as_u64().filter(|number| (min..=max).contains(number));
    number.ok_or_else(|| {
        SettingsError(format!(
            "{name} must be a whole number between {min} and {max}."
        ))
    })
}

fn integer_in_range(value: &Value, name: &str, min: u64, max: u64) -> Result<u64, SettingsError> {
    whole_number_in_range(value, name, min, max)
}

/// The two approval timeouts: whole seconds in 5..=600, with the message
/// wording the contract pins ("of seconds" is part of it).
fn timeout_seconds(value: &Value, name: &str) -> Result<u64, SettingsError> {
    let number = value.as_u64().filter(|number| (5..=600).contains(number));
    number.ok_or_else(|| {
        SettingsError(format!(
            "{name} must be a whole number of seconds between 5 and 600."
        ))
    })
}

// --- The store ---------------------------------------------------------------

/// Theme presentation (`Dark` | `Light` | `System`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Dark,
    Light,
    System,
}

impl Theme {
    pub fn as_str(self) -> &'static str {
        match self {
            Theme::Dark => "Dark",
            Theme::Light => "Light",
            Theme::System => "System",
        }
    }
}

/// Approval policy for tool calls. The original `ask` value migrates to
/// `workspace` at load; nothing else is coerced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalMode {
    Workspace,
    AutoReview,
    FullAccess,
}

impl ApprovalMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalMode::Workspace => "workspace",
            ApprovalMode::AutoReview => "auto-review",
            ApprovalMode::FullAccess => "full-access",
        }
    }
}

/// Team membership role (`hub` | `member`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TeamRole {
    Hub,
    Member,
}

/// One provider entry — the schema owns the field list (`KNOWN_PROVIDER_KEYS`).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderState {
    pub enabled: bool,
    pub keys: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub npm: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routing: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_routing: Option<BTreeMap<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_specs: Option<BTreeMap<String, ProviderModelSpec>>,
}

/// Settings-side description of one custom model.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderModelSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_efforts: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
}

/// Transport of one MCP server: `stdio` (command/args/env/cwd) or an HTTP
/// style (`http` | `sse` with url/headers).
#[derive(Debug, Clone, PartialEq)]
pub enum McpServerState {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: Option<String>,
    },
    Http {
        transport: &'static str,
        url: String,
        headers: BTreeMap<String, String>,
    },
}

/// The wire form is one flat object with a `transport` discriminator; the
/// `http` variant carries its own `http`/`sse` value, so the tag cannot be
/// derived and the encoder is written out.
impl serde::Serialize for McpServerState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            McpServerState::Stdio {
                command,
                args,
                env,
                cwd,
            } => {
                let mut map = serializer.serialize_map(Some(4 + usize::from(cwd.is_some())))?;
                map.serialize_entry("transport", "stdio")?;
                map.serialize_entry("command", command)?;
                map.serialize_entry("args", args)?;
                map.serialize_entry("env", env)?;
                if let Some(cwd) = cwd {
                    map.serialize_entry("cwd", cwd)?;
                }
                map.end()
            }
            McpServerState::Http {
                transport,
                url,
                headers,
            } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("transport", transport)?;
                map.serialize_entry("url", url)?;
                map.serialize_entry("headers", headers)?;
                map.end()
            }
        }
    }
}

/// Telegram channel settings. `token_env` is an environment-variable *name*;
/// a literal token in the document is rejected at parse.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TelegramSettings {
    pub enabled: bool,
    pub token_env: String,
    pub workspace_root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

/// Non-secret team membership. The bearer ticket lives in `config.yaml`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamSettings {
    pub role: TeamRole,
    pub team_name: String,
    pub member_name: String,
    pub hub_host: String,
    pub hub_port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hub_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
}

/// Worker caps of the custodian, filled per field when the block is present.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerCaps {
    pub per_session: u64,
    pub per_worker_parent: u64,
    pub per_machine: u64,
    pub depth: u64,
}

/// The head's settings, validated where they are read: a wrong value is
/// refused by name and range instead of silently becoming the default.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CustodianSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_limit_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_line: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_turn_gap_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_caps: Option<WorkerCaps>,
}

/// A role → provider/model binding; whole pairs only.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoleBinding {
    pub provider: String,
    pub model: String,
}

/// The persisted configuration union (the non-secret half).
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

/// The secret half: provider keys plus the team ticket and Modal
/// credentials — nothing else, ever.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SecretsStore {
    pub providers: BTreeMap<String, ProviderSecrets>,
    pub team_ticket: Option<String>,
    pub modal: Option<ModalCredentials>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ProviderSecrets {
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModalCredentials {
    pub token_id: String,
    pub token_secret: String,
}

// --- Section parsers ---------------------------------------------------------

/// Parses a settings document that is already known to be a mapping.
pub fn parse_store(doc: &Map<String, Value>) -> Result<SettingsStore, SettingsError> {
    for key in doc.keys() {
        if !KNOWN_KEYS.contains(&key.as_str()) {
            return Err(SettingsError(format!(
                "{key} is not a setting openbutler knows (known: {})",
                known(&KNOWN_KEYS)
            )));
        }
    }

    let mut settings = default_settings();

    if let Some(raw_providers) = doc.get("providers").and_then(as_record) {
        for (id, raw) in raw_providers {
            settings
                .providers
                .insert(id.clone(), parse_provider_state(raw, id)?);
        }
    }
    if let Some(raw_mcp) = doc.get("mcpServers") {
        let Some(record) = as_record(raw_mcp) else {
            return Err(SettingsError("mcpServers must be a mapping".to_owned()));
        };
        for (id, raw) in record {
            settings
                .mcp_servers
                .insert(id.clone(), parse_mcp_server_state(raw, id)?);
        }
    }

    if let Some(raw_order) = doc.get("providerOrder") {
        let order = string_array(raw_order, "providerOrder")?;
        settings.provider_order = order
            .into_iter()
            .filter(|id| settings.providers.contains_key(id))
            .collect();
    }
    for id in settings.providers.keys() {
        if !settings.provider_order.contains(id) {
            settings.provider_order.push(id.clone());
        }
    }

    if let Some(value) = doc.get("defaultProvider").and_then(Value::as_str) {
        settings.default_provider = value.to_owned();
    }
    if let Some(value) = doc.get("defaultModel").and_then(Value::as_str) {
        settings.default_model = value.to_owned();
    }
    if let Some(value) = doc.get("reasoning") {
        let Some(text) = value.as_str() else {
            return Err(SettingsError(
                "reasoning must be a valid model variant id".to_owned(),
            ));
        };
        if !is_variant_id(text) {
            return Err(SettingsError(
                "reasoning must be a valid model variant id".to_owned(),
            ));
        }
        settings.reasoning = Some(text.to_owned());
    }
    if let Some(value) = doc.get("showReasoning") {
        let Some(flag) = value.as_bool() else {
            return Err(SettingsError("showReasoning must be a boolean".to_owned()));
        };
        settings.show_reasoning = Some(flag);
    }
    if let Some(value) = doc.get("theme") {
        let Some(text) = value.as_str() else {
            return Err(SettingsError(
                "theme must be Dark, Light, or System".to_owned(),
            ));
        };
        settings.theme = Some(match text {
            "Dark" => Theme::Dark,
            "Light" => Theme::Light,
            "System" => Theme::System,
            _ => {
                return Err(SettingsError(
                    "theme must be Dark, Light, or System".to_owned(),
                ));
            }
        });
    }
    if let Some(value) = doc.get("commandShell") {
        let shell = value.as_str().map(str::trim).unwrap_or("");
        if !is_shell_setting_value(shell) {
            return Err(SettingsError(format!(
                "commandShell must be one of {} or an executable path",
                SHELL_PRESETS.join(", ")
            )));
        }
        settings.command_shell = Some(shell.to_owned());
    }
    if let Some(value) = doc.get("daemon") {
        let Some(record) = as_record(value) else {
            return Err(SettingsError("daemon must be a mapping".to_owned()));
        };
        for key in record.keys() {
            if key != "listen" {
                return Err(SettingsError(format!(
                    "{key} is not a daemon setting openbutler knows (known: listen)"
                )));
            }
        }
        if let Some(listen) = record.get("listen") {
            let Some(text) = listen.as_str() else {
                return Err(SettingsError("daemon.listen must be a string".to_owned()));
            };
            parse_listen_target(text)
                .map_err(|error: ListenTargetError| SettingsError(error.to_string()))?;
            settings.daemon = Some(DaemonSettings {
                listen: text.to_owned(),
            });
        }
    }
    if let Some(value) = doc.get("approvalMode") {
        let Some(text) = value.as_str() else {
            return Err(SettingsError(
                "approvalMode must be workspace, auto-review, or full-access".to_owned(),
            ));
        };
        settings.approval_mode = match text {
            "ask" => ApprovalMode::Workspace,
            "workspace" => ApprovalMode::Workspace,
            "auto-review" => ApprovalMode::AutoReview,
            "full-access" => ApprovalMode::FullAccess,
            _ => {
                return Err(SettingsError(
                    "approvalMode must be workspace, auto-review, or full-access".to_owned(),
                ));
            }
        };
    }
    if let Some(value) = doc.get("autoReviewTimeoutSeconds") {
        settings.auto_review_timeout_seconds =
            Some(timeout_seconds(value, "autoReviewTimeoutSeconds")?);
    }
    if doc.contains_key("autoReviewProvider") || doc.contains_key("autoReviewModel") {
        let provider = doc.get("autoReviewProvider");
        let model = doc.get("autoReviewModel");
        match (provider, model) {
            (None, None) => {}
            (Some(Value::Null), _) | (_, Some(Value::Null)) => {
                settings.auto_review_provider = None;
                settings.auto_review_model = None;
            }
            _ => {
                let valid = |value: Option<&Value>| matches!(value, Some(Value::String(text)) if !text.trim().is_empty());
                if !(valid(provider) && valid(model)) {
                    return Err(SettingsError(
                        "autoReviewProvider and autoReviewModel must both be non-empty strings, or both omitted."
                            .to_owned(),
                    ));
                }
                settings.auto_review_provider = Some(
                    provider
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_owned(),
                );
                settings.auto_review_model = Some(
                    model
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_owned(),
                );
            }
        }
    }
    if let Some(value) = doc.get("roles") {
        settings.roles = parse_roles(value)?;
    }
    if let Some(value) = doc.get("approvalTimeoutSeconds") {
        settings.approval_timeout_seconds = Some(timeout_seconds(value, "approvalTimeoutSeconds")?);
    }
    if let Some(value) = doc.get("defaultWorkspaceRoot") {
        let Some(text) = value.as_str() else {
            return Err(SettingsError(
                "defaultWorkspaceRoot must be an absolute path.".to_owned(),
            ));
        };
        if js_len(text) > 32_768 || !is_absolute_path(text) {
            return Err(SettingsError(
                "defaultWorkspaceRoot must be an absolute path.".to_owned(),
            ));
        }
        settings.default_workspace_root = Some(text.to_owned());
    }
    if let Some(value) = doc.get("telegram") {
        settings.telegram = Some(parse_telegram_settings(value)?);
    }
    if let Some(value) = doc.get("team") {
        settings.team = Some(parse_team_settings(value)?);
    }
    if let Some(value) = doc.get("custodian") {
        settings.custodian = Some(parse_custodian_settings(value)?);
    }

    // Read-time ingestion of the single-purpose fields written before roles
    // existed: from parse on there is one place every reader consults.
    if settings
        .roles
        .as_ref()
        .is_none_or(|roles| !roles.contains_key("review"))
        && settings.auto_review_provider.is_some()
        && settings.auto_review_model.is_some()
    {
        let roles = settings.roles.get_or_insert_with(BTreeMap::new);
        roles.insert(
            "review".to_owned(),
            RoleBinding {
                provider: settings.auto_review_provider.clone().unwrap_or_default(),
                model: settings.auto_review_model.clone().unwrap_or_default(),
            },
        );
    }
    if settings
        .roles
        .as_ref()
        .is_none_or(|roles| !roles.contains_key("custodian"))
        && let (Some(provider), Some(model)) = (
            settings
                .custodian
                .as_ref()
                .and_then(|c| c.provider.as_ref()),
            settings.custodian.as_ref().and_then(|c| c.model.as_ref()),
        )
    {
        let roles = settings.roles.get_or_insert_with(BTreeMap::new);
        roles.insert(
            "custodian".to_owned(),
            RoleBinding {
                provider: provider.clone(),
                model: model.clone(),
            },
        );
    }

    Ok(settings)
}

/// `/^[A-Za-z0-9:_-]{1,128}$/` — a model variant id.
fn is_variant_id(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= 128
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, ':' | '_' | '-'))
}

/// A preset name or anything path-shaped.
fn is_shell_setting_value(value: &str) -> bool {
    SHELL_PRESETS.contains(&value) || value.contains('/') || value.contains('\\')
}

fn parse_roles(value: &Value) -> Result<Option<BTreeMap<String, RoleBinding>>, SettingsError> {
    if value.is_null() {
        return Ok(None);
    }
    let Some(record) = as_record(value) else {
        return Err(SettingsError(
            "roles must be a mapping of role name to a { provider, model } pair.".to_owned(),
        ));
    };
    let mut roles = BTreeMap::new();
    for (role, raw) in record {
        if raw.is_null() {
            continue;
        }
        let Some(pair) = as_record(raw) else {
            return Err(SettingsError(format!(
                "roles.{role} must be a {{ provider, model }} pair."
            )));
        };
        let provider = pair.get("provider");
        let model = pair.get("model");
        if provider.is_none_or(Value::is_null) && model.is_none_or(Value::is_null) {
            continue;
        }
        let (Some(Value::String(provider)), Some(Value::String(model))) = (provider, model) else {
            return Err(SettingsError(format!(
                "roles.{role}.provider and roles.{role}.model must both be non-empty strings, or the role must be omitted."
            )));
        };
        if provider.trim().is_empty() || model.trim().is_empty() {
            return Err(SettingsError(format!(
                "roles.{role}.provider and roles.{role}.model must both be non-empty strings, or the role must be omitted."
            )));
        }
        roles.insert(
            role.clone(),
            RoleBinding {
                provider: provider.trim().to_owned(),
                model: model.trim().to_owned(),
            },
        );
    }
    Ok((!roles.is_empty()).then_some(roles))
}

fn parse_provider_state(raw: &Value, id: &str) -> Result<ProviderState, SettingsError> {
    let Some(record) = as_record(raw) else {
        return Err(SettingsError(format!("providers.{id} must be a mapping")));
    };
    for key in record.keys() {
        if !KNOWN_PROVIDER_KEYS.contains(&key.as_str()) {
            return Err(SettingsError(format!(
                "providers.{id}.{key} is not a setting openbutler knows (known: {})",
                known(&KNOWN_PROVIDER_KEYS)
            )));
        }
    }
    let mut state = ProviderState {
        enabled: record
            .get("enabled")
            .is_none_or(|value| value == &Value::Bool(true)),
        keys: match record.get("keys") {
            None => Vec::new(),
            Some(value) => string_array(value, &format!("providers.{id}.keys"))?,
        },
        models: None,
        url: None,
        npm: None,
        options: None,
        routing: None,
        model_routing: None,
        payload: None,
        model_specs: None,
    };
    if let Some(value) = record.get("models") {
        let models = string_array(value, &format!("providers.{id}.models"))?;
        if models.len() > 512
            || models
                .iter()
                .any(|model| model.trim().is_empty() || js_len(model) > 512)
        {
            return Err(SettingsError(format!(
                "providers.{id}.models must contain at most 512 non-empty model IDs under 512 characters each."
            )));
        }
        let mut unique = Vec::new();
        for model in models {
            let trimmed = model.trim().to_owned();
            if !unique.contains(&trimmed) {
                unique.push(trimmed);
            }
        }
        state.models = Some(unique);
    }
    if let Some(text) = record.get("url").and_then(Value::as_str)
        && !text.is_empty()
    {
        state.url = Some(text.to_owned());
    }
    if let Some(text) = record.get("npm").and_then(Value::as_str)
        && !text.is_empty()
    {
        state.npm = Some(text.to_owned());
    }
    if let Some(value) = record.get("options") {
        let options = opaque_bounded(value, &format!("providers.{id}.options"))?;
        if as_record(&options).is_some_and(|map| !map.is_empty()) {
            state.options = Some(options);
        }
    }
    if let Some(value) = record.get("routing") {
        let routing = opaque_bounded(value, &format!("providers.{id}.routing"))?;
        if as_record(&routing).is_some_and(|map| !map.is_empty()) {
            state.routing = Some(routing);
        }
    }
    if let Some(value) = record.get("modelRouting") {
        let entries = opaque_bounded(value, &format!("providers.{id}.modelRouting"))?;
        let entries_map = as_record(&entries).ok_or_else(|| {
            SettingsError(format!(
                "providers.{id}.modelRouting must be a mapping of option names to values"
            ))
        })?;
        let mut model_routing = BTreeMap::new();
        for (model_id, routing) in entries_map {
            if model_id.trim().is_empty() || js_len(model_id) > 512 {
                return Err(SettingsError(format!(
                    "providers.{id}.modelRouting requires non-empty model IDs under 512 characters."
                )));
            }
            let nested =
                opaque_bounded(routing, &format!("providers.{id}.modelRouting.{model_id}"))?;
            model_routing.insert(model_id.clone(), nested);
        }
        state.model_routing = Some(model_routing);
    }
    if let Some(value) = record.get("payload") {
        let payload = opaque_bounded(value, &format!("providers.{id}.payload"))?;
        if as_record(&payload).is_some_and(|map| !map.is_empty()) {
            state.payload = Some(payload);
        }
    }
    if let Some(value) = record.get("modelSpecs") {
        let Some(specs) = as_record(value) else {
            return Err(SettingsError(format!(
                "providers.{id}.modelSpecs must be a mapping"
            )));
        };
        let mut parsed = BTreeMap::new();
        for (model_id, raw_spec) in specs {
            if model_id.trim().is_empty() || js_len(model_id) > 512 {
                return Err(SettingsError(format!(
                    "providers.{id}.modelSpecs requires non-empty model IDs under 512 characters."
                )));
            }
            let Some(spec_record) = as_record(raw_spec) else {
                return Err(SettingsError(format!(
                    "providers.{id}.modelSpecs.{model_id} must be a mapping"
                )));
            };
            for key in spec_record.keys() {
                if !MODEL_SPEC_KEYS.contains(&key.as_str()) {
                    return Err(SettingsError(format!(
                        "providers.{id}.modelSpecs.{model_id}.{key} is not a model spec openbutler knows (known: {})",
                        known(&MODEL_SPEC_KEYS)
                    )));
                }
            }
            let mut spec = ProviderModelSpec::default();
            if let Some(value) = spec_record.get("name") {
                let Some(name) = value.as_str() else {
                    return Err(SettingsError(format!(
                        "providers.{id}.modelSpecs.{model_id}.name must use 1-128 characters."
                    )));
                };
                if name.trim().is_empty() || js_len(name) > 128 {
                    return Err(SettingsError(format!(
                        "providers.{id}.modelSpecs.{model_id}.name must use 1-128 characters."
                    )));
                }
                spec.name = Some(name.trim().to_owned());
            }
            for field in ["context", "output"] {
                let Some(value) = spec_record.get(field) else {
                    continue;
                };
                let number = value
                    .as_u64()
                    .filter(|number| *number >= 1 && *number <= 9_007_199_254_740_991);
                let Some(number) = number else {
                    return Err(SettingsError(format!(
                        "providers.{id}.modelSpecs.{model_id}.{field} must be a positive whole number of tokens."
                    )));
                };
                if field == "context" {
                    spec.context = Some(number);
                } else {
                    spec.output = Some(number);
                }
            }
            if let Some(value) = spec_record.get("reasoning") {
                let Some(reasoning) = as_record(value) else {
                    return Err(SettingsError(format!(
                        "providers.{id}.modelSpecs.{model_id}.reasoning must be a mapping"
                    )));
                };
                let efforts_raw = reasoning
                    .get("efforts")
                    .map(|value| {
                        string_array(
                            value,
                            &format!("providers.{id}.modelSpecs.{model_id}.reasoning.efforts"),
                        )
                    })
                    .transpose()?;
                let mut efforts = Vec::new();
                for effort in efforts_raw.unwrap_or_default() {
                    let trimmed = effort.trim().to_owned();
                    if !trimmed.is_empty() && !efforts.contains(&trimmed) {
                        efforts.push(trimmed);
                    }
                }
                if !efforts.is_empty() {
                    spec.reasoning_efforts = Some(efforts);
                }
            }
            if let Some(value) = spec_record.get("payload") {
                let payload = opaque_bounded(
                    value,
                    &format!("providers.{id}.modelSpecs.{model_id}.payload"),
                )?;
                if as_record(&payload).is_some_and(|map| !map.is_empty()) {
                    spec.payload = Some(payload);
                }
            }
            parsed.insert(model_id.clone(), spec);
        }
        if !parsed.is_empty() {
            state.model_specs = Some(parsed);
        }
    }
    Ok(state)
}

fn parse_mcp_server_state(raw: &Value, id: &str) -> Result<McpServerState, SettingsError> {
    if !is_mcp_id(id) {
        return Err(SettingsError(format!(
            "mcpServers.{id} has an invalid server id"
        )));
    }
    let Some(record) = as_record(raw) else {
        return Err(SettingsError(format!("mcpServers.{id} must be a mapping")));
    };
    match record.get("transport").and_then(Value::as_str) {
        Some("stdio") => {
            let allowed = ["transport", "command", "args", "env", "cwd"];
            for key in record.keys() {
                if !allowed.contains(&key.as_str()) {
                    return Err(SettingsError(format!(
                        "mcpServers.{id}.{key} is not a stdio setting"
                    )));
                }
            }
            let command = record.get("command").and_then(Value::as_str).unwrap_or("");
            if command.trim().is_empty() {
                return Err(SettingsError(format!(
                    "mcpServers.{id}.command must be a non-empty executable name"
                )));
            }
            let args = record
                .get("args")
                .map(|value| string_array(value, &format!("mcpServers.{id}.args")))
                .transpose()?
                .unwrap_or_default();
            let env = environment_references(
                record.get("env"),
                &format!("mcpServers.{id}.env"),
                is_environment_name,
            )?;
            let cwd = match record.get("cwd") {
                None | Some(Value::Null) => None,
                Some(Value::String(text)) if text.is_empty() => {
                    return Err(SettingsError(format!(
                        "mcpServers.{id}.cwd must be a non-empty path"
                    )));
                }
                Some(Value::String(text)) => Some(text.clone()),
                Some(_) => {
                    return Err(SettingsError(format!(
                        "mcpServers.{id}.cwd must be a non-empty path"
                    )));
                }
            };
            Ok(McpServerState::Stdio {
                command: command.to_owned(),
                args,
                env,
                cwd,
            })
        }
        Some("http") | Some("sse") => {
            let allowed = ["transport", "url", "headers"];
            for key in record.keys() {
                if !allowed.contains(&key.as_str()) {
                    return Err(SettingsError(format!(
                        "mcpServers.{id}.{key} is not an HTTP setting"
                    )));
                }
            }
            let Some(url_text) = record.get("url").and_then(Value::as_str) else {
                return Err(SettingsError(format!("mcpServers.{id}.url must be a URL")));
            };
            let Ok(url) = url::Url::parse(url_text) else {
                return Err(SettingsError(format!(
                    "mcpServers.{id}.url must be a valid HTTP or HTTPS URL"
                )));
            };
            if !matches!(url.scheme(), "http" | "https")
                || !url.username().is_empty()
                || url.password().is_some()
            {
                return Err(SettingsError(format!(
                    "mcpServers.{id}.url must use HTTP(S) and must not contain credentials"
                )));
            }
            let headers = environment_references(
                record.get("headers"),
                &format!("mcpServers.{id}.headers"),
                is_header_name,
            )?;
            Ok(McpServerState::Http {
                transport: if record.get("transport").and_then(Value::as_str) == Some("sse") {
                    "sse"
                } else {
                    "http"
                },
                url: url.to_string(),
                headers,
            })
        }
        _ => Err(SettingsError(format!(
            "mcpServers.{id}.transport must be stdio, http, or sse"
        ))),
    }
}

/// Maps names exposed to a server to host environment-variable names. A
/// literal secret in either half is rejected at parse.
fn environment_references(
    value: Option<&Value>,
    where_: &str,
    is_target_name: fn(&str) -> bool,
) -> Result<BTreeMap<String, String>, SettingsError> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let Some(record) = as_record(value) else {
        return Err(SettingsError(format!(
            "{where_} must map names to environment-variable names"
        )));
    };
    let mut out = BTreeMap::new();
    for (key, reference) in record {
        let reference_text = reference.as_str().unwrap_or("");
        if matches!(key.as_str(), "__proto__" | "prototype" | "constructor")
            || !is_target_name(key)
            || !reference.is_string()
            || !is_environment_name(reference_text)
        {
            return Err(SettingsError(format!(
                "{where_}.{key} must name a host environment variable"
            )));
        }
        out.insert(key.clone(), reference_text.to_owned());
    }
    Ok(out)
}

fn parse_telegram_settings(raw: &Value) -> Result<TelegramSettings, SettingsError> {
    let Some(record) = as_record(raw) else {
        return Err(SettingsError(
            "telegram contains an unsupported setting.".to_owned(),
        ));
    };
    for key in record.keys() {
        if !TELEGRAM_KEYS.contains(&key.as_str()) {
            return Err(SettingsError(
                "telegram contains an unsupported setting.".to_owned(),
            ));
        }
    }
    let enabled = match record.get("enabled") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => {
            return Err(SettingsError(
                "telegram.enabled must be a boolean.".to_owned(),
            ));
        }
    };
    let Some(token_env) = record.get("tokenEnv").and_then(Value::as_str) else {
        return Err(SettingsError(
            "telegram.tokenEnv must name an environment variable, not contain a token.".to_owned(),
        ));
    };
    if !is_environment_name(token_env) {
        return Err(SettingsError(
            "telegram.tokenEnv must name an environment variable, not contain a token.".to_owned(),
        ));
    }
    let Some(workspace_root) = record.get("workspaceRoot").and_then(Value::as_str) else {
        return Err(SettingsError(
            "telegram.workspaceRoot must be an absolute path.".to_owned(),
        ));
    };
    if !is_absolute_path(workspace_root) {
        return Err(SettingsError(
            "telegram.workspaceRoot must be an absolute path.".to_owned(),
        ));
    }
    if record.contains_key("chatId") != record.contains_key("userId") {
        return Err(SettingsError(
            "telegram chatId and userId must be paired together.".to_owned(),
        ));
    }
    let (chat_id, user_id) = match (record.get("chatId"), record.get("userId")) {
        (None, None) => (None, None),
        (Some(Value::String(chat)), Some(Value::String(user))) => {
            if !is_telegram_id(chat) {
                return Err(SettingsError(
                    "telegram.chatId must be a private chat id.".to_owned(),
                ));
            }
            if !is_telegram_id(user) {
                return Err(SettingsError("telegram.userId is invalid.".to_owned()));
            }
            (Some(chat.clone()), Some(user.clone()))
        }
        _ => {
            if record.contains_key("chatId") {
                return Err(SettingsError(
                    "telegram.chatId must be a private chat id.".to_owned(),
                ));
            }
            return Err(SettingsError("telegram.userId is invalid.".to_owned()));
        }
    };
    Ok(TelegramSettings {
        enabled,
        token_env: token_env.to_owned(),
        workspace_root: workspace_root.to_owned(),
        chat_id,
        user_id,
    })
}

/// `/^\d{1,20}$/` — a Telegram chat/user id.
fn is_telegram_id(value: &str) -> bool {
    (1..=20).contains(&value.chars().count()) && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn parse_team_settings(raw: &Value) -> Result<TeamSettings, SettingsError> {
    let Some(record) = as_record(raw) else {
        return Err(SettingsError("team must be a mapping.".to_owned()));
    };
    for key in record.keys() {
        if !TEAM_KEYS.contains(&key.as_str()) {
            return Err(SettingsError(format!(
                "team.{key} is not a supported setting."
            )));
        }
    }
    let role = match record.get("role").and_then(Value::as_str) {
        Some("hub") => TeamRole::Hub,
        Some("member") => TeamRole::Member,
        _ => return Err(SettingsError("team.role must be hub or member.".to_owned())),
    };
    let bounded = |field: &str, max: usize, message: &str| -> Result<String, SettingsError> {
        let Some(text) = record.get(field).and_then(Value::as_str) else {
            return Err(SettingsError(message.to_owned()));
        };
        if text.trim().is_empty() || js_len(text) > max {
            return Err(SettingsError(message.to_owned()));
        }
        Ok(text.to_owned())
    };
    let team_name = bounded("teamName", 64, "team.teamName must be 1-64 characters.")?;
    let member_name = bounded("memberName", 64, "team.memberName must be 1-64 characters.")?;
    let hub_host = bounded(
        "hubHost",
        512,
        "team.hubHost must be a non-empty host under 512 characters.",
    )?;
    let hub_port = record
        .get("hubPort")
        .ok_or_else(|| SettingsError("team.hubPort must be a port 1-65535.".to_owned()))?;
    // TS wraps every hubPort failure (type, integer, range) in the one port message.
    let hub_port = integer_in_range(hub_port, "team.hubPort", 1, 65_535)
        .map_err(|_| SettingsError("team.hubPort must be a port 1-65535.".to_owned()))?;
    let hub_fingerprint = match record.get("hubFingerprint") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if js_len(text) <= 64 => Some(text.clone()),
        Some(_) => {
            return Err(SettingsError(
                "team.hubFingerprint must be a short string.".to_owned(),
            ));
        }
    };
    let team_id = match record.get("teamId") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if js_len(text) <= 64 => Some(text.clone()),
        Some(_) => {
            return Err(SettingsError(
                "team.teamId must be a short string.".to_owned(),
            ));
        }
    };
    Ok(TeamSettings {
        role,
        team_name,
        member_name,
        hub_host,
        hub_port: hub_port as u16,
        hub_fingerprint,
        team_id,
    })
}

fn parse_custodian_settings(raw: &Value) -> Result<CustodianSettings, SettingsError> {
    let Some(record) = as_record(raw) else {
        return Err(SettingsError("custodian must be a mapping".to_owned()));
    };
    for key in record.keys() {
        if !CUSTODIAN_KEYS.contains(&key.as_str()) {
            return Err(SettingsError(format!(
                "custodian.{key} is not a setting openbutler knows (known: {})",
                known(&CUSTODIAN_KEYS)
            )));
        }
    }
    let mut settings = CustodianSettings::default();
    let provider = record.get("provider");
    let model = record.get("model");
    if provider.is_some() || model.is_some() {
        let (Some(Value::String(provider)), Some(Value::String(model))) = (provider, model) else {
            return Err(SettingsError(
                "custodian.provider and custodian.model must both be non-empty strings, or both omitted."
                    .to_owned(),
            ));
        };
        if provider.trim().is_empty() || model.trim().is_empty() {
            return Err(SettingsError(
                "custodian.provider and custodian.model must both be non-empty strings, or both omitted."
                    .to_owned(),
            ));
        }
        settings.provider = Some(provider.trim().to_owned());
        settings.model = Some(model.trim().to_owned());
    }
    if let Some(value) = record.get("contextLimitTokens") {
        settings.context_limit_tokens = Some(whole_number_in_range(
            value,
            "custodian.contextLimitTokens",
            CUSTODIAN_CONTEXT_LIMIT_RANGE.0,
            CUSTODIAN_CONTEXT_LIMIT_RANGE.1,
        )?);
    }
    if let Some(value) = record.get("reportLine") {
        let Some(flag) = value.as_bool() else {
            return Err(SettingsError(
                "custodian.reportLine must be true or false.".to_owned(),
            ));
        };
        settings.report_line = Some(flag);
    }
    if let Some(value) = record.get("reportTurnGapSeconds") {
        settings.report_turn_gap_seconds = Some(whole_number_in_range(
            value,
            "custodian.reportTurnGapSeconds",
            CUSTODIAN_REPORT_GAP_RANGE.0,
            CUSTODIAN_REPORT_GAP_RANGE.1,
        )?);
    }
    if let Some(value) = record.get("workerCaps") {
        let Some(caps) = as_record(value) else {
            return Err(SettingsError(
                "custodian.workerCaps must be a mapping".to_owned(),
            ));
        };
        for key in caps.keys() {
            if !CUSTODIAN_WORKER_CAP_KEYS.contains(&key.as_str()) {
                return Err(SettingsError(format!(
                    "custodian.workerCaps.{key} is not a setting openbutler knows (known: {})",
                    known(&CUSTODIAN_WORKER_CAP_KEYS)
                )));
            }
        }
        let cap = |field: &str, min: u64, max: u64| -> Result<u64, SettingsError> {
            let default = match field {
                "perSession" => 8,
                "perWorkerParent" => 2,
                "perMachine" => 10,
                _ => 2,
            };
            match caps.get(field) {
                None | Some(Value::Null) => Ok(default),
                Some(value) => {
                    whole_number_in_range(value, &format!("custodian.workerCaps.{field}"), min, max)
                }
            }
        };
        settings.worker_caps = Some(WorkerCaps {
            per_session: cap("perSession", 1, 32)?,
            per_worker_parent: cap("perWorkerParent", 0, 8)?,
            per_machine: cap("perMachine", 1, 64)?,
            depth: cap("depth", 1, 3)?,
        });
    }
    Ok(settings)
}

// --- Secrets -----------------------------------------------------------------

/// Parses the `config.yaml` document that is already known to be a mapping.
pub fn parse_secrets(doc: &Map<String, Value>) -> Result<SecretsStore, SettingsError> {
    for key in doc.keys() {
        if !KNOWN_SECRET_KEYS.contains(&key.as_str()) {
            return Err(SettingsError(format!(
                "{key} is not a secret openbutler knows (known: {})",
                known(&KNOWN_SECRET_KEYS)
            )));
        }
    }
    let mut secrets = SecretsStore::default();
    match doc.get("providers") {
        None => {}
        Some(value) => {
            // Unlike the public half (which skips a non-record providers
            // block), the secret half reads it strictly: `Object.entries`
            // over a non-record either throws or walks the wrong shape, and
            // an unreadable store must never count as an empty one.
            let Some(raw_providers) = as_record(value) else {
                return Err(SettingsError(
                    "config providers must be a mapping".to_owned(),
                ));
            };
            for (id, raw) in raw_providers {
                secrets
                    .providers
                    .insert(id.clone(), parse_provider_secrets(raw, id)?);
            }
        }
    }
    if let Some(value) = doc.get("teamTicket") {
        let Some(text) = value.as_str() else {
            return Err(SettingsError(
                "config teamTicket must be a non-empty string under 512 characters.".to_owned(),
            ));
        };
        if text.is_empty() || js_len(text) > 512 {
            return Err(SettingsError(
                "config teamTicket must be a non-empty string under 512 characters.".to_owned(),
            ));
        }
        secrets.team_ticket = Some(text.to_owned());
    }
    if let Some(value) = doc.get("modal") {
        secrets.modal = Some(parse_modal_secrets(value)?);
    }
    Ok(secrets)
}

fn parse_provider_secrets(raw: &Value, id: &str) -> Result<ProviderSecrets, SettingsError> {
    let Some(record) = as_record(raw) else {
        return Err(SettingsError(format!(
            "config providers.{id} must be a mapping"
        )));
    };
    for key in record.keys() {
        if !KNOWN_PROVIDER_SECRET_KEYS.contains(&key.as_str()) {
            return Err(SettingsError(format!(
                "config providers.{id}.{key} is not a secret openbutler knows (known: {})",
                known(&KNOWN_PROVIDER_SECRET_KEYS)
            )));
        }
    }
    Ok(ProviderSecrets {
        keys: match record.get("keys") {
            None => Vec::new(),
            Some(value) => string_array(value, &format!("config providers.{id}.keys"))?,
        },
    })
}

fn parse_modal_secrets(raw: &Value) -> Result<ModalCredentials, SettingsError> {
    let Some(record) = as_record(raw) else {
        return Err(SettingsError("config modal must be a mapping".to_owned()));
    };
    for key in record.keys() {
        if key != "tokenId" && key != "tokenSecret" {
            return Err(SettingsError(format!(
                "config modal.{key} is not a secret openbutler knows"
            )));
        }
    }
    let Some(token_id) = record.get("tokenId").and_then(Value::as_str) else {
        return Err(SettingsError(
            "config modal.tokenId must be a non-empty string.".to_owned(),
        ));
    };
    if token_id.is_empty() || js_len(token_id) > 256 {
        return Err(SettingsError(
            "config modal.tokenId must be a non-empty string.".to_owned(),
        ));
    }
    let Some(token_secret) = record.get("tokenSecret").and_then(Value::as_str) else {
        return Err(SettingsError(
            "config modal.tokenSecret must be a non-empty string.".to_owned(),
        ));
    };
    if token_secret.is_empty() || js_len(token_secret) > 512 {
        return Err(SettingsError(
            "config modal.tokenSecret must be a non-empty string.".to_owned(),
        ));
    }
    Ok(ModalCredentials {
        token_id: token_id.to_owned(),
        token_secret: token_secret.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_names_follow_the_regex_shape() {
        assert!(is_environment_name("OPENBUTLER_TOKEN"));
        assert!(is_environment_name("_x9"));
        assert!(!is_environment_name("9x"));
        assert!(!is_environment_name("A-B"));
        assert!(!is_environment_name(""));
    }

    #[test]
    fn id_tokens_follow_the_schedule_shape() {
        assert!(is_id_token("morning-brief_1", 64));
        assert!(!is_id_token("_leading", 64));
        assert!(!is_id_token("has space", 64));
        assert!(!is_id_token(&"x".repeat(65), 64));
    }

    #[test]
    fn absolute_paths_follow_the_host_rule() {
        #[cfg(windows)]
        {
            assert!(is_absolute_path(r"C:\work"));
            assert!(is_absolute_path(r"\\server\share"));
            assert!(!is_absolute_path("work/dir"));
        }
        #[cfg(not(windows))]
        {
            assert!(is_absolute_path("/home/user"));
            assert!(!is_absolute_path("home/user"));
        }
    }
}
