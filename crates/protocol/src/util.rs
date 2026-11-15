//! Serde helpers for wire-fidelity details.

use serde::{Deserialize, Deserializer};

/// Deserializes the TypeScript `field?: T | null` pattern where *absent* and
/// *null* mean different things on the wire.
///
/// - key absent (with `#[serde(default)]`) → `None`
/// - key present with `null` → `Some(None)`
/// - key present with a value → `Some(Some(value))`
///
/// Used where the contract documents distinct meanings — `buildId` is `null`
/// for a source run but *absent* on an older host, so a client that cannot
/// tell them apart cannot tell a source run from an old binary either.
pub fn de_trio_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Ok(Some(Option::<T>::deserialize(deserializer)?))
}

/// `skip_serializing_if` companion for [`de_trio_option`]: only a truly absent
/// key is dropped; an explicit `null` is written back as `null`.
pub fn trio_absent<T>(value: &Option<Option<T>>) -> bool {
    value.is_none()
}
