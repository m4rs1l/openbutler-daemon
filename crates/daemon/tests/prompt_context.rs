//! The runtime context builder: fills `SystemPromptContext` from disk and
//! settings the way legacy `buildAgentSystem(workspaceRoot, base)` does for
//! one session, then assembles the frozen prompt around it.
//!
//! Sources: legacy `src/agent.ts` L48-L127 (composition), L144
//! (`memoryProfileAvailability`), L159 (`buildTeamBlock`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use openbutler_daemon::agent::context::{PromptEnvironment, build_system};
use openbutler_daemon::agent::prompt::PromptRole;
use openbutler_daemon::agent::providers::{TeamInfo, memory_paths};

fn temp_dir(tag: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("ob-prompt-ctx-{tag}-{nonce}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A populated tree: index, both user notes, the active workspace profile,
/// skills in two scopes, and a peer note for the custodian.
fn populated() -> (PathBuf, PathBuf, PathBuf) {
    let data = temp_dir("data");
    let workspace = temp_dir("workspace");
    let home = temp_dir("home");

    let memory = data.join("MEMORY");
    fs::create_dir_all(memory.join("user")).unwrap();
    fs::create_dir_all(memory.join("self").join("peers")).unwrap();
    fs::create_dir_all(memory.join("topics")).unwrap();
    fs::create_dir_all(memory.join("areas")).unwrap();
    fs::write(memory.join("MEMORY.md"), "## You\n- reply plain\n").unwrap();
    fs::write(memory.join("user").join("preferences.md"), "- no emojis\n").unwrap();
    fs::write(memory.join("user").join("profile.md"), "- solo developer\n").unwrap();

    let paths = memory_paths(&data, &workspace);
    fs::create_dir_all(&paths.workspace_directory).unwrap();
    fs::write(&paths.workspace_file, "# Workspace note\n").unwrap();
    fs::write(
        memory.join("self").join("peers").join("peer-a.md"),
        "---\ntitle: skipped\n---\nRuns on box-2\n",
    )
    .unwrap();

    let skill = workspace
        .join(".agents")
        .join("skills")
        .join("demo-skill")
        .join("SKILL.md");
    fs::create_dir_all(skill.parent().unwrap()).unwrap();
    fs::write(
        &skill,
        "---\nname: demo-skill\ndescription: Does demo things\n---\nbody\n",
    )
    .unwrap();
    let global_skill = home
        .join(".claude")
        .join("skills")
        .join("global-skill")
        .join("SKILL.md");
    fs::create_dir_all(global_skill.parent().unwrap()).unwrap();
    fs::write(
        &global_skill,
        "---\nname: global-skill\ndescription: Global knowledge\n---\nbody\n",
    )
    .unwrap();

    (data, workspace, home)
}

fn environment(data: PathBuf, workspace: PathBuf, home: PathBuf) -> PromptEnvironment {
    PromptEnvironment {
        data_root: data,
        workspace_root: workspace,
        home,
        codex_home: None,
        builtin_skill_dir: None,
        team: Some(TeamInfo {
            team_name: "Alpha".to_owned(),
            member_name: "hub-one".to_owned(),
            hub: true,
        }),
    }
}

#[test]
fn the_agent_system_carries_every_live_value() {
    let (data, workspace, home) = populated();
    let paths = memory_paths(&data, &workspace);
    let env = environment(data, workspace, home);
    let system = build_system(PromptRole::Agent, &env)
        .expect("assembly succeeds")
        .expect("marker counts match");

    // Paths, each with its availability read from disk.
    assert!(system.contains(&paths.root.display().to_string()));
    assert!(system.contains(&paths.index_file.display().to_string()));
    assert!(
        system.contains(&format!("{} (available)", paths.preferences_file.display())),
        "preferences availability comes from the read"
    );
    assert!(
        system.contains(&format!("{} (available)", paths.user_file.display())),
        "profile availability comes from the read"
    );
    assert!(
        system.contains(&format!("{} (available)", paths.workspace_file.display())),
        "workspace profile availability comes from the structural check"
    );

    // Loaded reference data, not fallbacks.
    assert!(
        system.contains("- reply plain"),
        "the memory index content loads"
    );
    assert!(
        !system.contains("(No memory index entries are saved.)"),
        "an existing index never falls back"
    );
    assert!(system.contains("- no emojis"), "preferences load");
    assert!(system.contains("- solo developer"), "profile loads");
    assert!(system.contains("## Loaded user context"));
    assert!(
        system.contains("- demo-skill [workspace]: Does demo things"),
        "the workspace skill index line"
    );
    assert!(
        system.contains("- global-skill [global]: Global knowledge"),
        "the global skill index line"
    );
    assert!(system.contains(&env_workspace_skill_dir(&env)));

    // The role's team block (agent wording) and the role's base text.
    assert!(
        system.contains(
            "This machine is part of the team \"Alpha\". Peers on other machines \
             are reached by the machine's custodian, not by this session."
        ),
        "the agent team block"
    );
    assert!(
        !system.contains("## Peers"),
        "the agent never reads peer notes"
    );
}

fn env_workspace_skill_dir(env: &PromptEnvironment) -> String {
    env.workspace_root
        .join(".agents")
        .join("skills")
        .display()
        .to_string()
}

#[test]
fn an_empty_tree_falls_back_and_omits_the_user_context_section() {
    let data = temp_dir("bare-data");
    let workspace = temp_dir("bare-workspace");
    let home = temp_dir("bare-home");
    let env = environment(data, workspace, home);
    let system = build_system(PromptRole::Agent, &env)
        .expect("assembly succeeds")
        .expect("marker counts match");

    assert!(system.contains("(No memory index entries are saved.)"));
    assert!(
        system.contains("(No skills found in the global or active-workspace skill directories.)")
    );
    assert!(
        system.contains(
            &env.workspace_root
                .join(".agents")
                .join("skills")
                .display()
                .to_string()
        ),
        "the workspace skill dir slot still renders its path"
    );
    assert!(
        !system.contains("## Loaded user context"),
        "both user notes empty means the section is omitted"
    );
    assert!(
        !system.contains("(unavailable)"),
        "the workspace skill directory exists, so no fallback"
    );
}

#[test]
fn a_budget_cut_appends_the_notice_and_reports_truncation() {
    let (data, workspace, home) = populated();
    // 5 000 lines × 5 bytes = 25 000 bytes — over the 16 KiB budget with
    // line boundaries to cut on.
    let big = "line\n".repeat(5_000);
    fs::write(
        data.join("MEMORY").join("user").join("preferences.md"),
        &big,
    )
    .unwrap();
    let paths = memory_paths(&data, &workspace);
    let env = environment(data, workspace, home);
    let system = build_system(PromptRole::Agent, &env)
        .expect("assembly succeeds")
        .expect("marker counts match");

    assert!(
        system.contains("[truncated: this note is"),
        "the cut note carries the notice"
    );
    assert!(
        system.contains(&format!(
            "{} (available, truncated - read the file for the rest)",
            paths.preferences_file.display()
        )),
        "the path line reports the truncation"
    );
    assert!(
        system.contains("- solo developer"),
        "the untouched profile still loads whole"
    );
}

#[test]
fn roles_select_their_base_and_the_custodian_reads_peer_notes() {
    let (data, workspace, home) = populated();
    let env = environment(data, workspace, home);

    let agent = build_system(PromptRole::Agent, &env)
        .expect("assembly succeeds")
        .expect("marker counts match");
    let custodian = build_system(PromptRole::Custodian, &env)
        .expect("assembly succeeds")
        .expect("marker counts match");
    let worker = build_system(PromptRole::Worker, &env)
        .expect("assembly succeeds")
        .expect("marker counts match");

    assert_ne!(agent, custodian, "each role has its own base text");
    assert!(
        custodian.contains("## Peers"),
        "only the custodian reads peer notes"
    );
    assert!(custodian.contains("- peer-a: Runs on box-2"));
    assert!(
        custodian.contains("This machine hosts the team"),
        "the hub sentence for the hosting custodian"
    );
    assert!(
        !worker.contains("## Team"),
        "a worker gets no team block at all"
    );
}
