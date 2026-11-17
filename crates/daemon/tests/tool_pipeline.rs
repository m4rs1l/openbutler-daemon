//! c2 — the shared tool pipeline: the gate every tool call passes, the
//! denial contract, the ledger sequence, and the model-facing wire shape.
//! One definition per tool, one path for all of them.
//!
//! Sources: legacy `src/tools/registry.ts` — `stopped` (L237), `failClosed`
//! (L286), `proposed` (L303), scope (L310), `autoApproved` (L336),
//! `gateReason` (L349), denial messages (L366-381), `approved`/`started`
//! (L417), result bounding (L454-459), `truncatedResult` (L676),
//! `toolOutcome` (L650); `src/tools/timeout.ts` (field + limits);
//! `src/tools/execute-command.ts` (definition, L25-41).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::temp_data_dir;
use openbutler_daemon::ledger::LedgerWriter;
use openbutler_daemon::settings::ApprovalMode;
use openbutler_daemon::tool_runtime::catalog::{create_execute_command_tool, sidecar_tool};
use openbutler_daemon::tool_runtime::pipeline::{
    ApprovalOutcome, DEFAULT_TOOL_TIMEOUT_SECONDS, GateDecision, TOOL_TIMEOUT_FIELD,
    ToolCallContext, ToolDefinition, ToolFuture, approval_failure, assess, denial_message,
    denial_result, policy_failure, proposal_failure, run_tool_call, stop_result,
    tool_timeout_seconds,
};
use openbutler_daemon::tool_runtime::workspace_scope::WorkspaceScope;
use serde_json::json;
use std::path::Path;

/// A definition whose scope hook panics: in full-access mode the source never
/// computes scope at all (registry.ts L312), so reaching it is the test.
fn unreachable_scope(_context: &ToolCallContext) -> WorkspaceScope {
    panic!("scope must not be computed in full-access mode");
}

fn workspace_scope(_context: &ToolCallContext) -> WorkspaceScope {
    WorkspaceScope::Workspace
}

fn outside_scope(_context: &ToolCallContext) -> WorkspaceScope {
    WorkspaceScope::Outside
}

fn unknown_scope(_context: &ToolCallContext) -> WorkspaceScope {
    WorkspaceScope::Unknown
}

/// The execute path under test: a no-effect echo stand-in for the ledger
/// sequence; definitions carry function pointers, so helpers never capture.
fn echo_tool() -> ToolDefinition {
    ToolDefinition {
        provider_id: "host",
        name: "execute_command",
        description: "echo stand-in".to_owned(),
        parameters: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        effect_json: r#"{"change":"unknown","data":"unknown","network":"possible","reversible":"unknown","outbound":"model"}"#,
        scope: workspace_scope,
        execute: |context| {
            let command = context.input["command"]
                .as_str()
                .unwrap_or("echo")
                .to_owned();
            Box::pin(async move {
                json!({ "status": "completed", "exitCode": 0, "stdout": format!("{command}\n"), "stderr": "" })
            })
        },
    }
}

/// The non-zero exit path behind the future interface.
fn exit_seven(_context: &ToolCallContext) -> ToolFuture {
    Box::pin(async { json!({ "status": "completed", "exitCode": 7, "stdout": "", "stderr": "" }) })
}

fn invalid_tool() -> ToolDefinition {
    let mut definition = echo_tool();
    definition.execute = |_| {
        Box::pin(async { json!({ "status": "invalid", "error": "command must be a string." }) })
    };
    definition
}

/// An oversized result: 16_001 characters of text over the inline budget.
fn big_result(_context: &ToolCallContext) -> ToolFuture {
    let result =
        json!({ "status": "completed", "exitCode": 0, "text": "x".repeat(16_001), "stdout": "" });
    Box::pin(async move { result })
}

fn context(workspace: &Path, mode: ApprovalMode) -> ToolCallContext {
    ToolCallContext::new(
        json!({ "command": "echo hello" }),
        mode,
        workspace.to_path_buf(),
        workspace.join("scratch"),
        workspace.to_path_buf(),
        "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_owned(),
        None,
    )
    .expect("plain input carries no timeout field")
}

fn transitions(data_dir: &Path, request_id: &str) -> Vec<String> {
    let connection = rusqlite::Connection::open(data_dir.join("state.db")).unwrap();
    let mut statement = connection
        .prepare(
            "SELECT state FROM openbutler_action_transitions \
             WHERE request_id = ?1 ORDER BY id",
        )
        .unwrap();
    statement
        .query_map([request_id], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

// --- gate --------------------------------------------------------------------

#[test]
fn gate_decisions_follow_the_source_rules() {
    let dir = temp_data_dir("pipeline-gate");

    // Full-access: everything runs, and scope is never consulted (L312).
    let full = context(&dir, ApprovalMode::FullAccess);
    let full_tool = ToolDefinition {
        scope: unreachable_scope,
        ..echo_tool()
    };
    assert!(matches!(
        assess(&full_tool, &full),
        GateDecision::AutoApproved
    ));

    // Workspace / auto-review auto-approve an in-workspace scope (L341).
    for mode in [ApprovalMode::Workspace, ApprovalMode::AutoReview] {
        let ctx = context(&dir, mode);
        assert!(matches!(
            assess(&echo_tool(), &ctx),
            GateDecision::AutoApproved
        ));
    }

    // Outside scope: gateReason is scope-first, mode-agnostic (L349).
    for mode in [ApprovalMode::Workspace, ApprovalMode::AutoReview] {
        let ctx = context(&dir, mode);
        let tool = ToolDefinition {
            scope: outside_scope,
            ..echo_tool()
        };
        assert!(
            matches!(
                assess(&tool, &ctx),
                GateDecision::NeedsApproval {
                    gate_reason: "outside-workspace"
                }
            ),
            "{mode:?}"
        );
        let tool = ToolDefinition {
            scope: unknown_scope,
            ..echo_tool()
        };
        assert!(
            matches!(
                assess(&tool, &ctx),
                GateDecision::NeedsApproval {
                    gate_reason: "unknown-scope"
                }
            ),
            "{mode:?}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// --- denial contract ---------------------------------------------------------

#[test]
fn denial_results_are_the_source_sentences() {
    let declined = denial_result(&ApprovalOutcome::Declined);
    assert_eq!(declined["status"], "denied");
    assert_eq!(
        declined["error"],
        "The approval was declined. The tool call was not run."
    );

    let denied = denial_result(&ApprovalOutcome::AutoReviewDenied {
        reason: "this writes outside the workspace.".to_owned(),
    });
    assert_eq!(
        denied["error"],
        "Auto-review denied this exact action: this writes outside the workspace. Do not retry it, change its wording, or switch tools to bypass the decision. Propose a materially safer alternative or drop this line of work and say so in your report."
    );

    // An absent reason interpolates as an empty string (L370): the sentence
    // keeps its double space rather than rewording itself.
    let absent = denial_result(&ApprovalOutcome::AutoReviewDenied {
        reason: String::new(),
    });
    assert_eq!(
        absent["error"],
        "Auto-review denied this exact action:  Do not retry it, change its wording, or switch tools to bypass the decision. Propose a materially safer alternative or drop this line of work and say so in your report."
    );
    // The message builder is the single source: denial results and ledger
    // records share it.
    assert_eq!(
        denial_message(&ApprovalOutcome::Declined),
        "The approval was declined. The tool call was not run."
    );

    let timed_out = denial_result(&ApprovalOutcome::TimedOut { seconds: 60 });
    assert_eq!(
        timed_out["error"],
        "No approval decision within 60 seconds: nobody approved or declined, so the action was not run. This is not a refusal on the merits. Work around it: propose a materially safer alternative that needs no approval, defer it, and name the blocked action in your report."
    );
    let custom = denial_result(&ApprovalOutcome::TimedOut { seconds: 120 });
    assert!(
        custom["error"]
            .as_str()
            .unwrap()
            .starts_with("No approval decision within 120 seconds:")
    );

    // Prior decline stops the run before any ledger write (L237).
    let stopped = stop_result();
    assert_eq!(stopped["status"], "denied");
    assert_eq!(
        stopped["error"],
        "A previous tool call was declined; this agent run has stopped."
    );

    // Fail-closed branches keep their own sentences (L286/L303/L352).
    let policy = policy_failure("boom");
    assert_eq!(policy["status"], "error");
    assert_eq!(
        policy["error"],
        "Tool approval policy failed; the tool was not run: boom"
    );
    assert_eq!(
        proposal_failure("boom")["error"],
        "The tool was not run because its proposal could not be recorded: boom"
    );
    assert_eq!(
        approval_failure("boom")["error"],
        "Tool approval failed; the tool was not run: boom"
    );
}

// --- ledger sequence ---------------------------------------------------------

#[tokio::test]
async fn the_ledger_records_the_full_sequence() {
    let dir = temp_data_dir("pipeline-seq");
    let mut ledger = LedgerWriter::open(&dir).expect("writer opens");
    let ctx = context(&dir, ApprovalMode::Workspace);

    // Auto-approved run: proposed → approved → started → completed (L303, L417).
    let outcome = run_tool_call(
        &echo_tool(),
        &ctx,
        "req-ok",
        None,
        ApprovalOutcome::Approved,
        &mut ledger,
    )
    .await;
    assert_eq!(outcome.result["status"], "completed");
    assert!(!outcome.run_stopped, "a success never stops the run");
    assert_eq!(
        transitions(&dir, "req-ok"),
        ["proposed", "approved", "started", "completed"],
        "write-ahead order"
    );

    // A non-zero exit is a failed state with a normal result (L656-658):
    // execution failures are results, not run stops.
    let outcome = run_tool_call(
        &ToolDefinition {
            execute: exit_seven,
            ..echo_tool()
        },
        &ctx,
        "req-exit",
        None,
        ApprovalOutcome::Approved,
        &mut ledger,
    )
    .await;
    assert!(!outcome.run_stopped);
    assert_eq!(transitions(&dir, "req-exit").last().unwrap(), "failed");

    // The source maps every other status — including a validation refusal
    // that never ran a command — to `completed` (L650-664).
    let outcome = run_tool_call(
        &invalid_tool(),
        &ctx,
        "req-invalid",
        None,
        ApprovalOutcome::Approved,
        &mut ledger,
    )
    .await;
    assert_eq!(
        outcome.result["status"], "invalid",
        "the tool result passes through unchanged"
    );
    assert_eq!(
        transitions(&dir, "req-invalid").last().unwrap(),
        "completed"
    );

    // A plain decline: proposed → denied, and it stops the agent run.
    let outcome = run_tool_call(
        &echo_tool(),
        &ctx,
        "req-declined",
        None,
        ApprovalOutcome::Declined,
        &mut ledger,
    )
    .await;
    assert!(outcome.run_stopped, "a decline stops the run (L365)");
    assert_eq!(outcome.result["status"], "denied");
    assert_eq!(transitions(&dir, "req-declined"), ["proposed", "denied"]);

    // Auto-review denial and approval timeouts never stop the run (L365).
    for (index, resolution) in [
        ApprovalOutcome::AutoReviewDenied {
            reason: "risky.".to_owned(),
        },
        ApprovalOutcome::TimedOut { seconds: 60 },
    ]
    .into_iter()
    .enumerate()
    {
        let request_id = format!("req-soft-{index}");
        let outcome = run_tool_call(
            &echo_tool(),
            &ctx,
            &request_id,
            None,
            resolution,
            &mut ledger,
        )
        .await;
        assert!(!outcome.run_stopped, "soft denials keep the run alive");
        assert_eq!(transitions(&dir, &request_id), ["proposed", "denied"]);
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// --- timeout field + wire shape ---------------------------------------------

#[test]
fn the_timeout_field_is_host_owned_and_stripped() {
    let dir = temp_data_dir("pipeline-timeout");

    // Present: extracted and removed — a host option never reaches the tool
    // (timeout.ts L1: "control metadata never becomes tool/server arguments").
    let ctx = ToolCallContext::new(
        json!({ "command": "echo", "_timeoutSeconds": 120 }),
        ApprovalMode::FullAccess,
        dir.clone(),
        dir.join("scratch"),
        dir.clone(),
        "session".to_owned(),
        None,
    )
    .expect("valid timeout");
    assert_eq!(ctx.timeout_seconds, 120);
    assert!(
        ctx.input.get(TOOL_TIMEOUT_FIELD).is_none(),
        "stripped from the tool input"
    );

    // Absent: the default of 60 (timeout.ts L7).
    let ctx = context(&dir, ApprovalMode::FullAccess);
    assert_eq!(ctx.timeout_seconds, DEFAULT_TOOL_TIMEOUT_SECONDS);

    // Invalid values fail closed with the source sentence (timeout.ts L15).
    for bad in [json!(0), json!(501), json!(1.5), json!("30"), json!(true)] {
        let error = ToolCallContext::new(
            json!({ "command": "echo", "_timeoutSeconds": bad }),
            ApprovalMode::FullAccess,
            dir.clone(),
            dir.join("scratch"),
            dir.clone(),
            "session".to_owned(),
            None,
        )
        .expect_err("rejected");
        assert_eq!(
            error,
            "_timeoutSeconds must be an integer from 1 to 500 seconds."
        );
    }

    // The parser takes the field's value — absent is the default (60),
    // present-but-invalid is refused, `null` included (timeout.ts L15).
    assert_eq!(tool_timeout_seconds(None).unwrap(), 60);
    assert_eq!(tool_timeout_seconds(Some(&json!(500))).unwrap(), 500);
    assert!(tool_timeout_seconds(Some(&json!(1.5))).is_err());
    assert!(tool_timeout_seconds(Some(&json!(null))).is_err());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_wire_shape_matches_the_registry_s_denotation() {
    let dir = temp_data_dir("pipeline-wire");
    // A shell setting nothing can resolve → the description carries the
    // source's `unavailable` label (execute-command.ts L19-24).
    let definition = create_execute_command_tool(&dir, Some("not-a-real-shell-xyz"));
    let tool = sidecar_tool(&definition).expect("reserved field absent");

    assert_eq!(tool.name, "execute_command");
    let description = tool.description.as_deref().unwrap();
    assert!(
        description.starts_with(&format!(
            "Run a shell command in the active workspace ({}) by default. Use cwd for another directory. Shell: unavailable. ",
            dir.display()
        )),
        "{description}"
    );
    // The registry's denotation suffix, effect byte-exact (registry L546).
    assert!(
        description.ends_with(
            "Source: host/execute_command. Declared effects: \
             {\"change\":\"unknown\",\"data\":\"unknown\",\"network\":\"possible\",\
             \"reversible\":\"unknown\",\"outbound\":\"model\"}."
        ),
        "{description}"
    );

    // Schema: base properties intact, timeout injected (timeout.ts L31-33).
    assert_eq!(
        tool.parameters["properties"][TOOL_TIMEOUT_FIELD],
        json!({ "type": "integer", "minimum": 1, "maximum": 500 }),
    );
    assert_eq!(tool.parameters["required"], json!(["command"]));
    assert_eq!(tool.parameters["additionalProperties"], json!(false));
    assert!(tool.parameters["properties"]["command"]["description"].is_string());
    assert_eq!(
        tool.parameters["properties"]["cwd"]["description"],
        "Optional working directory. Relative paths are resolved from the session workspace."
    );

    // The field is reserved: a schema carrying it is refused (timeout.ts L34).
    let reserved = ToolDefinition {
        parameters: json!({
            "type": "object",
            "properties": { "_timeoutSeconds": { "type": "string" } }
        }),
        ..create_execute_command_tool(&dir, None)
    };
    assert_eq!(
        sidecar_tool(&reserved).expect_err("reserved field rejected"),
        "_timeoutSeconds is reserved for host execution control."
    );

    // The effect declaration parses for the ledger as the same five axes.
    assert_eq!(
        definition.effect(),
        json!({ "change": "unknown", "data": "unknown", "network": "possible",
                "reversible": "unknown", "outbound": "model" })
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// --- result bounding ---------------------------------------------------------

#[tokio::test]
async fn results_over_the_inline_budget_spill_to_disk() {
    let dir = temp_data_dir("pipeline-spill");
    let mut ledger = LedgerWriter::open(&dir).expect("writer opens");
    let ctx = context(&dir, ApprovalMode::FullAccess);

    let serialized_chars =
        json!({ "status": "completed", "exitCode": 0, "text": "x".repeat(16_001), "stdout": "" })
            .to_string()
            .len();
    let outcome = run_tool_call(
        &ToolDefinition {
            execute: big_result,
            ..echo_tool()
        },
        &ctx,
        "req-spill",
        None,
        ApprovalOutcome::Approved,
        &mut ledger,
    )
    .await;

    let result = &outcome.result;
    assert_eq!(result["status"], "truncated");
    assert_eq!(
        result["resultPreview"].as_str().unwrap().len(),
        16_000,
        "the preview is cut at the inline budget"
    );
    assert_eq!(
        result["originalChars"],
        json!(serialized_chars),
        "the declared size is the full serialized result"
    );
    assert!(result["spilledTo"].is_string(), "the rest is on disk");
    let note = result["note"].as_str().unwrap();
    assert!(
        note.starts_with("The full output (")
            && note.contains(result["spilledTo"].as_str().unwrap()),
        "{note}"
    );

    // The ledger keeps the bounded shape, not the megabyte original.
    let connection = rusqlite::Connection::open(dir.join("state.db")).unwrap();
    let recorded: String = connection
        .query_row(
            "SELECT result_json FROM openbutler_actions WHERE request_id = 'req-spill'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(recorded.contains("\"truncated\""), "{recorded}");

    let _ = std::fs::remove_dir_all(&dir);
}
