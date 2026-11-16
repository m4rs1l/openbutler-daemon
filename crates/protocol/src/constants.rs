//! Protocol constants: versions, caps, deadlines, and message limits.
//!
//! Every value here is pinned by the TypeScript implementation; the source
//! anchor is noted per group so a drift is caught during review, not in
//! production.

/// Wire version carried by every frame (TS `DAEMON_PROTOCOL_VERSION`).
pub const DAEMON_PROTOCOL_VERSION: u32 = 2;

/// Hard frame cap in both directions (TS `MAX_DAEMON_FRAME_BYTES`).
/// Exceeding it destroys the connection with a named error; frames are never
/// silently truncated.
pub const MAX_DAEMON_FRAME_BYTES: usize = 32 * 1024 * 1024;

/// Host-side outbound queue cap; an unread peer is disconnected rather than
/// allowed to grow host memory (`daemon-host.ts` `MAX_QUEUED_OUTPUT_BYTES`).
pub const MAX_QUEUED_OUTPUT_BYTES: usize = 32 * 1024 * 1024;

/// Request `id` and `action` are 1..=64 JS string length units
/// (`daemon-host.ts` `isDaemonRequest`).
pub const MAX_ID_LENGTH: usize = 64;
pub const MAX_ACTION_LENGTH: usize = 64;

/// Client-side deadlines (milliseconds).
///
/// | Constant | Value | Source |
/// |---|---|---|
/// | connect | 10 000 | `CLIENT_CONNECT_TIMEOUT_MS` |
/// | auth handshake | 10 000 | `AUTH_HANDSHAKE_TIMEOUT_MS` |
/// | attach request | 90 000 | `DaemonSessionClient.request` |
/// | any other request | 30 000 | `DaemonSessionClient.request` |
/// | control-plane probe | 5 000 | `daemon.ts` `requestDaemon` |
/// | host chat load during attach | 20 000 | attach path |
pub const CLIENT_CONNECT_TIMEOUT_MS: u64 = 10_000;
pub const AUTH_HANDSHAKE_TIMEOUT_MS: u64 = 10_000;
pub const ATTACH_REQUEST_TIMEOUT_MS: u64 = 90_000;
pub const REQUEST_TIMEOUT_MS: u64 = 30_000;
pub const CONTROL_REQUEST_TIMEOUT_MS: u64 = 5_000;
pub const HOST_LOAD_DEADLINE_MS: u64 = 20_000;

/// Reconnect backoff: `min(5000, 250 * 2^min(attempt, 5))` ms.
pub const RECONNECT_BACKOFF_BASE_MS: u64 = 250;
pub const RECONNECT_BACKOFF_CAP_MS: u64 = 5_000;
pub const RECONNECT_BACKOFF_MAX_EXPONENT: u32 = 5;

/// Remote pairing (`daemon-pairing.test.ts`): 12 hex characters, five minutes,
/// one outstanding code held in host memory only.
pub const PAIR_CODE_LENGTH: usize = 12;
pub const PAIR_CODE_TTL_MS: u64 = 5 * 60 * 1_000;
pub const AUTH_TOKEN_PREFIX: &str = "obt_";

/// Message payload limits (`session.send`, attach window, paging).
pub const MAX_MESSAGE_TEXT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_MESSAGE_IMAGES: usize = 8;
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// Attachment media types a browser renders inline (`daemon-host.ts`
/// `INLINE_IMAGE_MEDIA_TYPES`): the `session.find-attachment` filter.
pub const INLINE_IMAGE_MEDIA_TYPES: [&str; 4] =
    ["image/png", "image/jpeg", "image/webp", "image/gif"];

pub const MAX_ENTRIES_RANGE_LIMIT: u32 = 200;
/// Windowed transcripts are a web-only attach option; older hosts ignore it.
pub const MIN_ENTRIES_WINDOW: u32 = 1;
pub const MAX_ENTRIES_WINDOW: u32 = 2_000;
