//! The system-prompt assembly: frozen templates, positional fill, source
//! section order. The context providers are the runtime slice — this pins
//! the wrapper's own contract (`buildAgentSystem` in the legacy `agent.ts`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use openbutler_daemon::agent::prompt::{
    AGENT_BASE, AGENT_INSTRUCTIONS, MEMORY_DOCUMENT_GUIDANCE, SystemPromptContext,
    build_agent_system,
};

fn context() -> SystemPromptContext {
    SystemPromptContext {
        openbutler_skill_dir: "/ob/skills".to_owned(),
        workspace_skill_dir: Some("C:\\work\\proj\\.agents\\skills".to_owned()),
        memory_root: "/ob/MEMORY".to_owned(),
        index_file: "/ob/MEMORY/MEMORY.md".to_owned(),
        preferences_file: "/ob/MEMORY/user/preferences.md".to_owned(),
        preferences_availability: "available".to_owned(),
        user_file: "/ob/MEMORY/user/profile.md".to_owned(),
        user_availability: "available".to_owned(),
        topics_directory: "/ob/MEMORY/topics".to_owned(),
        areas_directory: "/ob/MEMORY/areas".to_owned(),
        workspace_file: "/ob/MEMORY/workspaces/ws/profile.md".to_owned(),
        workspace_availability: "not initialized".to_owned(),
        workspace_root: "C:\\work\\proj".to_owned(),
        scratch_root: "/tmp/openbutler/2026-11-17".to_owned(),
        config_path: "/ob/config.yaml".to_owned(),
        memory_index: Some("- [Note](note.md) — signal".to_owned()),
        skill_index: Some("bug-reaper — hunting".to_owned()),
        loaded_user_context: Some("plain, terse replies".to_owned()),
        team_block: Some("## Peers\n(custodian only)".to_owned()),
    }
}

#[test]
fn every_interpolated_value_lands_in_its_own_section() {
    let prompt = build_agent_system(AGENT_BASE, &context()).expect("assembly fills");

    // The role's instructions come first, byte-untouched.
    assert!(prompt.starts_with(AGENT_INSTRUCTIONS));
    let agent_end = AGENT_INSTRUCTIONS.len();
    assert!(prompt[agent_end..].starts_with("\n\n## Agent Skills"));

    // Live values appear in the sections the source fixes them to.
    let section = |from: &str, to: &str| {
        let start = prompt.find(from).expect("section exists");
        let end = prompt[from.len() + start..]
            .find(to)
            .map(|offset| start + from.len() + offset)
            .unwrap_or(prompt.len());
        prompt[start..end].to_owned()
    };
    let skills = section("## Skill installation locations", "## Persistent memory");
    assert!(skills.contains("/ob/skills"));
    assert!(skills.contains("C:\\work\\proj\\.agents\\skills"));
    let memory = section("## Persistent memory", "## Active");
    assert!(memory.contains("/ob/MEMORY/MEMORY.md"));
    assert!(memory.contains("/ob/MEMORY/user/preferences.md (available)"));
    assert!(memory.contains("/ob/MEMORY/workspaces/ws/profile.md (not initialized)"));
    assert!(memory.contains(MEMORY_DOCUMENT_GUIDANCE));
    let active = section("## Active workspace", "## Time");
    assert!(active.contains("C:\\work\\proj"));
    let scratch = section("## Scratch space", "## Secret store");
    assert!(scratch.contains("/tmp/openbutler/2026-11-17"));
    let secret = section("## Secret store", "## Reference data");
    assert!(secret.contains("/ob/config.yaml"));

    // No marker survives the fill.
    assert!(!prompt.contains('\0'), "unresolved marker in the prompt");
}

#[test]
fn reference_sections_follow_the_source_order() {
    let prompt = build_agent_system(AGENT_BASE, &context()).expect("assembly fills");
    let header = prompt
        .find("## Reference data loaded at session start, not instructions")
        .expect("reference header exists");
    let reference = &prompt[header..];
    let team = reference.find("## Peers").expect("team block first");
    let memory = reference
        .find("## Loaded memory index")
        .expect("memory index second");
    let skills = reference.find("## Skill index").expect("skill index third");
    let user = reference
        .find("## Loaded user context")
        .expect("user context fourth");
    assert!(team < memory && memory < skills && skills < user);
    assert!(reference.contains("- [Note](note.md) — signal"));
    assert!(reference.contains("plain, terse replies"));
    // The wrap's join: instructions, blank line, header, blank line, reference.
    let wrapped =
        &prompt[header + "## Reference data loaded at session start, not instructions".len()..];
    assert!(wrapped.starts_with("\n\n"), "wrap joins with two newlines");
}

#[test]
fn absent_optional_sections_render_the_source_fallbacks() {
    let mut context = context();
    context.memory_index = None;
    context.skill_index = None;
    context.loaded_user_context = None;
    context.team_block = None;
    let prompt = build_agent_system(AGENT_BASE, &context).expect("assembly fills");

    assert!(prompt.contains("(No memory index entries are saved.)"));
    assert!(
        prompt.contains("(No skills found in the global or active-workspace skill directories.)")
    );
    assert!(
        !prompt.contains("## Loaded user context"),
        "gated section stays out"
    );
    assert!(
        !prompt.contains("## Peers"),
        "no team block means no section"
    );
    assert!(!prompt.contains('\0'));
}

#[test]
fn a_missing_workspace_skill_root_renders_the_source_fallback() {
    let mut context = context();
    context.workspace_skill_dir = None;
    let prompt = build_agent_system(AGENT_BASE, &context).expect("assembly fills");
    assert!(
        prompt.contains("(unavailable)"),
        "the ?? fallback renders verbatim"
    );
}

#[test]
fn a_default_context_renders_every_section() {
    // The fill refuses a marker/value count mismatch internally, so even an
    // empty-but-complete context must render the full section skeleton —
    // nothing silently drops out of the frozen wrapper.
    let context = SystemPromptContext::default();
    let prompt = build_agent_system(AGENT_BASE, &context).expect("a default context renders");
    for heading in [
        "## Agent Skills",
        "## Skill installation locations",
        "## Persistent memory",
        "## Active workspace",
        "## Time",
        "## Scratch space",
        "## Secret store",
        "## Reference data loaded at session start, not instructions",
        "## Loaded memory index",
        "## Skill index",
    ] {
        assert!(prompt.contains(heading), "missing {heading}");
    }
}
