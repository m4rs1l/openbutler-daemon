//! The settings schema layer at the load surface: closed parsers, exact
//! refusal wording, secret merge, and the shared listen-target rule.
//!
//! Fail-before pins ported from the settings contract (settings.ts L226-L1150
//! and listen-target.ts, read 2026-11-16): a typo must fail at load with the
//! known set named, ranges refuse by name, and the secret file never leaks
//! into the public document.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

use openbutler_daemon::listen_target::parse_listen_target;
use openbutler_daemon::settings::{
    ApprovalMode, SettingsError, TeamRole, Theme, load_secrets, load_settings,
};

/// A scratch data directory; each test owns its own files.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ob-settings-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// An absolute path that satisfies the host rule on every platform the
/// suite runs on (the fixtures must load on Windows and Linux alike).
fn absolute_root() -> &'static str {
    if cfg!(windows) { r"C:\work" } else { "/work" }
}

fn put(root: &Path, name: &str, body: &str) {
    fs::write(root.join(name), body).unwrap();
}

fn load_err(root: &Path) -> String {
    load_settings(root).unwrap_err().to_string()
}

const KNOWN_TOP: &str = "providers, mcpServers, providerOrder, defaultProvider, defaultModel, reasoning, showReasoning, theme, approvalMode, autoReviewTimeoutSeconds, autoReviewProvider, autoReviewModel, roles, approvalTimeoutSeconds, commandShell, schedules, backgroundTasks, defaultWorkspaceRoot, daemon, telegram, team, custodian";

const KNOWN_PROVIDER: &str =
    "enabled, keys, models, url, npm, options, routing, modelRouting, payload, modelSpecs";

#[test]
fn missing_files_load_the_empty_machine() {
    let root = scratch("defaults");
    let settings = load_settings(&root).expect("absent files are defaults");
    assert!(settings.providers.is_empty());
    assert!(settings.provider_order.is_empty());
    assert_eq!(settings.default_provider, "");
    assert_eq!(settings.default_model, "");
    assert_eq!(settings.approval_mode, ApprovalMode::Workspace);
    assert!(settings.schedules.is_empty());
    assert!(settings.roles.is_none());

    let secrets = load_secrets(&root).expect("absent secrets are empty");
    assert!(secrets.providers.is_empty());
    assert!(secrets.team_ticket.is_none());
    assert!(secrets.modal.is_none());
}

#[test]
fn an_empty_document_is_defaults() {
    let root = scratch("empty");
    put(&root, "settings.yaml", "\n");
    let settings = load_settings(&root).expect("null document is defaults");
    assert!(settings.providers.is_empty());
    assert_eq!(settings.approval_mode, ApprovalMode::Workspace);
}

#[test]
fn a_non_mapping_document_names_the_file() {
    let root = scratch("seq");
    put(&root, "settings.yaml", "- one\n- two\n");
    let error = load_err(&root);
    let expected = format!("{} must be a mapping", root.join("settings.yaml").display());
    assert_eq!(error, expected);
}

#[test]
fn an_unknown_top_level_key_is_named_with_the_known_set() {
    let root = scratch("typo");
    put(&root, "settings.yaml", "defualtModel: x\n");
    let error = load_err(&root);
    assert_eq!(
        error,
        format!("defualtModel is not a setting openbutler knows (known: {KNOWN_TOP})")
    );
}

#[test]
fn a_full_document_parses_every_section() {
    let root = scratch("full");
    put(
        &root,
        "settings.yaml",
        &r#"providers:
  vercel:
    enabled: true
    keys:
      - old-split-key
    models:
      - gpt-x
      - gpt-x
    url: https://gateway.example
    npm: "@ai-sdk/openai"
    options:
      region: eu
    routing:
      pinned: parasail
  orphan:
    enabled: false
providerOrder:
  - vercel
  - ghost
defaultProvider: vercel
defaultModel: gpt-x
reasoning: high:2026-01-01
showReasoning: false
theme: System
approvalMode: auto-review
autoReviewTimeoutSeconds: 30
autoReviewProvider: vercel
autoReviewModel: gpt-x
roles:
  custodian:
    provider: vercel
    model: gpt-x
approvalTimeoutSeconds: 120
commandShell: pwsh
schedules:
  - id: morning
    name: Morning brief
    workspaceRoot: C:\work
    instruction: Summarize the inbox.
    trigger:
      adapter: daily
      options:
        at: "08:00"
defaultWorkspaceRoot: C:\work
daemon:
  listen: 127.0.0.1:45278
telegram:
  tokenEnv: OPENBUTLER_TELEGRAM_TOKEN
  workspaceRoot: C:\work
team:
  role: member
  teamName: Family
  memberName: node-a
  hubHost: 100.64.0.9
  hubPort: 45279
custodian:
  provider: vercel
  model: gpt-x
  contextLimitTokens: 200000
  reportLine: true
  reportTurnGapSeconds: 0
"#
        .replace(r"C:\work", absolute_root()),
    );
    let settings = load_settings(&root).expect("the full document parses");

    let provider = &settings.providers["vercel"];
    assert!(provider.enabled);
    assert_eq!(provider.keys, vec!["old-split-key"]);
    assert_eq!(provider.models.as_deref(), Some(&["gpt-x".to_owned()][..]));
    assert_eq!(provider.url.as_deref(), Some("https://gateway.example"));
    assert_eq!(provider.npm.as_deref(), Some("@ai-sdk/openai"));
    assert_eq!(provider.options.as_ref().unwrap()["region"], "eu");
    assert_eq!(provider.routing.as_ref().unwrap()["pinned"], "parasail");
    assert!(!settings.providers["orphan"].enabled);

    // Order keeps only real providers; a missing one is appended.
    assert_eq!(settings.provider_order, ["vercel", "orphan"]);
    assert_eq!(settings.default_provider, "vercel");
    assert_eq!(settings.default_model, "gpt-x");
    assert_eq!(settings.reasoning.as_deref(), Some("high:2026-01-01"));
    assert_eq!(settings.show_reasoning, Some(false));
    assert_eq!(settings.theme, Some(Theme::System));
    assert_eq!(settings.approval_mode, ApprovalMode::AutoReview);
    assert_eq!(settings.auto_review_timeout_seconds, Some(30));
    assert_eq!(settings.approval_timeout_seconds, Some(120));
    assert_eq!(settings.command_shell.as_deref(), Some("pwsh"));

    // Legacy single-purpose fields ingest into roles; custodian pair stays too.
    let roles = settings.roles.as_ref().unwrap();
    assert_eq!(roles["review"].provider, "vercel");
    assert_eq!(roles["review"].model, "gpt-x");
    assert_eq!(roles["custodian"].provider, "vercel");

    let schedule = &settings.schedules[0];
    assert_eq!(schedule.id, "morning");
    assert!(schedule.enabled, "enabled defaults to true");
    assert_eq!(schedule.origin_session_id, "");
    assert_eq!(schedule.workspace_root, absolute_root());
    assert_eq!(schedule.trigger.adapter, "daily");
    assert_eq!(schedule.trigger.options["at"], "08:00");

    assert_eq!(
        settings.default_workspace_root.as_deref(),
        Some(absolute_root())
    );
    assert_eq!(settings.daemon.as_ref().unwrap().listen, "127.0.0.1:45278");

    let telegram = settings.telegram.as_ref().unwrap();
    assert!(telegram.enabled);
    assert_eq!(telegram.token_env, "OPENBUTLER_TELEGRAM_TOKEN");

    let team = settings.team.as_ref().unwrap();
    assert_eq!(team.role, TeamRole::Member);
    assert_eq!(team.team_name, "Family");
    assert_eq!(team.member_name, "node-a");
    assert_eq!(team.hub_host, "100.64.0.9");
    assert_eq!(team.hub_port, 45279);

    let custodian = settings.custodian.as_ref().unwrap();
    assert_eq!(custodian.provider.as_deref(), Some("vercel"));
    assert_eq!(custodian.context_limit_tokens, Some(200_000));
    assert_eq!(custodian.report_line, Some(true));
    assert_eq!(custodian.report_turn_gap_seconds, Some(0));
    // workerCaps absent → no block; present → per-field defaults.
    assert!(custodian.worker_caps.is_none());
}

#[test]
fn worker_caps_fill_per_field_with_defaults_when_the_block_is_present() {
    let root = scratch("caps");
    put(
        &root,
        "settings.yaml",
        "custodian:\n  workerCaps:\n    depth: 3\n",
    );
    let settings = load_settings(&root).expect("caps parse");
    let caps = settings
        .custodian
        .as_ref()
        .unwrap()
        .worker_caps
        .as_ref()
        .unwrap();
    assert_eq!(caps.depth, 3);
    assert_eq!(caps.per_session, 8);
    assert_eq!(caps.per_worker_parent, 2);
    assert_eq!(caps.per_machine, 10);
}

#[test]
fn approval_mode_ask_migrates_to_workspace() {
    let root = scratch("ask");
    put(&root, "settings.yaml", "approvalMode: ask\n");
    let settings = load_settings(&root).expect("ask loads");
    assert_eq!(settings.approval_mode, ApprovalMode::Workspace);
}

#[test]
fn validation_refuses_by_name_and_range() {
    let root = scratch("ranges");

    put(&root, "settings.yaml", "reasoning: bad id!\n");
    assert_eq!(
        load_err(&root),
        "reasoning must be a valid model variant id"
    );

    put(&root, "settings.yaml", "theme: Solarized\n");
    assert_eq!(load_err(&root), "theme must be Dark, Light, or System");

    put(&root, "settings.yaml", "autoReviewTimeoutSeconds: 4\n");
    assert_eq!(
        load_err(&root),
        "autoReviewTimeoutSeconds must be a whole number of seconds between 5 and 600."
    );

    put(&root, "settings.yaml", "approvalTimeoutSeconds: 601\n");
    assert_eq!(
        load_err(&root),
        "approvalTimeoutSeconds must be a whole number of seconds between 5 and 600."
    );

    put(&root, "settings.yaml", "autoReviewProvider: vercel\n");
    assert_eq!(
        load_err(&root),
        "autoReviewProvider and autoReviewModel must both be non-empty strings, or both omitted."
    );

    put(
        &root,
        "settings.yaml",
        "custodian:\n  contextLimitTokens: 4000\n",
    );
    assert_eq!(
        load_err(&root),
        "custodian.contextLimitTokens must be a whole number between 10000 and 2000000."
    );

    put(
        &root,
        "settings.yaml",
        "custodian:\n  workerCaps:\n    perWorkerParent: 9\n",
    );
    assert_eq!(
        load_err(&root),
        "custodian.workerCaps.perWorkerParent must be a whole number between 0 and 8."
    );

    put(
        &root,
        "settings.yaml",
        "defaultWorkspaceRoot: relative/path\n",
    );
    assert_eq!(
        load_err(&root),
        "defaultWorkspaceRoot must be an absolute path."
    );

    put(
        &root,
        "settings.yaml",
        "telegram:\n  tokenEnv: 7123-literal-token\n",
    );
    assert_eq!(
        load_err(&root),
        "telegram.tokenEnv must name an environment variable, not contain a token."
    );

    put(
        &root,
        "settings.yaml",
        &"telegram:\n  tokenEnv: T_OK\n  workspaceRoot: PLACEHOLDER_ABS\n  chatId: \"123\"\n"
            .replace("PLACEHOLDER_ABS", absolute_root()),
    );
    assert_eq!(
        load_err(&root),
        "telegram chatId and userId must be paired together."
    );

    put(&root, "settings.yaml", "team:\n  role: sidekick\n");
    assert_eq!(load_err(&root), "team.role must be hub or member.");

    put(
        &root,
        "settings.yaml",
        "team:\n  role: hub\n  teamName: t\n  memberName: m\n  hubHost: h.example\n  hubPort: 70000\n",
    );
    assert_eq!(load_err(&root), "team.hubPort must be a port 1-65535.");
}

#[test]
fn schedules_validate_ids_names_and_triggers() {
    let root = scratch("schedules");
    let abs = absolute_root();

    put(
        &root,
        "settings.yaml",
        "schedules:\n  - id: \"bad id!\"\n    name: n\n    instruction: i\n    trigger: {adapter: daily, options: {}}\n",
    );
    assert_eq!(
        load_err(&root),
        "Schedule id must use 1-64 letters, digits, underscores, or dashes."
    );

    put(
        &root,
        "settings.yaml",
        "schedules:\n  - id: ok\n    name: \"\"\n    instruction: i\n    trigger: {adapter: daily, options: {}}\n",
    );
    assert_eq!(load_err(&root), "Schedule name must use 1-128 characters.");

    put(
        &root,
        "settings.yaml",
        &format!(
            "schedules:\n  - id: ok\n    name: n\n    workspaceRoot: {abs}\n    instruction: \"\"\n    trigger: {{adapter: daily, options: {{}}}}\n"
        ),
    );
    assert_eq!(
        load_err(&root),
        "Schedule instruction must use 1-16000 characters."
    );

    put(
        &root,
        "settings.yaml",
        &format!(
            "schedules:\n  - id: ok\n    name: n\n    workspaceRoot: {abs}\n    instruction: i\n    trigger: {{options: {{}}, bogus: 1}}\n"
        ),
    );
    assert_eq!(
        load_err(&root),
        "Schedule trigger must contain adapter and options."
    );

    put(
        &root,
        "settings.yaml",
        &format!(
            "schedules:\n  - id: ok\n    name: n\n    workspaceRoot: {abs}\n    instruction: i\n    trigger: {{options: {{}}}}\n"
        ),
    );
    assert_eq!(load_err(&root), "Schedule trigger adapter is invalid.");

    // The pre-rename key reads through; duplicates are refused.
    put(
        &root,
        "settings.yaml",
        &format!(
            "backgroundTasks:\n  - id: a\n    name: n\n    workspaceRoot: {abs}\n    instruction: i\n    trigger: {{adapter: daily, options: {{}}}}\n  - id: a\n    name: n\n    workspaceRoot: {abs}\n    instruction: i\n    trigger: {{adapter: daily, options: {{}}}}\n"
        ),
    );
    assert_eq!(
        load_err(&root),
        "schedules contains duplicate schedule ids."
    );
}

#[test]
fn providers_are_a_closed_schema() {
    let root = scratch("providers");

    put(
        &root,
        "settings.yaml",
        "providers:\n  vercel:\n    enabld: true\n",
    );
    assert_eq!(
        load_err(&root),
        format!(
            "providers.vercel.enabld is not a setting openbutler knows (known: {KNOWN_PROVIDER})"
        )
    );

    put(
        &root,
        "settings.yaml",
        "providers:\n  vercel:\n    models:\n      - \"\"\n",
    );
    assert_eq!(
        load_err(&root),
        "providers.vercel.models must contain at most 512 non-empty model IDs under 512 characters each."
    );

    put(
        &root,
        "settings.yaml",
        "providers:\n  vercel:\n    modelSpecs:\n      gpt-x:\n        contxt: 1000\n",
    );
    assert_eq!(
        load_err(&root),
        "providers.vercel.modelSpecs.gpt-x.contxt is not a model spec openbutler knows (known: name, context, output, reasoning, payload)"
    );
}

#[test]
fn option_bags_are_bounded_and_refuse_prototype_keys() {
    let root = scratch("options");

    put(
        &root,
        "settings.yaml",
        "providers:\n  vercel:\n    options:\n      __proto__:\n        polluted: true\n",
    );
    assert_eq!(
        load_err(&root),
        "providers.vercel.options.__proto__ is not an allowed option name."
    );

    let big = "x".repeat(16_001);
    put(
        &root,
        "settings.yaml",
        &format!("providers:\n  vercel:\n    options:\n      blob: {big}\n"),
    );
    let error = load_err(&root);
    assert!(
        error.starts_with("providers.vercel.options must serialize under 16000 characters (got "),
        "unexpected: {error}"
    );
}

#[test]
fn mcp_servers_are_closed_per_transport() {
    let root = scratch("mcp");

    put(
        &root,
        "settings.yaml",
        "mcpServers:\n  a:\n    transport: stdio\n    command: npx\n    colour: red\n",
    );
    assert_eq!(
        load_err(&root),
        "mcpServers.a.colour is not a stdio setting"
    );

    put(
        &root,
        "settings.yaml",
        "mcpServers:\n  a:\n    transport: stdio\n    command: \"\"\n",
    );
    assert_eq!(
        load_err(&root),
        "mcpServers.a.command must be a non-empty executable name"
    );

    put(
        &root,
        "settings.yaml",
        "mcpServers:\n  a:\n    transport: stdio\n    command: npx\n    env:\n      KEY: not-a-valid-env-name\n",
    );
    assert_eq!(
        load_err(&root),
        "mcpServers.a.env.KEY must name a host environment variable"
    );

    put(
        &root,
        "settings.yaml",
        "mcpServers:\n  a:\n    transport: http\n    url: ftp://example.com\n",
    );
    assert_eq!(
        load_err(&root),
        "mcpServers.a.url must use HTTP(S) and must not contain credentials"
    );

    put(
        &root,
        "settings.yaml",
        "mcpServers:\n  a:\n    transport: tcp\n",
    );
    assert_eq!(
        load_err(&root),
        "mcpServers.a.transport must be stdio, http, or sse"
    );
}

#[test]
fn command_shell_accepts_presets_and_paths() {
    let root = scratch("shell");
    put(&root, "settings.yaml", "commandShell: pwsh\n");
    let settings = load_settings(&root).expect("preset loads");
    assert_eq!(settings.command_shell.as_deref(), Some("pwsh"));

    put(&root, "settings.yaml", "commandShell: C:/tools/sh.exe\n");
    let settings = load_settings(&root).expect("path loads");
    assert_eq!(settings.command_shell.as_deref(), Some("C:/tools/sh.exe"));

    put(&root, "settings.yaml", "commandShell: fish\n");
    assert_eq!(
        load_err(&root),
        "commandShell must be one of auto, pwsh, powershell, cmd, git-bash, wsl, bash, zsh, sh or an executable path"
    );
}

#[test]
fn secrets_parse_only_their_own_known_keys() {
    let root = scratch("secrets");
    put(&root, "config.yaml", "sessionToken: nope\n");
    let error = load_secrets(&root).unwrap_err().to_string();
    assert_eq!(
        error,
        "sessionToken is not a secret openbutler knows (known: providers, teamTicket, modal)"
    );

    put(&root, "config.yaml", "- not\n- a mapping\n");
    let error = load_secrets(&root).unwrap_err().to_string();
    assert_eq!(
        error,
        format!("{} must be a mapping", root.join("config.yaml").display())
    );

    put(
        &root,
        "config.yaml",
        "providers:\n  vercel:\n    keys:\n      - secret-1\nteamTicket: t-123\n",
    );
    let secrets = load_secrets(&root).expect("secrets load");
    assert_eq!(secrets.providers["vercel"].keys, vec!["secret-1"]);
    assert_eq!(secrets.team_ticket.as_deref(), Some("t-123"));
}

#[test]
fn secrets_merge_unites_keys_and_creates_config_only_providers() {
    let root = scratch("merge");
    put(
        &root,
        "settings.yaml",
        "providers:\n  vercel:\n    keys:\n      - public-side-key\n",
    );
    put(
        &root,
        "config.yaml",
        "providers:\n  vercel:\n    keys:\n      - config-side-key\n  onlysecrets:\n    keys:\n      - k\n",
    );
    let settings = load_settings(&root).expect("merge loads");
    let vercel = &settings.providers["vercel"];
    assert_eq!(vercel.keys, ["config-side-key", "public-side-key"]);
    // The config-only provider is created enabled and joins the order.
    let only = &settings.providers["onlysecrets"];
    assert!(only.enabled);
    assert_eq!(only.keys, ["k"]);
    assert_eq!(settings.provider_order, ["vercel", "onlysecrets"]);
}

#[test]
fn listen_target_accepts_tailnet_and_loopback_only() {
    let parsed = parse_listen_target("100.64.0.5:45280").expect("tailnet parses");
    assert_eq!((parsed.host.as_str(), parsed.port), ("100.64.0.5", 45280));

    let parsed = parse_listen_target(" 127.0.0.1:1 ").expect("loopback parses");
    assert_eq!(parsed.port, 1);

    let error = parse_listen_target("0.0.0.0:45280")
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "0.0.0.0 is not a tailnet address. The daemon binds the tailnet or loopback only; 0.0.0.0 and public addresses are refused."
    );

    let error = parse_listen_target("100.128.0.1:45280")
        .unwrap_err()
        .to_string();
    assert!(
        error.starts_with("100.128.0.1 is not a tailnet address."),
        "unexpected: {error}"
    );

    let error = parse_listen_target("45280").unwrap_err().to_string();
    assert_eq!(
        error,
        "--listen needs an IPv4 address and port, e.g. 100.64.0.5:45280 (got \"45280\")."
    );

    let error = parse_listen_target("100.64.0.5:70000")
        .unwrap_err()
        .to_string();
    assert_eq!(error, "Listen port 70000 is out of range 1-65535.");
}

#[test]
fn daemon_listen_validates_through_the_shared_rule() {
    let root = scratch("daemon-listen");
    put(
        &root,
        "settings.yaml",
        "daemon:\n  listen: 100.128.0.1:45280\n",
    );
    let error = load_err(&root);
    assert!(
        error.starts_with("100.128.0.1 is not a tailnet address."),
        "unexpected: {error}"
    );

    put(&root, "settings.yaml", "daemon:\n  extra: 1\n");
    assert_eq!(
        load_err(&root),
        "extra is not a daemon setting openbutler knows (known: listen)"
    );
}

#[test]
fn malformed_yaml_fails_loudly() {
    let root = scratch("malformed");
    put(&root, "settings.yaml", "providers: [unclosed\n");
    let error: Result<_, SettingsError> = load_settings(&root);
    assert!(
        error.is_err(),
        "a broken document must not load as defaults"
    );
}
