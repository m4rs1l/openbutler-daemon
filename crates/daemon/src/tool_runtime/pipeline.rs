//! The shared tool pipeline: the one path every native tool call takes —
//! gate, denial contract, ledger write-ahead, execution, and result
//! bounding. One definition per tool (`ToolDefinition`), one sequence for
//! all of them (`run_tool_call`); adding a tool never edits this file.
//!
//! Sources: legacy `src/tools/registry.ts` — `stopped` (L237),
//! `failClosed` (L286), `proposed` (L303), scope (L310), `autoApproved`
//! (L336), `gateReason` (L349), denials (L366-381), `approved`/`started`
//! (L417), catch (L461-497), `toolOutcome` (L650), `truncatedResult`
//! (L676); `src/tools/timeout.ts` (field, limits, schema injection).
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::ledger::{LedgerWriter, ToolActionState, ToolActionUpdate};
use crate::settings::ApprovalMode;
use crate::state::slice_utf16;
use crate::tool_runtime::tool_output::{ToolOutputWriter, tool_output_notice, tool_output_root_in};
use crate::tool_runtime::workspace_scope::WorkspaceScope;

/// Host control metadata: stripped before execution — a host option is never
/// a tool or server argument (timeout.ts L1).
pub const TOOL_TIMEOUT_FIELD: &str = "_timeoutSeconds";
/// The default tool timeout when settings carries none (source: 60 s).
pub const DEFAULT_TOOL_TIMEOUT_SECONDS: u64 = 60;
/// The hard ceiling every tool timeout is clamped to (source: 500 s).
pub const MAX_TOOL_TIMEOUT_SECONDS: u64 = 500;
/// The model-facing inline budget for one tool result (registry L454).
pub const MAX_TOOL_RESULT_CHARS: usize = 16_000;

/// One tool execution, as a future: a tool may await (the `question`
/// tool queues and waits for the owner), so `execute` is a boxed future
/// rather than a plain call.
pub type ToolFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send>>;

/// The answer a queued question resolves to: `None` when dismissed
/// (the owner's stop). The tool shapes its own result from it; the
/// channel only transports the answer.
pub type AskFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>>;

/// The question channel a tool may hold (the `question` tool's ask).
/// The host wires the runner in; a context without it answers with the
/// source's `unavailable` shape.
pub trait AskPort: Send + Sync + std::fmt::Debug {
    /// Queues the question and waits for the answer — `None` when the
    /// question was dismissed (the owner's stop).
    fn ask(&self, text: String, options: Option<Vec<String>>) -> AskFuture;
}

/// Everything a tool needs, owned by the host: the validated input (the
/// timeout field already removed), the policy in force, and the paths.
#[derive(Debug, Clone)]
/// What one tool call runs with: ids, arguments, timeout and the
/// approval decision — assembled before the gate, never after.
pub struct ToolCallContext {
    pub input: Value,
    pub timeout_seconds: u64,
    pub approval_mode: ApprovalMode,
    pub workspace_root: PathBuf,
    pub scratch_root: PathBuf,
    pub data_dir: PathBuf,
    pub session_id: String,
    pub command_shell: Option<String>,
    /// The question channel when a runner drives this call; `None` for
    /// callers with no interactive owner (the source's `ask ===
    /// undefined` path, question.ts L56).
    pub ask: Option<Arc<dyn AskPort>>,
}

impl ToolCallContext {
    /// Builds a context from model arguments, extracting the host's timeout
    /// field. An invalid value fails closed with the source sentence
    /// (timeout.ts L13-18); a valid one never reaches the tool.
    pub fn new(
        input: Value,
        approval_mode: ApprovalMode,
        workspace_root: PathBuf,
        scratch_root: PathBuf,
        data_dir: PathBuf,
        session_id: String,
        command_shell: Option<String>,
    ) -> Result<Self, String> {
        Self::with_ask(
            input,
            approval_mode,
            workspace_root,
            scratch_root,
            data_dir,
            session_id,
            command_shell,
            None,
        )
    }

    /// The same construction with a question channel (the batch loop
    /// wires the runner in; `new` stays the no-channel shape tests use).
    #[allow(clippy::too_many_arguments)]
    /// Derives a context carrying the question port (the ask tool awaits
    /// through it).
    pub fn with_ask(
        input: Value,
        approval_mode: ApprovalMode,
        workspace_root: PathBuf,
        scratch_root: PathBuf,
        data_dir: PathBuf,
        session_id: String,
        command_shell: Option<String>,
        ask: Option<Arc<dyn AskPort>>,
    ) -> Result<Self, String> {
        let timeout_seconds = tool_timeout_seconds(input.get(TOOL_TIMEOUT_FIELD))?;
        let mut input = input;
        if let Some(object) = input.as_object_mut() {
            object.remove(TOOL_TIMEOUT_FIELD);
        }
        Ok(Self {
            input,
            timeout_seconds,
            approval_mode,
            workspace_root,
            scratch_root,
            data_dir,
            session_id,
            command_shell,
            ask,
        })
    }
}

/// `toolTimeoutSeconds`: absent means the default; anything that is not a
/// safe integer inside 1-500 is refused (timeout.ts L13-18). `null` is
/// refused too — JS `Number.isSafeInteger(null)` is false, no coercion.
pub fn tool_timeout_seconds(value: Option<&Value>) -> Result<u64, String> {
    let Some(value) = value else {
        return Ok(DEFAULT_TOOL_TIMEOUT_SECONDS);
    };
    let seconds = value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|number| u64::try_from(number).ok()))
        .or_else(|| {
            value
                .as_f64()
                .filter(|number| {
                    number.fract() == 0.0
                        && (1.0..=MAX_TOOL_TIMEOUT_SECONDS as f64).contains(number)
                })
                .map(|number| number as u64)
        });
    match seconds {
        Some(seconds) if (1..=MAX_TOOL_TIMEOUT_SECONDS).contains(&seconds) => Ok(seconds),
        _ => Err(format!(
            "{TOOL_TIMEOUT_FIELD} must be an integer from 1 to {MAX_TOOL_TIMEOUT_SECONDS} seconds."
        )),
    }
}

/// One native tool. `scope` and `execute` are plain functions — the
/// definition carries no state, so tools are values, not closures.
#[derive(Clone)]
/// One registered tool: identity, input schema, effect declaration,
/// and the executor. Values, not services — the registry holds these.
pub struct ToolDefinition {
    pub provider_id: &'static str,
    pub name: &'static str,
    pub description: String,
    pub parameters: Value,
    /// The effect declaration as its exact wire string (the registry's
    /// `JSON.stringify(effect)` is order-sensitive byte-for-byte), with
    /// `effect()` for the ledger's structured copy.
    pub effect_json: &'static str,
    pub scope: fn(&ToolCallContext) -> WorkspaceScope,
    /// Runs the tool: a future, because a tool may await (the `question`
    /// tool queues for the owner). The context is cloned into the box —
    /// the future is `'static` and the caller never holds a borrow.
    pub execute: fn(&ToolCallContext) -> ToolFuture,
}

impl ToolDefinition {
    /// The tool's effect as the sidecar sees it (denotation JSON).
    pub fn effect(&self) -> Value {
        serde_json::from_str(self.effect_json).unwrap_or_else(|error| {
            panic!("catalog effect declarations must be valid JSON: {error}")
        })
    }

    /// The registry's model-tool identity: `providerId/name` (registry L546).
    pub fn identity(&self) -> String {
        format!("{}/{}", self.provider_id, self.name)
    }
}

/// The gate's answer. `gate_reason` values are the source's strings
/// (registry L349): `outside-workspace`, `unknown-scope`, `manual-review`.
#[derive(Debug, Clone, PartialEq, Eq)]
/// The gate's verdict before anything runs: allow, deny, or the
/// manual-review reason.
pub enum GateDecision {
    AutoApproved,
    NeedsApproval { gate_reason: &'static str },
}

/// What the approval channel answered. Auto-approval passes `Approved`
/// without consulting any channel.
#[derive(Debug, Clone, PartialEq, Eq)]
/// What an approval resolves to: approved, denied, or timed out.
pub enum ApprovalOutcome {
    Approved,
    Declined,
    AutoReviewDenied { reason: String },
    TimedOut { seconds: u64 },
}

/// The gate: full-access runs everything without consulting scope at all
/// (the source computes scope only for workspace/auto-review, L312);
/// those two modes auto-approve exactly the in-workspace calls (L341).
///
/// The `manual-review` reason exists in the source's mapping but only for
/// a scope that neither auto-approves nor classifies — unreachable with
/// today's three modes; the arm stays so the mapping stays complete.
pub fn assess(definition: &ToolDefinition, context: &ToolCallContext) -> GateDecision {
    match context.approval_mode {
        ApprovalMode::FullAccess => GateDecision::AutoApproved,
        ApprovalMode::Workspace | ApprovalMode::AutoReview => match (definition.scope)(context) {
            WorkspaceScope::Workspace => GateDecision::AutoApproved,
            WorkspaceScope::Outside => GateDecision::NeedsApproval {
                gate_reason: "outside-workspace",
            },
            WorkspaceScope::Unknown => GateDecision::NeedsApproval {
                gate_reason: "unknown-scope",
            },
        },
    }
}

/// The denial message (registry L366-381), shared by the result and the
/// ledger record.
pub fn denial_message(outcome: &ApprovalOutcome) -> String {
    match outcome {
        ApprovalOutcome::Declined => {
            "The approval was declined. The tool call was not run.".to_owned()
        }
        ApprovalOutcome::AutoReviewDenied { reason } => format!(
            "Auto-review denied this exact action: {reason} Do not retry it, change its wording, \
             or switch tools to bypass the decision. Propose a materially safer alternative or drop \
             this line of work and say so in your report."
        ),
        ApprovalOutcome::TimedOut { seconds } => format!(
            "No approval decision within {seconds} seconds: nobody approved or declined, so the \
             action was not run. This is not a refusal on the merits. Work around it: propose a \
             materially safer alternative that needs no approval, defer it, and name the blocked \
             action in your report."
        ),
        // Callers reach this builder only through a denial path (`run_tool_call`
        // gates on `Approved` first); the arm exists for exhaustiveness.
        ApprovalOutcome::Approved => String::new(),
    }
}

/// The denial result shape: `{ status: 'denied', error }` (registry L367).
pub fn denial_result(outcome: &ApprovalOutcome) -> Value {
    json!({ "status": "denied", "error": denial_message(outcome) })
}

/// A prior plain decline stops the run before any ledger write (L237).
pub fn stop_result() -> Value {
    json!({
        "status": "denied",
        "error": "A previous tool call was declined; this agent run has stopped.",
    })
}

/// Fail-closed wrappers (L286/L303/L352): every one is a result, never a
/// panic, so the model gets the sentence and the exchange stays paired.
pub fn policy_failure(message: &str) -> Value {
    json!({
        "status": "error",
        "error": format!("Tool approval policy failed; the tool was not run: {message}"),
    })
}

/// The model-visible refusal when a proposal could not be recorded
/// (write-ahead failure stops the run).
pub fn proposal_failure(message: &str) -> Value {
    json!({
        "status": "error",
        "error": format!("The tool was not run because its proposal could not be recorded: {message}"),
    })
}

/// The model-visible refusal when a denial could not be recorded.
pub fn approval_failure(message: &str) -> Value {
    json!({
        "status": "error",
        "error": format!("Tool approval failed; the tool was not run: {message}"),
    })
}

/// What one call produced, and whether the agent run must stop after it.
/// Only a plain decline or a fail-closed error stops the run (L365).
#[derive(Debug)]
/// One completed call: the model-visible result plus the state the
/// ledger records.
pub struct CallOutcome {
    pub result: Value,
    pub run_stopped: bool,
}

/// `toolOutcome` (registry L650-664): the ledger state for a result.
fn tool_outcome(result: &Value) -> ToolActionState {
    if let Some(record) = result.as_object() {
        match record.get("status").and_then(Value::as_str) {
            Some("cancelled") => return ToolActionState::Cancelled,
            Some("timed_out" | "failed" | "error") => return ToolActionState::Failed,
            _ => {}
        }
        if record
            .get("exitCode")
            .and_then(Value::as_i64)
            .is_some_and(|code| code != 0)
        {
            return ToolActionState::Failed;
        }
    }
    ToolActionState::Completed
}

/// `truncatedResult` (L676-701): over the inline budget the model gets a
/// preview cut at UTF-16 parity plus the spill path and notice; the rest
/// lives on disk. An unwritable spill degrades to the source's note.
fn bound_result(result: &Value, context: &ToolCallContext, tool: &str) -> Value {
    let Ok(serialized) = serde_json::to_string(result) else {
        return Value::Null;
    };
    let total_units = serialized.encode_utf16().count();
    if total_units <= MAX_TOOL_RESULT_CHARS {
        return result.clone();
    }
    let preview = slice_utf16(&serialized, MAX_TOOL_RESULT_CHARS);
    let root = tool_output_root_in(&context.data_dir);
    let spilled = (|| -> std::io::Result<(PathBuf, usize)> {
        let mut writer = ToolOutputWriter::open_at(&root, Some(&context.session_id), tool)?;
        writer.write(&serialized);
        let (path, _saved, dropped) = writer.close();
        Ok((path, dropped))
    })();
    match spilled {
        Ok((path, dropped)) => json!({
            "status": "truncated",
            "resultPreview": preview,
            "originalChars": total_units,
            "spilledTo": path.display().to_string(),
            "note": tool_output_notice(&path, total_units, dropped),
        }),
        Err(error) => json!({
            "status": "truncated",
            "resultPreview": preview,
            "originalChars": total_units,
            "note": format!(
                "The rest of this result could not be saved to disk ({error}). \
                 Narrow the request to see it."
            ),
        }),
    }
}

/// The full sequence for one tool call (registry L237-497): write-ahead
/// proposal, denial or execution, and exactly one ledger record per state.
/// Ledger failures land in the source's fail-closed shapes — never a panic.
pub async fn run_tool_call(
    definition: &ToolDefinition,
    context: &ToolCallContext,
    request_id: &str,
    event_id: Option<&str>,
    outcome: ApprovalOutcome,
    ledger: &mut LedgerWriter,
) -> CallOutcome {
    let base = ToolActionUpdate {
        request_id: request_id.to_owned(),
        workspace_root: context.workspace_root.display().to_string(),
        session_id: context.session_id.clone(),
        event_id: event_id.map(str::to_owned),
        provider_id: definition.provider_id.to_owned(),
        name: definition.name.to_owned(),
        input: context.input.clone(),
        effect: definition.effect(),
        state: ToolActionState::Proposed,
        result: None,
        error: None,
    };
    let mut write = |state: ToolActionState, result: Option<Value>, error: Option<String>| {
        ledger.record_tool_action(&ToolActionUpdate {
            state,
            result,
            error,
            ..base.clone()
        })
    };

    // Write-ahead: the proposal must exist before anything else (L303).
    if let Err(error) = write(ToolActionState::Proposed, None, None) {
        let message = error.to_string();
        let result = proposal_failure(&message);
        // The worst path (the call failed *and* the ledger write):
        // never silent — the run still carries the tool result.
        if let Err(error) = write(ToolActionState::Failed, Some(result.clone()), Some(message)) {
            eprintln!("openbutler-daemon: ledger action write: {error}");
        }
        return CallOutcome {
            result,
            run_stopped: true,
        };
    }

    // Denial: one `denied` record, and only a plain decline stops the run.
    if !matches!(outcome, ApprovalOutcome::Approved) {
        let message = denial_message(&outcome);
        let denial = denial_result(&outcome);
        let run_stopped = matches!(outcome, ApprovalOutcome::Declined);
        return match write(ToolActionState::Denied, Some(denial.clone()), None) {
            Ok(()) => CallOutcome {
                result: denial,
                run_stopped,
            },
            Err(error) => {
                // L383-390: a failed denial record rewords the result and
                // stops the run — the original sentence is never lost.
                CallOutcome {
                    result: json!({
                        "status": "error",
                        "error": format!("{message} The denial could not be recorded: {error}"),
                    }),
                    run_stopped: true,
                }
            }
        };
    }

    // Approved path: approved → started → execute → one outcome record
    // (L417-418). A failed record before execution never ran the tool.
    if let Err(error) = write(ToolActionState::Approved, None, None)
        .and_then(|()| write(ToolActionState::Started, None, None))
    {
        let message = format!("The tool was not run. {error}");
        let result = json!({ "status": "error", "error": message });
        // The worst path (the call failed *and* the ledger write):
        // never silent — the run still carries the tool result.
        if let Err(error) = write(ToolActionState::Failed, Some(result.clone()), Some(message)) {
            eprintln!("openbutler-daemon: ledger action write: {error}");
        }
        return CallOutcome {
            result,
            run_stopped: false,
        };
    }

    let executed = (definition.execute)(context).await;
    let state = tool_outcome(&executed);
    let bounded = bound_result(
        &executed,
        context,
        &format!("{}:{}", definition.provider_id, definition.name),
    );
    match write(state, Some(bounded.clone()), None) {
        Ok(()) => CallOutcome {
            result: bounded,
            run_stopped: false,
        },
        Err(error) => {
            // L463-497 with `executionCompleted`: the tool ran, the record
            // failed — a replacement error result, the run keeps going.
            let message = format!(
                "The tool ran, but its final result could not be recorded. Inspect the target \
                 before retrying. {error}"
            );
            let result = json!({ "status": "error", "error": message });
            // The worst path (the call failed *and* the ledger write):
            // never silent — the run still carries the tool result.
            if let Err(error) = write(ToolActionState::Failed, Some(result.clone()), Some(message))
            {
                eprintln!("openbutler-daemon: ledger action write: {error}");
            }
            CallOutcome {
                result,
                run_stopped: false,
            }
        }
    }
}
