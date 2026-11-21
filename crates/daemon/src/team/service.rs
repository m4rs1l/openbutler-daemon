//! Service helpers — id minting, name rules, fingerprints.
//!
//! Source: legacy `team/service.ts` (116 lines, read 2026-11-22). The id
//! shapes and every refusal sentence are the source's; entropy comes from
//! `getrandom` instead of `crypto.randomBytes`.

use sha2::{Digest, Sha256};

use super::operations::TeamError;

/// The reserved recipient name for the shared group channel.
pub const GROUP_RECIPIENT: &str = "group";

/// `randomInt(1000, 10000)` in modulo form: the code is display-only and a
/// uniform-enough four digits matters more than rejection sampling.
fn random_below(bound: u32) -> Result<u32, TeamError> {
    let mut buffer = [0u8; 4];
    getrandom::fill(&mut buffer)
        .map_err(|error| TeamError::new(format!("Random source unavailable: {error}"), 500))?;
    Ok(u32::from_le_bytes(buffer) % bound)
}

fn random_hex(bytes: usize) -> Result<String, TeamError> {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer)
        .map_err(|error| TeamError::new(format!("Random source unavailable: {error}"), 500))?;
    Ok(buffer.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// `^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$` by hand (the same ASCII classes, no
/// regex machinery), then the reserved set.
fn is_name_shaped(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 || !bytes[0].is_ascii_alphanumeric() {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-' || *byte == b'_')
}

/// Member names are shown to humans and used as file names: 1-64 letters,
/// digits, dashes, or underscores — and never the reserved group channel.
/// Returns the trimmed name.
pub fn validate_member_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if !is_name_shaped(name) {
        return Err(format!(
            "\"{raw}\" is not a valid member name. Use 1-64 letters, digits, dashes, or underscores."
        ));
    }
    if name.eq_ignore_ascii_case(GROUP_RECIPIENT) {
        return Err(format!(
            "\"{name}\" is reserved and cannot be used as a member name."
        ));
    }
    Ok(name.to_owned())
}

/// Team names follow the same rule as member names (no reserved set).
/// Returns the trimmed name.
pub fn validate_team_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if !is_name_shaped(name) {
        return Err(format!(
            "\"{raw}\" is not a valid team name. Use 1-64 letters, digits, dashes, or underscores."
        ));
    }
    Ok(name.to_owned())
}

/// The invite word list (`INVITE_WORDS`), in order.
const INVITE_WORDS: [&str; 16] = [
    "kartal", "kurt", "sahin", "pars", "ejder", "kaplan", "yildiz", "deniz", "dag", "ruzgar",
    "ates", "bulut", "orman", "nehir", "cakal", "dogan",
];

/// Short human-readable invite code: `kartal-4821`. Single use, short TTL
/// (hub enforces).
pub fn make_invite_code() -> Result<String, TeamError> {
    let word = INVITE_WORDS[random_below(16)? as usize];
    Ok(format!("{word}-{}", 1000 + random_below(9_000)?))
}

/// Bearer ticket the hub issues on approval. Stored hashed hub-side, raw
/// client-side.
pub fn make_ticket() -> Result<String, TeamError> {
    Ok(format!("rt_{}", random_hex(24)?))
}

/// sha256 of the ticket, lowercase hex.
pub fn hash_ticket(ticket: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(ticket.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Stable hub identity shown to humans at pairing time. The hub id never
/// leaves the hub (`fingerprintOf`).
pub fn fingerprint_of(hub_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("openbutler-team-hub:{hub_id}").as_bytes());
    let hex = format!("{:x}", hasher.finalize());
    format!("{}:{}:{}", &hex[0..4], &hex[4..8], &hex[8..12]).to_uppercase()
}

/// Machine hub id: 16 random bytes as hex.
pub fn make_hub_id() -> Result<String, TeamError> {
    random_hex(16)
}

/// The team's stable public id. It keys shared memory and scopes local state.
pub fn make_team_id() -> Result<String, TeamError> {
    Ok(format!("t_{}", random_hex(12)?))
}

/// Unguessable join-request id. Knowing it is the only credential for
/// polling that request.
pub fn make_request_id() -> Result<String, TeamError> {
    Ok(format!("jr_{}", random_hex(12)?))
}

/// Message id. Unique per team, so a re-sent message is recognised and not
/// duplicated.
pub fn make_message_id() -> Result<String, TeamError> {
    Ok(format!("m_{}", random_hex(12)?))
}

/// File id. Keys the stored bytes on the hub and the metadata row.
pub fn make_file_id() -> Result<String, TeamError> {
    Ok(format!("f_{}", random_hex(12)?))
}

/// Whether this trimmed value names the group channel.
pub fn is_group_recipient(value: &str) -> bool {
    value.trim().eq_ignore_ascii_case(GROUP_RECIPIENT)
}

/// Whether a member-side poll should hand a wake to the session: a burst
/// wakes once because the highest seen sequence advances the cursor; group
/// messages never reach here (count 0).
pub fn should_wake_for_team_dms(count: i64, highest_seq: i64, seen_seq: i64) -> bool {
    if count <= 0 {
        return false;
    }
    highest_seq > seen_seq
}
