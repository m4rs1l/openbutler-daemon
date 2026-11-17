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

use crate::tools::execute_command::{
    ExecuteEnvironment, command_scope, execute_command, resolve_command_directory,
};
use crate::tools::pipeline::{
    MAX_TOOL_TIMEOUT_SECONDS, TOOL_TIMEOUT_FIELD, ToolCallContext, ToolDefinition,
};
use crate::tools::shell::resolve_shell;

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
        execute: |context| {
            let environment = environment(context);
            let target =
                resolve_command_directory(context.input.get("cwd"), &context.workspace_root);
            execute_command(
                &context.input,
                &environment,
                Some(&target),
                Some(context.timeout_seconds),
            )
        },
    }
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
