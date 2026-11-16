//! OpenButler backend library: state, dispatch, and the host runtime.
//!
//! The binary in `main.rs` stays a thin wiring layer; everything testable
//! lives here.

pub mod auth;
pub mod config;
pub mod dispatch;
pub mod server;
pub mod state;
