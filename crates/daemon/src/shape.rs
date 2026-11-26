//! Shared value-shape utilities: the leaf checks both the settings schema
//! and the schedule store validate against. One home so neither module
//! imports the other for a helper.
//!
//! Origin: the settings parser's helper kit (`settings.ts` equivalents —
//! `jsLen`, id-token regexes, `path.isAbsolute`); moved out when schedules
//! left settings (owner ruling 2026-11-18).

use serde_json::{Map, Value};

/// The record check behind every `typeof x === 'object' && x !== null &&
/// !Array.isArray(x)` in the source.
pub(crate) fn as_record(value: &Value) -> Option<&Map<String, Value>> {
    value.as_object()
}

/// JS `String.length`: UTF-16 code units — the one authority lives in
/// the protocol crate (`envelope::js_length`); this alias keeps the
/// daemon's 17 call sites reading the shared name.
pub(crate) use openbutler_protocol::envelope::js_length as js_len;

/// The compact JSON size the source measures with `JSON.stringify`.
pub(crate) fn json_size(value: &Value) -> Result<usize, String> {
    serde_json::to_string(value)
        .map(|text| text.len())
        .map_err(|error| error.to_string())
}

/// `/^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$/` - schedule ids and adapters.
pub(crate) fn is_id_token(value: &str, max: usize) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > max {
        return false;
    }
    let first = bytes[0];
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'-')
}

/// `path.isAbsolute` on the host platform (the daemon loads its own file).
pub(crate) fn is_absolute_path(value: &str) -> bool {
    #[cfg(windows)]
    {
        let bytes = value.as_bytes();
        (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && (bytes[2] == b'\\' || bytes[2] == b'/'))
            || value.starts_with("\\\\")
    }
    #[cfg(not(windows))]
    {
        value.starts_with('/')
    }
}

/// `Number.toFixed(digits)` for non-negative values — the one rounding rule
/// every rendered number uses (`ui.ts`'s `toFixed` call sites).
pub(crate) fn to_fixed(value: f64, digits: i32) -> String {
    let scale = 10f64.powi(digits);
    format!("{:.*}", digits as usize, (value * scale).round() / scale)
}

/// `ui.bytes` (cli/ui.ts L76): binary units, one decimal above bytes.
pub(crate) fn format_bytes(value: u64) -> String {
    if value == 0 {
        return "0 B".to_owned();
    }
    let units = ["B", "KB", "MB", "GB", "TB"];
    let size = value as f64;
    let exponent = ((size.ln() / 1024f64.ln()).floor() as usize).min(units.len() - 1);
    let scaled = size / 1024f64.powi(exponent as i32);
    let digits = if exponent == 0 { 0 } else { 1 };
    format!("{} {}", to_fixed(scaled, digits), units[exponent])
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn bytes_follow_the_source_formatter() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1), "1 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0 GB");
        // The source's unit table stops at TB; anything above scales into it.
        assert_eq!(format_bytes(u64::MAX), "16777216.0 TB");
    }

    #[test]
    fn to_fixed_rounds_half_away_from_zero() {
        assert_eq!(to_fixed(1.25, 1), "1.3");
        assert_eq!(to_fixed(1.24, 1), "1.2");
        assert_eq!(to_fixed(2.0, 0), "2");
        assert_eq!(to_fixed(0.000_06, 4), "0.0001");
    }
}
