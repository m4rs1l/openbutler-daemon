//! OpenButler backend library: state, dispatch, and the host runtime.
//!
//! The binary in `main.rs` stays a thin wiring layer; everything testable
//! lives here.

pub mod agent;
pub mod auth;
pub mod config;
pub mod dispatch;
pub mod ledger;
pub mod listen_target;
pub mod server;
pub mod settings;
pub mod sidecar;
pub mod state;
pub mod tool_runtime;
pub mod tools;
