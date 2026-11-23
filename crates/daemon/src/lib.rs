//! OpenButler backend library: state, dispatch, and the host runtime.
//!
//! The binary in `main.rs` stays a thin wiring layer; everything testable
//! lives here.

pub mod agent;
pub mod auth;
pub mod catalog;
pub mod compaction;
pub mod config;
pub mod default_model;
pub mod dispatch;
pub mod ledger;
pub mod listen_target;
pub mod schedule;
pub mod server;
pub mod settings;
pub mod shape;
pub mod sidecar;
pub mod start_lock;
pub mod state;
pub mod tailnet;
pub mod team;
pub mod tool_runtime;
pub mod tools;
