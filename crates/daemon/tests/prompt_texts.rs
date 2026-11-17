//! The prompt texts are a byte contract: extracted mechanically from the
//! legacy sources, they may not drift, grow slots, or lose sections. The
//! assembly that wraps them is the runtime slice — these pins hold the
//! texts themselves.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use openbutler_daemon::agent::prompt::{
    AGENT_BASE, AGENT_INSTRUCTIONS, BACKGROUND_INSTRUCTIONS, CUSTODIAN_BASE,
    CUSTODIAN_INSTRUCTIONS, MEMORY_DOCUMENT_GUIDANCE, PromptRole, TOOL_TIMEOUT_GUIDANCE,
    WORKER_BASE, WORKER_INSTRUCTIONS, background_policy,
};

const ALL_TEXTS: [(&str, &str); 6] = [
    ("agent", AGENT_INSTRUCTIONS),
    ("custodian", CUSTODIAN_INSTRUCTIONS),
    ("worker", WORKER_INSTRUCTIONS),
    ("background", BACKGROUND_INSTRUCTIONS),
    ("memory-guidance", MEMORY_DOCUMENT_GUIDANCE),
    ("timeout-guidance", TOOL_TIMEOUT_GUIDANCE),
];

#[test]
fn every_prompt_text_carries_its_source_opening() {
    assert!(
        AGENT_INSTRUCTIONS
            .starts_with("You are an OpenButler agent, working on the owner's machine")
    );
    assert!(CUSTODIAN_INSTRUCTIONS.starts_with("You are the machine's custodian"));
    assert!(WORKER_INSTRUCTIONS.starts_with("You are a worker: a bounded execution context"));
    assert!(BACKGROUND_INSTRUCTIONS.starts_with("This session executes a saved schedule"));
    assert!(
        MEMORY_DOCUMENT_GUIDANCE.starts_with("Memory is Markdown under the persistent memory root")
    );
    assert!(TOOL_TIMEOUT_GUIDANCE.starts_with("For every tool call you may set"));
}

#[test]
fn no_prompt_text_contains_an_unresolved_slot() {
    for (name, text) in ALL_TEXTS {
        assert!(
            !text.contains('\0'),
            "{name} still carries template slot markers"
        );
        assert!(
            !text.contains('\r'),
            "{name} must stay LF-only — CRLF would break the byte contract"
        );
        assert!(
            !text.contains("${"),
            "{name} still carries an unevaluated expression"
        );
        assert!(!text.is_empty(), "{name} is empty");
    }
}

#[test]
fn the_timeout_guidance_carries_the_resolved_constants() {
    // The three constants are part of the guidance's meaning: field name,
    // default and maximum — resolved from timeout.ts at extraction time.
    assert!(TOOL_TIMEOUT_GUIDANCE.contains("_timeoutSeconds"));
    assert!(TOOL_TIMEOUT_GUIDANCE.contains("60"));
    assert!(TOOL_TIMEOUT_GUIDANCE.contains("500"));
    assert!(!TOOL_TIMEOUT_GUIDANCE.contains("TOOL_TIMEOUT_FIELD"));
}

#[test]
fn the_memory_guidance_carries_the_resolved_index_budgets() {
    assert!(MEMORY_DOCUMENT_GUIDANCE.contains("200"));
    assert!(
        !MEMORY_DOCUMENT_GUIDANCE.contains("MEMORY_INDEX_MAX"),
        "constant names never reach the model"
    );
}

#[test]
fn the_background_policy_is_the_exact_composition() {
    let policy = background_policy();
    let separator = "\n\n## This scheduled run\n";
    let agent_end = policy
        .find(separator)
        .expect("the heading separator exists");
    assert_eq!(
        &policy[..agent_end],
        AGENT_INSTRUCTIONS,
        "agent text first, untouched"
    );
    assert_eq!(
        &policy[agent_end + separator.len()..],
        BACKGROUND_INSTRUCTIONS,
        "the run's own note last, untouched"
    );
    assert_eq!(
        policy.matches(separator).count(),
        1,
        "the separator appears exactly once"
    );
}

#[test]
fn the_bases_carry_the_right_role_and_text() {
    assert_eq!(AGENT_BASE.role, PromptRole::Agent);
    assert_eq!(CUSTODIAN_BASE.role, PromptRole::Custodian);
    assert_eq!(WORKER_BASE.role, PromptRole::Worker);
    assert_eq!(AGENT_BASE.instructions, AGENT_INSTRUCTIONS);
    assert_eq!(CUSTODIAN_BASE.instructions, CUSTODIAN_INSTRUCTIONS);
    assert_eq!(WORKER_BASE.instructions, WORKER_INSTRUCTIONS);
}
