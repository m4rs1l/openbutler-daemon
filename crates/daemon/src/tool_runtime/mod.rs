//! The tool runtime: machinery shared by every native tool call.
//!
//! One definition per tool (catalog), one sequence every call passes
//! through (pipeline: gate, denial contract, ledger write-ahead,
//! execution, result bounding), plus the supporting authorities:
//! workspace scope classification, the spill writer, and the settled
//! transcript formatting. The tools themselves live in `crate::tools`;
//! nothing in this module is a tool.

pub mod catalog;
pub mod format;
pub mod pipeline;
pub mod tool_output;
pub mod workspace_scope;
