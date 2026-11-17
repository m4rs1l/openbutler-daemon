//! System-prompt context providers: bounded memory reads, the workspace key
//! layout, profile availability, the team block wording, skill discovery and
//! the index budget.
//!
//! Sources: legacy `memory-paths.ts`, `agent.ts`, `skills.ts`, `paths.ts`.
//!
//! Order note: this module landed implementation-first (the compile fixes
//! preceded the pins) — the behaviour is pinned here retroactively; every
//! assertion below was written against the read source, not the impl.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use openbutler_daemon::agent::prompt::PromptRole;
use openbutler_daemon::agent::providers::{
    AgentSkill, MEMORY_INDEX_MAX_BYTES, MEMORY_INDEX_MAX_LINES, MemoryFileRead, SkillRoot,
    SkillScope, TeamInfo, discover_skills, format_skill_index, memory_paths, openbutler_skill_dir,
    profile_availability, read_memory_directory, read_memory_file, read_memory_index,
    render_team_block, scratch_root, skill_roots, truncation_notice, workspace_skill_directory,
};
use openbutler_daemon::agent::providers::{MEMORY_USER_CONTEXT_MAX_BYTES, build_team_block};

fn temp_dir(tag: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("ob-providers-{tag}-{nonce}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A skill directory whose frontmatter `name` must equal the directory name.
fn skill_dir(root: &Path, dir_name: &str, meta_name: &str, description: &str) {
    let dir = root.join(dir_name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {meta_name}\ndescription: {description}\n---\nbody\n"),
    )
    .unwrap();
}

#[test]
fn memory_paths_key_is_first_24_hex_and_stable_across_spellings() {
    let data = temp_dir("paths-data");
    let workspace = temp_dir("paths-ws");
    let one = memory_paths(&data, &workspace);
    let two = memory_paths(&data, &workspace.join("."));
    assert_eq!(
        one.workspace_key, two.workspace_key,
        "canonicalise collapses spellings"
    );
    assert_eq!(one.workspace_key.len(), 24);
    assert!(
        one.workspace_key
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit()),
        "key is hex: {}",
        one.workspace_key
    );
    assert_eq!(one.root, data.join("MEMORY"));
    assert_eq!(one.index_file, one.root.join("MEMORY.md"));
    assert_eq!(
        one.preferences_file,
        one.root.join("user").join("preferences.md")
    );
    assert_eq!(one.user_file, one.root.join("user").join("profile.md"));
    assert_eq!(one.topics_directory, one.root.join("topics"));
    assert_eq!(one.areas_directory, one.root.join("areas"));
    assert_eq!(one.self_directory, one.root.join("self"));
    assert_eq!(one.peers_directory, one.root.join("self").join("peers"));
    assert_eq!(
        one.workspace_file,
        one.workspace_directory.join("profile.md")
    );
    assert!(one.workspace_directory.ends_with(&one.workspace_key));
}

#[test]
fn memory_budget_constants_match_the_source() {
    assert_eq!(MEMORY_INDEX_MAX_BYTES, 25 * 1024);
    assert_eq!(MEMORY_INDEX_MAX_LINES, 200);
    assert_eq!(MEMORY_USER_CONTEXT_MAX_BYTES, 16 * 1024);
}

#[test]
fn read_memory_index_cuts_on_a_line_boundary_caps_at_200_lines_and_tolerates_crlf() {
    let dir = temp_dir("index");
    let file = dir.join("MEMORY.md");

    // Over budget: the cut lands on the last newline inside the window.
    let line_a = "a".repeat(20_000);
    let line_b = "b".repeat(20_000);
    fs::write(&file, format!("{line_a}\n{line_b}\n")).unwrap();
    assert_eq!(read_memory_index(&file), line_a);

    // Line cap: 250 lines load as exactly the first 200.
    let many = (0..250)
        .map(|index| format!("line-{index:03}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&file, &many).unwrap();
    let loaded = read_memory_index(&file);
    assert_eq!(loaded.lines().count(), MEMORY_INDEX_MAX_LINES);
    assert!(loaded.starts_with("line-000\n"));
    assert!(loaded.ends_with("line-199"));

    // CRLF is normalised away, the trailing newline trims off.
    fs::write(&file, "a\r\nb\r\n").unwrap();
    assert_eq!(read_memory_index(&file), "a\nb");

    // Missing, unreadable or empty loads empty — the agent runs without it.
    assert_eq!(read_memory_index(&dir.join("absent.md")), "");
}

#[test]
fn read_memory_file_reports_the_cut_and_the_raw_loaded_position() {
    let dir = temp_dir("note");
    let file = dir.join("note.md");
    fs::write(&file, "line one\nline two\n").unwrap();

    // Ten bytes reach into line two; the cut steps back to the newline.
    let cut = read_memory_file(&file, 10);
    assert!(cut.truncated);
    assert_eq!(cut.total_bytes, 18);
    assert_eq!(
        cut.loaded_bytes, 9,
        "raw position, pre-trim, counts the newline"
    );
    assert_eq!(cut.text, "line one");

    let whole = read_memory_file(&file, 100);
    assert!(!whole.truncated);
    assert_eq!(whole.total_bytes, 18);
    assert_eq!(whole.loaded_bytes, 18);
    assert_eq!(whole.text, "line one\nline two");

    let missing = read_memory_file(&dir.join("absent.md"), 10);
    assert_eq!(missing.text, "");
    assert!(!missing.truncated);
    assert_eq!(missing.total_bytes, 0);
    assert_eq!(missing.loaded_bytes, 0);
}

#[test]
fn truncation_notice_is_the_source_sentence_verbatim() {
    let read = MemoryFileRead {
        text: String::new(),
        truncated: true,
        total_bytes: 1000,
        loaded_bytes: 500,
    };
    assert_eq!(
        truncation_notice(&read),
        "[truncated: this note is 1000 bytes and only the first 500 are loaded. \
         Read the file from disk before relying on it; the cut hides the rules \
         that were written last.]"
    );
}

#[test]
fn profile_availability_has_three_states_and_refuses_paths_outside_the_root() {
    let root = temp_dir("memroot");
    let profile = root.join("profile.md");

    // Missing file under the root: not initialized.
    assert_eq!(
        profile_availability(&profile, &root, None),
        "not initialized"
    );

    fs::write(&profile, "# profile").unwrap();
    let whole = read_memory_file(&profile, 1024);
    assert!(!whole.truncated);
    assert_eq!(
        profile_availability(&profile, &root, Some(&whole)),
        "available"
    );

    let cut = MemoryFileRead {
        text: whole.text.clone(),
        truncated: true,
        total_bytes: whole.total_bytes,
        loaded_bytes: whole.loaded_bytes,
    };
    assert_eq!(
        profile_availability(&profile, &root, Some(&cut)),
        "available, truncated - read the file for the rest"
    );

    // Outside the managed root: never available, even as a real file.
    let outside = temp_dir("outside").join("profile.md");
    fs::write(&outside, "# elsewhere").unwrap();
    assert_eq!(
        profile_availability(&outside, &root, None),
        "not initialized"
    );
}

#[cfg(unix)]
#[test]
fn profile_availability_refuses_a_symlinked_profile() {
    let root = temp_dir("memroot-link");
    let target = root.join("real.md");
    fs::write(&target, "# real").unwrap();
    let link = root.join("profile.md");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert_eq!(
        profile_availability(&link, &root, None),
        "not initialized",
        "a symlinked component would let one scope masquerade as another"
    );
}

#[test]
fn render_team_block_wording_is_role_exact() {
    let team = TeamInfo {
        team_name: "Alpha".to_owned(),
        member_name: "hub-one".to_owned(),
        hub: true,
    };

    // No team or a worker role: no block at all.
    assert_eq!(render_team_block(PromptRole::Custodian, None, &[]), "");
    assert_eq!(
        render_team_block(
            PromptRole::Worker,
            Some(&team),
            &["- peer: note".to_owned()]
        ),
        ""
    );

    // The agent gets the short boundary sentence and never the peer list.
    let agent = render_team_block(PromptRole::Agent, Some(&team), &["- peer: note".to_owned()]);
    assert_eq!(
        agent,
        "## Team\nThis machine is part of the team \"Alpha\". Peers on other machines \
         are reached by the machine's custodian, not by this session."
    );

    // The custodian gets the hub sentence only when hosting.
    let hub = render_team_block(PromptRole::Custodian, Some(&team), &[]);
    assert!(hub.contains("This machine hosts the team, so peers reach it only while it runs."));
    assert!(hub.contains("## Team\n\nThis machine is \"hub-one\" in the team \"Alpha\"."));

    let member = TeamInfo {
        team_name: "Alpha".to_owned(),
        member_name: "worker-two".to_owned(),
        hub: false,
    };
    let plain = render_team_block(PromptRole::Custodian, Some(&member), &[]);
    assert!(!plain.contains("hosts the team"));

    // Peers append as their own section for the custodian.
    let with_peers = render_team_block(
        PromptRole::Custodian,
        Some(&team),
        &["- eve: runs on box-2".to_owned()],
    );
    assert!(with_peers.contains(
        "## Peers\nLocal notes about peers (may be stale, not shared):\n- eve: runs on box-2"
    ));
}

#[test]
fn build_team_block_reads_peer_notes_for_the_custodian_only() {
    let team = TeamInfo {
        team_name: "Alpha".to_owned(),
        member_name: "hub-one".to_owned(),
        hub: false,
    };
    let peers_dir = temp_dir("peers");
    fs::write(
        peers_dir.join("peer-a.md"),
        "---\ntitle: frontmatter is skipped\n---\n# Heading\nFirst real line\nmore\n",
    )
    .unwrap();

    let custodian = build_team_block(PromptRole::Custodian, Some(&team), &peers_dir);
    assert!(custodian.contains("## Peers"));
    assert!(custodian.contains("- peer-a: First real line"));

    let agent = build_team_block(PromptRole::Agent, Some(&team), &peers_dir);
    assert!(!agent.contains("## Peers"));
    assert_eq!(
        build_team_block(PromptRole::Worker, Some(&team), &peers_dir),
        ""
    );
    assert_eq!(
        build_team_block(PromptRole::Custodian, None, &peers_dir),
        ""
    );
}

#[test]
fn read_memory_directory_picks_the_first_content_line_in_sorted_order() {
    let dir = temp_dir("memdir");
    fs::write(
        dir.join("b.md"),
        "---\nfrontmatter: x\n---\n# Heading\nReal summary\nmore\n",
    )
    .unwrap();
    fs::write(dir.join("a.md"), "---\ntitle: only frontmatter\n---\n").unwrap();
    fs::write(dir.join("ignored.txt"), "not markdown\n").unwrap();

    let lines = read_memory_directory(&dir);
    assert_eq!(
        lines,
        vec![
            "- a: (no summary)".to_owned(),
            "- b: Real summary".to_owned()
        ]
    );
    assert_eq!(
        read_memory_directory(&dir.join("absent")),
        Vec::<String>::new()
    );
}

#[test]
fn discover_skills_overrides_caps_and_skips_broken_roots() {
    let global = temp_dir("skills-global");
    let workspace = temp_dir("skills-workspace");
    skill_dir(&global, "dup", "dup", "from the global root");
    skill_dir(&global, "dir-a", "other-name", "metadata name differs");
    skill_dir(&global, "bad-desc", "bad-desc", "");
    skill_dir(&global, "ok-name", "Bad_Name", "metadata name is not kebab");
    skill_dir(&global, "plain", "plain", "a plain skill");
    skill_dir(&workspace, "dup", "dup", "from the workspace root");

    let roots = vec![
        SkillRoot {
            path: global.clone(),
            scope: SkillScope::Global,
        },
        SkillRoot {
            path: temp_dir("skills-absent"),
            scope: SkillScope::Global,
        },
        SkillRoot {
            path: workspace.clone(),
            scope: SkillScope::Workspace,
        },
    ];
    let mut skills = discover_skills(&roots).unwrap();
    skills.sort_by(|left, right| left.name.cmp(&right.name));
    let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["dup", "plain"],
        "mismatched/blank metadata is skipped; missing roots are skipped"
    );

    let dup = skills.iter().find(|skill| skill.name == "dup").unwrap();
    assert_eq!(dup.scope, SkillScope::Workspace, "later roots override");
    assert_eq!(dup.description, "from the workspace root");
    assert_eq!(dup.file_path, workspace.join("dup").join("SKILL.md"));

    // Sort contract: builtin (0) < workspace (1) < global (2), names inside.
    let sort_global = temp_dir("skills-sort-global");
    let sort_workspace = temp_dir("skills-sort-workspace");
    let builtin = temp_dir("skills-builtin");
    skill_dir(&builtin, "alpha", "alpha", "builtin first");
    skill_dir(&sort_global, "zeta", "zeta", "global last");
    skill_dir(&sort_workspace, "mid", "mid", "workspace in between");
    let ordered = discover_skills(&[
        SkillRoot {
            path: sort_global,
            scope: SkillScope::Global,
        },
        SkillRoot {
            path: sort_workspace,
            scope: SkillScope::Workspace,
        },
        SkillRoot {
            path: builtin,
            scope: SkillScope::Builtin,
        },
    ])
    .unwrap();
    let ordered_names: Vec<&str> = ordered.iter().map(|skill| skill.name.as_str()).collect();
    assert_eq!(ordered_names, vec!["alpha", "mid", "zeta"]);
}

#[test]
fn discover_skills_caps_each_scope_at_128_new_names() {
    let root = temp_dir("skills-cap");
    for index in 0..129 {
        let name = format!("skill-{index:03}");
        skill_dir(&root, &name, &name, "capped scope");
    }
    let skills = discover_skills(&[SkillRoot {
        path: root,
        scope: SkillScope::Global,
    }])
    .unwrap();
    assert_eq!(
        skills.len(),
        128,
        "the 129th new name in a scope is dropped"
    );
    assert!(skills.iter().any(|skill| skill.name == "skill-127"));
    assert!(!skills.iter().any(|skill| skill.name == "skill-128"));
}

#[test]
fn format_skill_index_pins_the_line_shape_the_utf16_budget_and_its_boundary() {
    let path = PathBuf::from("skills").join("bug-reaper");
    let skill = AgentSkill {
        name: "bug-reaper".to_owned(),
        description: "finds things".to_owned(),
        file_path: path.clone(),
        scope: SkillScope::Global,
    };
    assert_eq!(
        format_skill_index(&[skill]),
        format!(
            "- bug-reaper [global]: finds things (read {} to load instructions)",
            path.display()
        )
    );

    // Exact budget boundary: the first line must fit in 8192 UTF-16 units.
    let base = format!(
        "- name-a [global]: {} (read {} to load instructions)",
        "",
        PathBuf::from("p").display()
    );
    let sized = |units: usize| AgentSkill {
        name: "name-a".to_owned(),
        description: "x".repeat(units - base.len()),
        file_path: PathBuf::from("p"),
        scope: SkillScope::Global,
    };
    assert_eq!(format_skill_index(&[sized(8_192)]).lines().count(), 1);
    assert_eq!(format_skill_index(&[sized(8_193)]).lines().count(), 0);

    // The budget counts UTF-16 units, not code points: 20 astral emoji
    // (40 units) push the line over while its char count stays under.
    let emoji_units = 8_193;
    let filler = emoji_units - base.len() - 40;
    let astral = AgentSkill {
        name: "name-a".to_owned(),
        description: format!("{}{}", "\u{1F600}".repeat(20), "x".repeat(filler)),
        file_path: PathBuf::from("p"),
        scope: SkillScope::Global,
    };
    let chars: usize = format!(
        "- name-a [global]: {} (read {} to load instructions)",
        astral.description,
        PathBuf::from("p").display()
    )
    .chars()
    .count();
    assert!(chars < 8_192, "char count would fit: {chars}");
    assert_eq!(
        format_skill_index(&[astral]),
        "",
        "UTF-16 units exceed the budget and the line is dropped"
    );
}

#[test]
fn workspace_skill_directory_is_the_first_matching_workspace_root() {
    let workspace = SkillRoot {
        path: PathBuf::from("root").join(".agents").join("skills"),
        scope: SkillScope::Workspace,
    };
    let other = SkillRoot {
        path: PathBuf::from("root").join(".claude").join("skills"),
        scope: SkillScope::Workspace,
    };
    let global = SkillRoot {
        path: PathBuf::from("global").join(".agents").join("skills"),
        scope: SkillScope::Global,
    };
    assert_eq!(
        workspace_skill_directory(&[other.clone(), global.clone(), workspace.clone()]),
        Some(workspace.path.clone()),
        "scope decides first, then the suffix"
    );
    assert_eq!(workspace_skill_directory(&[other]), None);
}

#[test]
fn skill_roots_order_and_simple_paths_follow_the_source() {
    let data = temp_dir("roots-data");
    let workspace = temp_dir("roots-ws");
    let home = temp_dir("roots-home");
    let builtin = PathBuf::from("bundled").join("skills");

    let roots = skill_roots(&data, &workspace, &home, None, Some(builtin.clone()));
    assert_eq!(roots.len(), 7);
    assert_eq!(roots[0].path, builtin);
    assert_eq!(roots[0].scope, SkillScope::Builtin);
    assert_eq!(roots[1].path, home.join(".claude").join("skills"));
    assert_eq!(roots[3].path, home.join(".codex").join("skills"));
    assert_eq!(roots[4].path, data.join("skills"));
    assert_eq!(roots[5].path, workspace.join(".claude").join("skills"));
    assert_eq!(roots[6].path, workspace.join(".agents").join("skills"));
    assert_eq!(roots[5].scope, SkillScope::Workspace);
    assert_eq!(roots[6].scope, SkillScope::Workspace);

    // Without a bundled root only the six user/workspace roots remain.
    assert_eq!(skill_roots(&data, &workspace, &home, None, None).len(), 6);

    assert_eq!(openbutler_skill_dir(&data), data.join("skills"));
}

#[test]
fn scratch_root_is_temp_openbutler_and_the_local_day() {
    let scratch = scratch_root();
    assert!(scratch.starts_with(std::env::temp_dir().join("openbutler")));
    let name = scratch.file_name().unwrap().to_string_lossy();
    assert_eq!(name.len(), 10, "YYYY-MM-DD, got {name}");
    let bytes = name.as_bytes();
    assert_eq!(bytes[4], b'-');
    assert_eq!(bytes[7], b'-');
    assert!(bytes[..4].iter().all(u8::is_ascii_digit));
    assert!(bytes[5..7].iter().all(u8::is_ascii_digit));
    assert!(bytes[8..].iter().all(u8::is_ascii_digit));
}
