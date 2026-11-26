//! Agent Skill discovery pins: the source's ordering, the duplicate-name
//! override, the frontmatter rules and the action's workspace default.
//!
//! Source: `skills.ts` `discoverSkills`/`readSkillMetadata`/`isSkillName`
//! and `cli-main.ts` `runSkills` (L580-597), read 2026-11-26.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;

use common::temp_data_dir;
use openbutler_daemon::dispatch::{DispatchState, Host, handle};
use openbutler_daemon::skills::{
    SkillRoot, SkillScope, discover_from_roots, discover_skills, skill_roots,
};
use openbutler_protocol::{DaemonRequest, DaemonResponse};

fn root(dir: &Path, scope: SkillScope) -> SkillRoot {
    SkillRoot {
        path: dir.to_path_buf(),
        scope,
    }
}

fn write_skill(roots: &Path, directory: &str, frontmatter: &str) {
    let skill = roots.join(directory);
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), frontmatter).unwrap();
}

fn skill_md(name: &str, description: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n\nBody.\n")
}

/// The description budget is measured the way the source measures it: in
/// UTF-16 units, not bytes. A Turkish note of 600 characters is 1200 bytes,
/// so a byte budget would refuse a description the source accepts.
#[test]
fn a_multibyte_description_under_the_budget_is_accepted() {
    let dir = temp_data_dir("skills-description-units");
    let description = "\u{11F}".repeat(600);
    write_skill(&dir, "turkce", &skill_md("turkce", &description));
    let found = discover_from_roots(&[root(&dir, SkillScope::Global)]);
    assert_eq!(found.len(), 1, "the description fits the 1024-unit budget");
    assert_eq!(found[0].description, description);
}

/// The prompt assembly and the `skills.list` action discover over two
/// implementations of one rule (the prompt half lives in
/// `agent::providers`, the action's in `skills`). They must agree: a skill the
/// listing shows but the prompt drops is invisible to the model, and the
/// description budget is the field where they drifted once already.
#[test]
fn the_prompt_and_the_action_agree_on_what_a_skill_is() {
    let dir = temp_data_dir("skills-agreement");
    let description = "\u{11F}".repeat(600);
    write_skill(&dir, "turkce", &skill_md("turkce", &description));
    let roots = [root(&dir, SkillScope::Global)];
    let prompt_roots = [openbutler_daemon::agent::providers::SkillRoot {
        path: dir.clone(),
        scope: openbutler_daemon::agent::providers::SkillScope::Global,
    }];

    let from_action = discover_from_roots(&roots);
    let from_prompt = openbutler_daemon::agent::providers::discover_skills(&prompt_roots)
        .expect("the prompt half reads the same roots");
    assert_eq!(from_action.len(), 1, "the action lists the skill");
    assert_eq!(
        from_prompt.len(),
        1,
        "the prompt sees the skill the action lists"
    );
    assert_eq!(from_action[0].name, from_prompt[0].name);
    assert_eq!(from_action[0].description, from_prompt[0].description);
}

#[test]
fn discovers_metadata_and_orders_by_scope_rank_then_name() {
    let builtin = temp_data_dir("skills-builtin");
    let workspace = temp_data_dir("skills-workspace");
    let global = temp_data_dir("skills-global");
    write_skill(&builtin, "beta", &skill_md("beta", "A bundled skill."));
    write_skill(
        &workspace,
        "alpha",
        &skill_md("alpha", "A workspace skill."),
    );
    write_skill(&global, "zeta", &skill_md("zeta", "A user skill."));

    let found = discover_from_roots(&[
        root(&global, SkillScope::Global),
        root(&workspace, SkillScope::Workspace),
        root(&builtin, SkillScope::Builtin),
    ]);
    let names: Vec<&str> = found.iter().map(|skill| skill.name.as_str()).collect();
    // Scope rank: builtin, workspace, global — never the root order.
    assert_eq!(names, vec!["beta", "alpha", "zeta"]);
    assert_eq!(found[0].scope, SkillScope::Builtin);
    assert_eq!(found[1].scope, SkillScope::Workspace);
    assert_eq!(found[2].scope, SkillScope::Global);
    assert_eq!(found[0].description, "A bundled skill.");
    assert!(
        found[0].file_path.ends_with("SKILL.md"),
        "{}",
        found[0].file_path
    );
}

#[test]
fn a_later_root_overrides_a_duplicate_name() {
    let workspace = temp_data_dir("skills-dup-ws");
    let global = temp_data_dir("skills-dup-global");
    write_skill(&workspace, "shared", &skill_md("shared", "Workspace copy."));
    write_skill(&global, "shared", &skill_md("shared", "User copy."));

    let found = discover_from_roots(&[
        root(&workspace, SkillScope::Workspace),
        root(&global, SkillScope::Global),
    ]);
    assert_eq!(found.len(), 1);
    // Later roots win, as `skills.set` always overwrote.
    assert_eq!(found[0].scope, SkillScope::Global);
    assert_eq!(found[0].description, "User copy.");
}

#[test]
fn a_mismatched_frontmatter_name_is_not_a_skill() {
    let dir = temp_data_dir("skills-mismatch");
    write_skill(&dir, "alpha", &skill_md("beta", "Names another directory."));
    assert!(discover_from_roots(&[root(&dir, SkillScope::Global)]).is_empty());
}

#[test]
fn descriptions_must_be_non_empty_and_at_most_1024_chars() {
    let dir = temp_data_dir("skills-description");
    write_skill(&dir, "empty", &skill_md("empty", "\"\""));
    write_skill(
        &dir,
        "long",
        &skill_md("long", &format!("\"{}\"", "a".repeat(1_025))),
    );
    let at_limit = "a".repeat(1_024);
    write_skill(
        &dir,
        "limit",
        &skill_md("limit", &format!("\"{at_limit}\"")),
    );
    let found = discover_from_roots(&[root(&dir, SkillScope::Global)]);
    let names: Vec<&str> = found.iter().map(|skill| skill.name.as_str()).collect();
    assert_eq!(names, vec!["limit"]);
}

#[test]
fn a_skill_needs_a_closed_frontmatter_block() {
    let dir = temp_data_dir("skills-fence");
    write_skill(
        &dir,
        "open",
        "---\nname: open\ndescription: No closing fence.\n",
    );
    write_skill(&dir, "plain", "Just prose, no frontmatter.\n");
    let found = discover_from_roots(&[root(&dir, SkillScope::Global)]);
    assert!(found.is_empty());
}

#[test]
fn invalid_directory_names_are_skipped() {
    let dir = temp_data_dir("skills-names");
    for name in ["Bad Name", "a--b", "Upper", "-lead", "trail-"] {
        write_skill(&dir, name, &skill_md("x", "ignored"));
    }
    write_skill(&dir, "good-name", &skill_md("good-name", "Kept."));
    let found = discover_from_roots(&[root(&dir, SkillScope::Global)]);
    let names: Vec<&str> = found.iter().map(|skill| skill.name.as_str()).collect();
    assert_eq!(names, vec!["good-name"]);
}

#[test]
fn the_action_reports_the_roots_and_the_custodian_default() {
    let dir = temp_data_dir("skills-action");
    let host = Host::new();
    host.reload_settings(&dir).expect("settings load");
    let request = DaemonRequest::new("r1", "skills.list");
    let DaemonResponse::Success { result, .. } =
        handle(&host, &dir, &mut DispatchState::new(), &request)
    else {
        panic!("expected a success envelope");
    };
    // No stored default: bare launches use the custodian directory.
    let workspace = result["workspace"].as_str().unwrap();
    assert!(workspace.ends_with("custodian"), "{workspace}");
    // Seven roots, in the source's order.
    let roots = result["roots"].as_array().unwrap();
    assert_eq!(roots.len(), 7);
    assert_eq!(roots[0]["scope"], "builtin");
    assert_eq!(roots[4]["scope"], "global");
    assert!(roots[4]["path"].as_str().unwrap().ends_with("skills"));
    assert_eq!(roots[6]["scope"], "workspace");
    assert!(result["skills"].is_array());
}

#[test]
fn an_explicit_workspace_is_canonicalized_and_scanned() {
    let dir = temp_data_dir("skills-explicit");
    let workspace = temp_data_dir("skills-explicit-ws");
    write_skill(
        &workspace.join(".agents").join("skills"),
        "demo",
        &skill_md("demo", "Found in the workspace."),
    );
    let host = Host::new();
    host.reload_settings(&dir).expect("settings load");
    let mut request = DaemonRequest::new("r1", "skills.list");
    request
        .payload
        .insert("workspace".to_owned(), serde_json::json!(workspace));
    let DaemonResponse::Success { result, .. } =
        handle(&host, &dir, &mut DispatchState::new(), &request)
    else {
        panic!("expected a success envelope");
    };
    let skills = result["skills"].as_array().unwrap();
    assert!(
        skills
            .iter()
            .any(|skill| skill["name"] == "demo" && skill["scope"] == "workspace"),
        "{skills:?}"
    );
    // The answer names the canonical form, not the text it was given.
    let answered = result["workspace"].as_str().unwrap();
    assert!(Path::new(answered).is_absolute(), "{answered}");
}

#[test]
fn discovery_reads_the_workspace_roots_it_composes() {
    let data_dir = temp_data_dir("skills-compose");
    let workspace = temp_data_dir("skills-compose-ws");
    write_skill(
        &workspace.join(".claude").join("skills"),
        "picked",
        &skill_md("picked", "Composed root."),
    );
    let found = discover_skills(&workspace.display().to_string(), &data_dir);
    assert!(found.iter().any(|skill| skill.name == "picked"));
    // The roots themselves carry the data dir's skills folder.
    let roots = skill_roots(&workspace.display().to_string(), &data_dir);
    assert!(
        roots
            .iter()
            .any(|root| root.path == data_dir.join("skills") && root.scope == SkillScope::Global)
    );
}
