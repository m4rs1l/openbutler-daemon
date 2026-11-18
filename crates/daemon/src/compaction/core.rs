//! Compaction's arithmetic: the measured reserve constants, the trigger
//! rule, the checkpoint prompt, and the conservative token estimators.
//!
//! Source: legacy `compaction.ts` L19-183 (constants, prompts, trigger)
//! and L636-683 (estimators), read 2026-11-18. The prompt files are
//! mechanical extractions of the TypeScript template literals; the rule
//! here is arithmetic the tests pin exactly, never a provider fixture's
//! side effect.

use serde_json::Value;

/// Components of the automatic trigger reserve.
///
/// Invariant: at the trigger, one more control step (measured growth) plus
/// the compaction request itself must still fit the window:
/// `window >= trigger + growth + instruction + summary + margin`.
/// Every term is measured or derived in the source repo; none is
/// model-specific:
/// - growth: step growth p99 = 11_583 tokens (2026-11-14, `state.db`,
///   20_296 consecutive positive chat input deltas).
/// - instruction: `estimate_text_tokens(COMPACTION_PROMPT)`, derived from
///   the prompt itself and passed per call, never hardcoded here.
/// - summary: 26_024 = the observed maximum of 144 `compaction` output
///   rows (p50 8_789, p90 14_955, p95 17_298). A percentile would allow
///   cutting a larger summary, and a summary is never cut.
/// - margin: 1_024, the existing summary-slack guard.
///
/// The model's declared output limit never enters this reserve.
pub const COMPACTION_HEADROOM_TOKENS: i64 = 11_583;
pub const COMPACTION_SUMMARY_MAX_TOKENS: i64 = 26_024;
pub const COMPACTION_SUMMARY_MARGIN_TOKENS: i64 = 1_024;

/// Shares of the window for the sliced fallback (map-reduce compaction),
/// for the engine that arrives with the sidecar wiring (C2).
pub const SLICED_NOTES_SHARE: f64 = 0.3;
pub const SLICED_TAIL_SHARE: f64 = 0.15;
pub const SLICED_NOTE_MIN_TOKENS: i64 = 512;
pub const SLICED_CONDENSE_ROUNDS: u32 = 2;

/// Settled-context share of the window after a successful compaction: the
/// trigger decides *when* to compact, this decides *where to land*. A
/// share of the window, never an absolute token count, so small and large
/// models scale the same way with no per-model branches.
pub const POST_COMPACTION_SETTLED_RATIO: f64 = 0.3;

/// The checkpoint instruction, byte-exact from `COMPACTION_PROMPT`
/// (mechanical extraction - see `tests/compaction_core.rs` for the pins).
pub const COMPACTION_PROMPT: &str = include_str!("prompts/compaction-prompt.txt");
/// Slice condensation instruction (mechanical extraction).
pub const SLICE_SYSTEM_PROMPT: &str = include_str!("prompts/slice-system.txt");
/// Slice merge instruction (mechanical extraction).
pub const SLICE_MERGE_INSTRUCTION: &str = include_str!("prompts/slice-merge.txt");

/// Conservative token estimate for one text: ASCII averages about four
/// characters per token, but CJK, emoji and other non-ASCII code points
/// often cost a full token each, so every one counts as a token - keeping
/// the error on the safe side of the capacity guards. Const so the prompt
/// token count below is derived at compile time from the prompt itself.
/// The count walks UTF-8 lead bytes (`>= 0xC0`), which is exactly one
/// count per code point - the same set `for..of` walks in the source.
pub const fn estimate_text_tokens(text: &str) -> i64 {
    let bytes = text.as_bytes();
    let mut index = 0;
    let mut ascii = 0_i64;
    let mut wide = 0_i64;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte < 0x80 {
            ascii += 1;
        } else if byte >= 0xC0 {
            wide += 1;
        }
        index += 1;
    }
    // `ceil(ascii / 4)` in integer arithmetic; zero stays zero.
    (ascii + 3) / 4 + wide
}

/// The prompt's own token cost, derived from the extracted bytes - the
/// input the trigger's reserve starts from.
pub const COMPACTION_PROMPT_TOKENS: i64 = estimate_text_tokens(COMPACTION_PROMPT);

/// The input-token count at which the automatic trigger fires:
/// `max(window - reserve, floor(window / 2))`, never a share of the
/// window and never derived from any model-declared value. The reserve is
/// what the compaction request itself needs when it fires; the 0.5 floor
/// is a guardrail for tiny windows only (it binds below 80_420 tokens
/// with the real prompt). There is no upper clamp: the reserve is
/// strictly positive, so the result always stays below the window.
pub fn auto_compaction_trigger_tokens(context_window_tokens: i64, prompt_tokens: i64) -> i64 {
    let reserve = prompt_tokens
        + COMPACTION_SUMMARY_MAX_TOKENS
        + COMPACTION_SUMMARY_MARGIN_TOKENS
        + COMPACTION_HEADROOM_TOKENS;
    let floor = context_window_tokens / 2;
    std::cmp::max(context_window_tokens - reserve, floor)
}

/// An estimate for unmeasured growth, never a substitute for provider
/// input usage: system text + message tree + tool schemas walked with the
/// conservative rules below.
pub fn estimate_context_tokens(system: &str, messages: &[Value], tools: &Value) -> i64 {
    estimate_text_tokens(system)
        + estimate_tokens(&Value::Array(messages.to_vec()))
        + estimate_tokens(tools)
}

/// `estimateTokens` over an arbitrary JSON value: strings by the text
/// rule, arrays as sums, scalars as one, media parts (`image`,
/// `image-data`, `image-url`, `file`) as a flat 1_024, every other object
/// as 4 plus its values (per nesting level). Binary buffers and URL
/// objects of the source are unreachable in JSON, so they are handled by
/// the same media-part rule where they are declared.
pub fn estimate_tokens(value: &Value) -> i64 {
    match value {
        Value::String(text) => estimate_text_tokens(text),
        Value::Array(items) => items.iter().map(estimate_tokens).sum(),
        Value::Null | Value::Bool(_) | Value::Number(_) => 1,
        Value::Object(object) => {
            let media = object
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| matches!(kind, "image" | "image-data" | "image-url" | "file"));
            if media {
                return 1_024;
            }
            4 + object.values().map(estimate_tokens).sum::<i64>()
        }
    }
}
