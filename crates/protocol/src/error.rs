//! Error types for framing and envelope validation.

/// Frame decoder failure. The message mirrors the TypeScript decoder exactly
/// so both sides report the same text when a peer breaks the cap.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    /// A frame (across all its chunks) exceeded the configured limit.
    #[error("IPC frame exceeded the 32 MB limit.")]
    ExceededLimit,
}

/// Envelope validation failure (`isDaemonRequest` / `isBasicResponse`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    /// Frame carried a version this host does not speak.
    #[error("Unsupported protocol version {actual}; this host speaks {expected}.")]
    WrongVersion { expected: u32, actual: u32 },
    /// `id` is empty or longer than 64 JS string length units.
    #[error("Request id must be 1-64 characters, got {actual}.")]
    InvalidIdLength { actual: usize },
    /// `action` is empty or longer than 64 JS string length units.
    #[error("Action must be 1-64 characters, got {actual}.")]
    InvalidActionLength { actual: usize },
    /// A response did not echo the requested version.
    #[error("Response version {actual} does not echo the requested version {expected}.")]
    ResponseVersionMismatch { expected: u32, actual: u32 },
    /// A response carried neither the success nor the failure shape.
    #[error("Response carries neither a result nor an error.")]
    MalformedResponse,
}
