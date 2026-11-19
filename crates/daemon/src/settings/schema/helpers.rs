//! Moved from settings/schema.rs by the split (see dispatch-style refactor notes).
use serde_json::{Map, Value};

use super::SettingsError;
use crate::shape::{as_record, is_id_token};
/// `Array.isArray` + every item a string — the legacy `stringArray`.
pub(super) fn string_array(value: &Value, where_: &str) -> Result<Vec<String>, SettingsError> {
    let Some(items) = value.as_array() else {
        return Err(SettingsError(format!("{where_} must be a list of strings")));
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Some(text) = item.as_str() else {
            return Err(SettingsError(format!("{where_} must be a list of strings")));
        };
        out.push(text.to_owned());
    }
    Ok(out)
}

/// The compact JSON size the source measures with `JSON.stringify`.
pub(super) fn json_size(value: &Value) -> Result<usize, SettingsError> {
    crate::shape::json_size(value).map_err(SettingsError)
}

/// `/^[A-Za-z_][A-Za-z0-9_]*$/` — an environment-variable name.
pub(super) fn is_environment_name(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// `/^[A-Za-z0-9][A-Za-z0-9_-]{0,58}$/` — MCP server ids (59 max).
pub(super) fn is_mcp_id(value: &str) -> bool {
    is_id_token(value, 59)
}

/// `/^[!#$%&'*+.^_\`|~0-9A-Za-z-]+$/` — an HTTP header field name.
pub(super) fn is_header_name(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                        | b'-'
                )
        })
}

/// The opaque-option-bag rule: a mapping of bounded size with no prototype
/// keys, because these reach `import()` arguments and request headers. Array
/// elements keep the source's pass-through (only object keys are walked).
pub(super) fn opaque_bounded(value: &Value, where_: &str) -> Result<Value, SettingsError> {
    let Some(map) = as_record(value) else {
        return Err(SettingsError(format!(
            "{where_} must be a mapping of option names to values"
        )));
    };
    let size = json_size(value)?;
    if size > 16_000 {
        return Err(SettingsError(format!(
            "{where_} must serialize under 16000 characters (got {size})."
        )));
    }
    reject_prototype_keys(map, where_)?;
    Ok(value.clone())
}

pub(super) fn reject_prototype_keys(
    map: &Map<String, Value>,
    path: &str,
) -> Result<(), SettingsError> {
    for (key, item) in map {
        if matches!(key.as_str(), "__proto__" | "prototype" | "constructor") {
            return Err(SettingsError(format!(
                "{path}.{key} is not an allowed option name."
            )));
        }
        if let Some(nested) = as_record(item) {
            reject_prototype_keys(nested, &format!("{path}.{key}"))?;
        }
    }
    Ok(())
}

/// `Number.isInteger` + inclusive range, one message for every failure mode.
pub(super) fn whole_number_in_range(
    value: &Value,
    name: &str,
    min: u64,
    max: u64,
) -> Result<u64, SettingsError> {
    let number = value.as_u64().filter(|number| (min..=max).contains(number));
    number.ok_or_else(|| {
        SettingsError(format!(
            "{name} must be a whole number between {min} and {max}."
        ))
    })
}

pub(super) fn integer_in_range(
    value: &Value,
    name: &str,
    min: u64,
    max: u64,
) -> Result<u64, SettingsError> {
    whole_number_in_range(value, name, min, max)
}

/// The two approval timeouts: whole seconds in 5..=600, with the message
/// wording the contract pins ("of seconds" is part of it).
pub(super) fn timeout_seconds(value: &Value, name: &str) -> Result<u64, SettingsError> {
    let number = value.as_u64().filter(|number| (5..=600).contains(number));
    number.ok_or_else(|| {
        SettingsError(format!(
            "{name} must be a whole number of seconds between 5 and 600."
        ))
    })
}
