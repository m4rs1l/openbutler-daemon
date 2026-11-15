//! Wire contract between the OpenButler daemon and its clients.
//!
//! This crate is the single source of truth for message shapes, framing, and
//! the protocol version. It stays dependency-light so every client can consume
//! it. Types are populated by the contract-extraction phase (docs/index.md).

#![forbid(unsafe_code)]

/// Version of the wire protocol, bumped on every breaking change.
/// Clients declare the minimum version they require against this number.
pub const PROTOCOL_VERSION: u32 = 1;
