//! The model's tools: one file (or folder) per tool, and nothing else.
//!
//! The machinery every call passes through — catalog wiring, the gate /
//! ledger / timeout pipeline, workspace scope classification, output
//! bounding and transcript formatting — lives in `crate::tool_runtime`.
//! A tool here holds only its own logic; adding a tool means one folder
//! in this module and one definition in the catalog.

pub mod execute_command;
