//! Tool result display: the settled transcript text for one outcome.
//!
//! Port of legacy `agent-session.ts` `formatToolResult` (L3051) and
//! `formatExecOutput` (L3304) — the branch order is theirs: host-tool
//! displayers first (they arrive with their tools), then `error`,
//! then `message`, then the generic execution shape. `execute_command`
//! rides the execution branch by having `stdout`/`stderr`.
//!
//! The transcript keeps `text || 'No output'` (the source's rule at
//! `onToolResult`, L1587).
use serde_json::Value;

/// `(outcome, text)` as the source computes them; the outcome feeds the
/// progress report, the text feeds the tool entry.
pub fn format_tool_result(name: &str, value: &Value) -> (String, String) {
    // Per-tool content displayers (`displayHostToolResult`) land with
    // their tools; every catalogued tool today is the generic shapes.
    let _ = name;
    if value.is_null() {
        return ("result".to_owned(), String::new());
    }
    if !value.is_object() {
        return ("result".to_owned(), scalar_text(value));
    }
    let Some(result) = value.as_object() else {
        unreachable!("is_object was checked");
    };
    if let Some(error) = result.get("error").and_then(Value::as_str)
        && !error.is_empty()
    {
        let status = result
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("error");
        return (status.to_owned(), error.to_owned());
    }
    if let Some(message) = result.get("message").and_then(Value::as_str)
        && !message.is_empty()
    {
        let status = result
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("result");
        return (status.to_owned(), message.to_owned());
    }
    format_exec_output(result)
}

/// JS `String(value)` for the scalar/array shapes that reach here.
fn scalar_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::String(text) => text.clone(),
                Value::Object(_) => "[object Object]".to_owned(),
                other => scalar_text(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        other => serde_json::to_string(other).unwrap_or_else(|_| String::from("undefined")),
    }
}

/// `formatExecOutput`: stdout/stderr joins with the exit note, the MCP
/// content-array shape, then the pretty-printed fallback.
fn format_exec_output(result: &serde_json::Map<String, Value>) -> (String, String) {
    if result.contains_key("stdout") || result.contains_key("stderr") {
        let status = result
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("completed");
        let exit = result
            .get("exitCode")
            .and_then(Value::as_i64)
            .map(|code| format!(" · exit {code}"))
            .unwrap_or_default();
        let text = ["stdout", "stderr", "error"]
            .iter()
            .filter_map(|key| result.get(*key).and_then(Value::as_str))
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_owned();
        return (format!("{status}{exit}"), text);
    }
    if let Some(content) = result.get("content").and_then(Value::as_array) {
        let text = content
            .iter()
            .map(|part| match part {
                Value::Object(fields) => {
                    if let Some(text) = fields.get("text").and_then(Value::as_str) {
                        text.to_owned()
                    } else if let Some(kind) = fields.get("type").and_then(Value::as_str) {
                        format!("[{kind}]")
                    } else {
                        String::new()
                    }
                }
                _ => String::new(),
            })
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        let structured = result.get("structuredContent");
        let fallback = structured
            .map(|value| serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()))
            .unwrap_or_default();
        let outcome = if result.get("isError") == Some(&Value::Bool(true)) {
            "server error"
        } else {
            "result"
        };
        return (
            outcome.to_owned(),
            if text.is_empty() { fallback } else { text },
        );
    }
    let outcome = result
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("result")
        .to_owned();
    let text = serde_json::to_string_pretty(result).unwrap_or_else(|_| String::from("undefined"));
    (outcome, text)
}
