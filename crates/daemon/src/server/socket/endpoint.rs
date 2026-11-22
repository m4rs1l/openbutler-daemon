//! Endpoint derivation — the single authority for how clients find us.
//!
//! The rule itself lives in the shared contract
//! (`openbutler_protocol::endpoint`) since the Rust CLI became the second
//! implementer (2026-11-22): one derivation, no drift. This module keeps
//! the daemon's import paths stable and re-exports it. Data-directory
//! resolution stays in `crate::config`.

pub use openbutler_protocol::endpoint::{Endpoint, ResolvedEndpoint, endpoint_for};
