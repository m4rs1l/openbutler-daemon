//! The daemon's two transport doors: framed socket and HTTP.
//!
//! Both doors only move bytes: framing, backpressure and the admission
//! gate live in their submodules, action routing in `dispatch`, every
//! decision in the host. A client picks a door — local pipe, remote TCP
//! or HTTP — and reaches the same API behind it; no door owns behavior
//! of its own, and no client-specific code belongs here.

pub mod http;
pub mod socket;
