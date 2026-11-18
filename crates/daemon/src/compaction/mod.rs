//! Context compaction's core: the measured trigger arithmetic, the
//! conservative estimators, the checkpoint prompts, and the conversation
//! projection helpers.
//!
//! Source: legacy `compaction.ts` + `conversation.ts` (read 2026-11-18).
//! This module is the pure half - no model call, no disk, no state, no
//! sidecar. The engine that streams a summarisation request (`compactConversation`)
//! and its session hooks arrive with the sidecar wiring (C2/C3); the
//! refusal-is-data contract they must honour is recorded in
//! `docs/context-compaction.md` when that slice lands.

mod conversation;
mod core;
mod engine;
mod projection;
mod slicing;

pub use conversation::{
    COMPACTION_CONTINUE_TEXT, COMPACTION_SUMMARY_MARKER, TOOL_REFERENCE_MARKER, ToolReference,
    append_transcript_archive_reference, continue_message, parse_tool_reference,
    repair_queued_message_order, retain_latest_tool_previews, retain_latest_tool_previews_changed,
    retire_covered_tool_previews, truncate_head_and_tail,
};
pub use core::{
    COMPACTION_HEADROOM_TOKENS, COMPACTION_PROMPT, COMPACTION_PROMPT_TOKENS,
    COMPACTION_SUMMARY_MARGIN_TOKENS, COMPACTION_SUMMARY_MAX_TOKENS, POST_COMPACTION_SETTLED_RATIO,
    SLICE_MERGE_INSTRUCTION, SLICE_SYSTEM_PROMPT, SLICED_CONDENSE_ROUNDS, SLICED_NOTE_MIN_TOKENS,
    SLICED_NOTES_SHARE, SLICED_TAIL_SHARE, auto_compaction_trigger_tokens, estimate_context_tokens,
    estimate_text_tokens, estimate_tokens,
};
pub use engine::{
    CompactOutcome, CompactionRequest, CompactionResult, EngineHooks, ImpossibleReason, SkipReason,
    compact_conversation,
};
pub use projection::{
    fold_oldest_dialogue, project_transcript, recovery_messages, render_tool_result_output,
    without_executors,
};
pub use slicing::split_into_slices;
