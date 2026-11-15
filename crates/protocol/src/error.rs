//! Error types for framing and envelope validation.

use std::fmt;

/// Frame decoder failure. The message mirrors the TypeScript decoder exactly
/// so both sides report the same text when a peer breaks the cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// A frame (across all its chunks) exceeded the configured limit.
    ExceededLimit,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::ExceededLimit => write!(f, "IPC frame exceeded the 32 MB limit."),
        }
    }
}

impl std::error::Error for FrameError {}

/// Envelope validation failure (`isDaemonRequest` / `isBasicResponse`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    /// Frame carried a version this host does not speak.
    WrongVersion { expected: u32, actual: u32 },
    /// `id` is empty or longer than 64 JS string length units.
    InvalidIdLength { actual: usize },
    /// `action` is empty or longer than 64 JS string length units.
    InvalidActionLength { actual: usize },
    /// A response did not echo the requested version.
    ResponseVersionMismatch { expected: u32, actual: u32 },
    /// A response carried neither the success nor the failure shape.
    MalformedResponse,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValidationError::WrongVersion { expected, actual } => {
                write!(
                    f,
                    "Unsupported protocol version {actual}; this host speaks {expected}."
                )
            }
            ValidationError::InvalidIdLength { actual } => {
                write!(f, "Request id must be 1-64 characters, got {actual}.")
            }
            ValidationError::InvalidActionLength { actual } => {
                write!(f, "Action must be 1-64 characters, got {actual}.")
            }
            ValidationError::ResponseVersionMismatch { expected, actual } => {
                write!(
                    f,
                    "Response version {actual} does not echo the requested version {expected}."
                )
            }
            ValidationError::MalformedResponse => {
                write!(f, "Response carries neither a result nor an error.")
            }
        }
    }
}

impl std::error::Error for ValidationError {}
