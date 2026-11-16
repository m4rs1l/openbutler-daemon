//! Payload key rules shared by every dispatched action: the fixed
//! `requireString` / `requireInteger` helpers from `daemon-host.ts` L4210 and
//! L4216 (read 2026-11-16).
//!
//! One authority for the wording and the semantics — missing keys, wrong
//! types, empty strings, byte caps, and JS `Number.isInteger` range checks —
//! so both implementations answer identically. Envelope-level rules live in
//! `envelope`; these are the per-action payload rules.

use serde_json::Value;

use crate::error::ValidationError;

/// `requireString(value, name, maxBytes)`: the value must be a non-empty
/// string of at most `max_bytes` UTF-8 bytes (`Buffer.byteLength(value,
/// 'utf8')` — a byte cap, deliberately not the UTF-16 length the envelope
/// uses for `id` and `action`).
pub fn require_string(
    value: Option<&Value>,
    name: &'static str,
    max_bytes: usize,
) -> Result<String, ValidationError> {
    match value {
        Some(Value::String(text)) if !text.is_empty() && text.len() <= max_bytes => {
            Ok(text.clone())
        }
        _ => Err(ValidationError::PayloadString { name, max_bytes }),
    }
}

/// `requireInteger(value, name, min, max)`: JS `Number.isInteger` followed by
/// an inclusive range — one message covers non-numbers, fractions, and
/// out-of-range values, exactly like the TypeScript template. The bounds fit
/// f64 exactly (the default cap is 2^53-1), so the comparison is honest.
pub fn require_integer(
    value: Option<&Value>,
    name: &'static str,
    min: u64,
    max: u64,
) -> Result<u64, ValidationError> {
    let number = value
        .and_then(Value::as_f64)
        .filter(|number| number.is_finite() && number.fract() == 0.0)
        .ok_or(ValidationError::PayloadInteger { name, min, max })?;
    if number < min as f64 || number > max as f64 {
        return Err(ValidationError::PayloadInteger { name, min, max });
    }
    Ok(number as u64)
}
