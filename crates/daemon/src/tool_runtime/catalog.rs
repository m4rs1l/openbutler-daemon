//! The native tool catalog: one definition per tool, wired to the shared
//! pipeline. Adding a tool means one definition here and one file holding
//! its logic — the gate, ledger, timeout and bounding stay in `pipeline`.
//!
//! Sources: legacy `src/tools/catalog.ts`, `src/tools/execute-command.ts`
//! (definition L25-41), `src/tools/registry.ts` (model shape L546-547),
//! `src/tools/timeout.ts` (schema injection L31-36).
use std::path::Path;

use openbutler_protocol::sidecar::SidecarTool;
use serde_json::{Value, json};

use crate::tool_runtime::pipeline::{
    MAX_TOOL_TIMEOUT_SECONDS, TOOL_TIMEOUT_FIELD, ToolCallContext, ToolDefinition,
};
use crate::tool_runtime::workspace_scope::WorkspaceScope;
use crate::tools::execute_command::shell::resolve_shell;
use crate::tools::execute_command::{
    ExecuteEnvironment, command_scope, execute_command, resolve_command_directory,
};

/// The `question` tool's bounds (question.ts L4-6): UTF-16 units, like
/// the source's `string.length`.
const MAX_QUESTION_CHARS: usize = 2_000;
const MAX_OPTION_CHARS: usize = 120;
const MAX_OPTIONS: usize = 8;

/// The flagship native tool. The description carries the live workspace and
/// the resolved shell label; a shell nothing can resolve yields the source's
/// `unavailable` label without breaking the tool (execute-command.ts L19-24).
pub fn create_execute_command_tool(
    workspace_root: &Path,
    command_shell: Option<&str>,
) -> ToolDefinition {
    let shell_label = resolve_shell(command_shell)
        .map(|shell| shell.label)
        .unwrap_or_else(|_| "unavailable".to_owned());
    let description = format!(
        "Run a shell command in the active workspace ({}) by default. Use cwd for another \
         directory. Shell: {shell_label}. Shell commands can create, replace, and append to files \
         independently of file_write, including files already read or written in this session. \
         After a command that edits a file, file_read.sha256 reports its current on-disk hash. \
         Out-of-workspace working directories and common explicit paths use the configured \
         permission mode.",
        workspace_root.display()
    );
    ToolDefinition {
        provider_id: "host",
        name: "execute_command",
        description,
        parameters: json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The exact shell command to run." },
                "cwd": {
                    "type": "string",
                    "description": "Optional working directory. Relative paths are resolved from the session workspace.",
                },
            },
            "required": ["command"],
            "additionalProperties": false,
        }),
        effect_json: r#"{"change":"unknown","data":"unknown","network":"possible","reversible":"unknown","outbound":"model"}"#,
        scope: |context| {
            let command = context.input["command"].as_str().unwrap_or_default();
            let environment = environment(context);
            let target =
                resolve_command_directory(context.input.get("cwd"), &context.workspace_root);
            command_scope(command, &target, &environment)
        },
        // The sync command run behind the future interface: the
        // pipeline awaits every tool uniformly (`ToolFuture`).
        execute: |context| {
            let environment = environment(context);
            let target =
                resolve_command_directory(context.input.get("cwd"), &context.workspace_root);
            let input = context.input.clone();
            let timeout = context.timeout_seconds;
            Box::pin(
                async move { execute_command(&input, &environment, Some(&target), Some(timeout)) },
            )
        },
    }
}

/// The `question` tool: queues one question for the interactive owner
/// and waits for the answer (question.ts). `autoApprove: () => true`
/// ports as a workspace scope — the gate never blocks it in any mode.
pub fn create_question_tool() -> ToolDefinition {
    ToolDefinition {
        provider_id: "host",
        name: "question",
        description: "Ask the user one direct question during a run and wait for their answer. Provide short options when you can enumerate the likely answers; the user can still reply in their own words. Use it for a blocking choice or a missing detail you cannot discover with tools or safe defaults — not for progress updates or rhetorical questions. If no interactive user is attached, ask in your final reply; a worker reports it to its parent with parent_message or its final report. Preserve the blocker rather than guessing. Continue independent work while awaiting a required answer; silence is not approval.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The single question to ask, with enough context to answer it.",
                },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "maxItems": MAX_OPTIONS,
                    "description": "Optional 1-8 short suggested answers; the user may still answer freely.",
                },
            },
            "required": ["question"],
            "additionalProperties": false,
        }),
        effect_json:
            r#"{"change":"none","data":"local","network":"none","reversible":"full","outbound":"model"}"#,
        scope: |_| WorkspaceScope::Workspace,
        execute: |context| match validate_question(context) {
            Err(result) => Box::pin(async move { result }),
            Ok((question, options)) => match context.ask.clone() {
                // The source's `!context?.ask` path (question.ts L56).
                None => Box::pin(async {
                    json!({
                        "status": "unavailable",
                        "error": "No interactive user is attached to this run. Ask in your final reply; workers report to their parent. Continue independent work; do not assume a required answer.",
                    })
                }),
                Some(port) => Box::pin(async move {
                    match port.ask(question.clone(), options).await {
                        // A stop that dismissed the queue (source: `answer === null`).
                        None => json!({
                            "status": "dismissed",
                            "error": "The user dismissed the question without answering. Continue with your best judgment, state the assumption you made, or include the question in your final response.",
                        }),
                        Some(answer) => {
                            json!({ "status": "answered", "question": question, "answer": answer })
                        }
                    }
                }),
            },
        },
    }
}

/// The source's execute-time validation order (question.ts): trim and
/// bound the question, then the options, then the ask gate.
fn validate_question(context: &ToolCallContext) -> Result<(String, Option<Vec<String>>), Value> {
    let question = context
        .input
        .get("question")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    if question.is_empty() {
        return Err(json!({
            "status": "invalid",
            "error": "question must be a non-empty string.",
        }));
    }
    if question.encode_utf16().count() > MAX_QUESTION_CHARS {
        return Err(json!({
            "status": "invalid",
            "error": format!("question must be at most {MAX_QUESTION_CHARS} characters."),
        }));
    }
    let options = match context.input.get("options") {
        None => None,
        Some(raw) => {
            let invalid_message = || {
                json!({
                    "status": "invalid",
                    "error": format!(
                        "options must be 1-{MAX_OPTIONS} non-empty strings of at most \
                         {MAX_OPTION_CHARS} characters."
                    ),
                })
            };
            // An explicit `null` is invalid too: JS `Array.isArray(null)`
            // is false and the key is not `undefined`.
            let Some(items) = raw.as_array() else {
                return Err(invalid_message());
            };
            if items.is_empty() || items.len() > MAX_OPTIONS {
                return Err(invalid_message());
            }
            let mut trimmed = Vec::with_capacity(items.len());
            for item in items {
                let Some(option) = item.as_str() else {
                    return Err(invalid_message());
                };
                let option = option.trim();
                if option.is_empty() || option.encode_utf16().count() > MAX_OPTION_CHARS {
                    return Err(invalid_message());
                }
                trimmed.push(option.to_owned());
            }
            Some(trimmed)
        }
    };
    Ok((question, options))
}

/// The catalog lookup the batch loop runs per model call: one name, one
/// definition built from the turn's workspace and shell setting. Unknown
/// names return `None` and pair an authored error result.
pub fn native_definition(
    name: &str,
    workspace_root: &Path,
    command_shell: Option<&str>,
) -> Option<ToolDefinition> {
    match name {
        "execute_command" => Some(create_execute_command_tool(workspace_root, command_shell)),
        "question" => Some(create_question_tool()),
        _ => None,
    }
}

/// The execution environment a tool derives from the host context.
fn environment(context: &ToolCallContext) -> ExecuteEnvironment {
    ExecuteEnvironment {
        workspace_root: context.workspace_root.clone(),
        scratch_root: context.scratch_root.clone(),
        command_shell: context.command_shell.clone(),
        session_id: Some(context.session_id.clone()),
        data_dir: context.data_dir.clone(),
    }
}

/// The model-facing shape (registry L546-547): the denotation suffix
/// `Source: provider/name. Declared effects: <json>.` and the host's
/// `_timeoutSeconds` injected into the schema. The field is reserved —
/// a schema already carrying it is refused (timeout.ts L34).
pub fn sidecar_tool(definition: &ToolDefinition) -> Result<SidecarTool, String> {
    let mut parameters = definition.parameters.clone();
    if !parameters.is_object() {
        return Err(format!(
            "Tool {}/{} has no object input schema.",
            definition.provider_id, definition.name
        ));
    }
    let existing = match parameters.get("properties") {
        Some(Value::Object(properties)) => properties.clone(),
        _ => serde_json::Map::new(),
    };
    if existing.contains_key(TOOL_TIMEOUT_FIELD) {
        return Err(format!(
            "{TOOL_TIMEOUT_FIELD} is reserved for host execution control."
        ));
    }
    let mut properties = existing;
    properties.insert(
        TOOL_TIMEOUT_FIELD.to_owned(),
        json!({ "type": "integer", "minimum": 1, "maximum": MAX_TOOL_TIMEOUT_SECONDS }),
    );
    // `parameters` was proven an object above, so this insert always lands.
    if let Some(object) = parameters.as_object_mut() {
        object.insert("properties".to_owned(), Value::Object(properties));
    }
    let description = format!(
        "{}\nSource: {}/{}. Declared effects: {}.",
        definition.description, definition.provider_id, definition.name, definition.effect_json
    );
    Ok(SidecarTool {
        name: definition.name.to_owned(),
        description: Some(description),
        parameters,
    })
}
