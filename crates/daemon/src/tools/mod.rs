//! Host tools: the single path from a model tool call to an effect.
//!
//! One definition per tool, one gate every call passes through — the
//! pipeline lands with each tool (legacy `src/tools/`).

pub mod catalog;
pub mod execute_command;
pub mod format;
pub mod pipeline;
pub mod shell;
pub mod tool_output;
pub mod workspace_scope;
