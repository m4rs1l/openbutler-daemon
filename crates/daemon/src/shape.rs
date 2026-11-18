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
