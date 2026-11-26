//! The attachment store: decode + magic validation, content-addressed
//! storage, integrity verification — one authority for
//! `sessions/<id>/attachments/<sha256>.<ext>`.
//!
//! Sources: legacy `src/image-data.ts` and `src/attachments.ts`
//! (`storeAttachment`, `verifyAttachmentPath`), read 2026-11-17. The
//! layout is the on-disk contract.

use std::path::{Path, PathBuf};

use base64::Engine;
use openbutler_protocol::entry::FileAttachment;
use sha2::{Digest, Sha256};

use super::paths::{session_directory, sessions_directory};

/// `MAX_IMAGE_BYTES` — an image payload never exceeds this.
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
/// `MAX_ATTACHMENT_BYTES` — the store's hard ceiling for any payload.
pub const MAX_ATTACHMENT_BYTES: usize = 50 * 1024 * 1024;

/// Magic-byte sniff (`imageTypeFromBytes`): PNG, JPEG, GIF, WEBP.
pub fn image_type_from_bytes(bytes: &[u8]) -> Option<&'static str> {
    const PNG: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    if bytes.len() >= 8 && bytes[..8] == PNG {
        return Some("image/png");
    }
    if bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
        return Some("image/jpeg");
    }
    if bytes.len() >= 6 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// `decodeBase64`: whitespace-tolerant, url-safe aware, validated by
/// character class and length before allocating — and padding-lenient
/// the way `Buffer.from(value, 'base64')` is.
fn decode_base64(value: &str, max_bytes: usize) -> Result<Vec<u8>, String> {
    let cleaned: String = value
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .map(|ch| match ch {
            '-' => '+',
            '_' => '/',
            other => other,
        })
        .collect();
    let body_len = cleaned.trim_end_matches('=').len();
    let padding = cleaned.len() - body_len;
    let body = &cleaned[..body_len];
    let valid = !body.is_empty()
        && padding <= 2
        && cleaned.len() % 4 != 1
        && body
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/');
    if !valid {
        return Err("The image data is not valid base64.".to_owned());
    }
    if cleaned.len() > max_bytes.div_ceil(3) * 4 + 4 {
        return Err("The image data exceeded its size limit.".to_owned());
    }
    // Node decodes unpadded input; the strict engine does not.
    let mut padded = cleaned;
    while !padded.len().is_multiple_of(4) {
        padded.push('=');
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&padded)
        .map_err(|_| "The image data is not valid base64.".to_owned())?;
    if bytes.len() > max_bytes {
        return Err("The image data exceeded its size limit.".to_owned());
    }
    Ok(bytes)
}

/// `decodeBase64Image`: the single authority for an image that arrived
/// as base64 — validated by decoded bytes, never by the string alone.
pub fn decode_base64_image(
    payload: &str,
    media_type: &str,
    max_bytes: usize,
) -> Result<Vec<u8>, String> {
    if payload.trim().is_empty() {
        return Err("An attached image is empty.".to_owned());
    }
    let bytes = decode_base64(payload, max_bytes)?;
    if bytes.is_empty() {
        return Err("An attached image is empty.".to_owned());
    }
    if bytes.len() > max_bytes {
        return Err(format!(
            "An attached image exceeded the {} MB limit.",
            max_bytes / (1024 * 1024)
        ));
    }
    if image_type_from_bytes(&bytes) != Some(media_type) {
        return Err(format!("An attached image is not a valid {media_type}."));
    }
    Ok(bytes)
}

/// `storeAttachment`: immutable, content-addressed files under the
/// session's attachments directory. Re-storing the same bytes is a no-op
/// that still verifies; the temporary file never outlives the call.
pub fn store_attachment(
    data_dir: &Path,
    session_id: &str,
    bytes: &[u8],
    name: &str,
    declared_type: &str,
) -> Result<FileAttachment, String> {
    if bytes.is_empty() || bytes.len() > MAX_ATTACHMENT_BYTES {
        return Err("Attachment must contain between 1 byte and 50 MB.".to_owned());
    }
    let sniffed = image_type_from_bytes(bytes);
    if declared_type.starts_with("image/") && sniffed != Some(declared_type) {
        return Err("The attachment image type does not match its data.".to_owned());
    }
    if sniffed.is_some() && bytes.len() > MAX_IMAGE_BYTES {
        return Err("Images must be 5 MB or smaller.".to_owned());
    }
    let media_type = sniffed.unwrap_or(declared_type);
    let safe_name = sanitize_name(name);
    let extension = extension_for(sniffed, &safe_name);
    let digest = format!("{:x}", Sha256::digest(bytes));

    let root = attachments_directory(data_dir, session_id)?;
    std::fs::create_dir_all(&root).map_err(|error| error.to_string())?;
    let path = root.join(format!("{digest}{extension}"));
    if path.exists() {
        verify_attachment_path(data_dir, &path)?;
        return Ok(attachment_fields(
            &digest, &path, &safe_name, media_type, bytes,
        ));
    }

    let temporary = root.join(format!(".{}.tmp", unique_token()?));
    std::fs::write(&temporary, bytes).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600));
    }
    let result = match std::fs::rename(&temporary, &path) {
        Ok(()) => Ok(()),
        // Lost a race to another writer with the same bytes: the file is
        // content-addressed, so the winner's copy is ours.
        Err(_) if path.exists() => Ok(()),
        Err(error) => Err(error.to_string()),
    };
    let _ = std::fs::remove_file(&temporary);
    result?;
    verify_attachment_path(data_dir, &path)?;
    Ok(attachment_fields(
        &digest, &path, &safe_name, media_type, bytes,
    ))
}

/// `verifyAttachmentPath`: only a regular, contained, hash-verified file
/// may be read as an attachment.
pub fn verify_attachment_path(data_dir: &Path, path: &Path) -> Result<(), String> {
    let sessions = sessions_directory(data_dir);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("Invalid attachment file reference.")?;
    let expected = attachment_digest(file_name).ok_or("Invalid attachment file reference.")?;
    let parent = path.parent().ok_or("Invalid attachment file reference.")?;
    let root = parent.parent().ok_or("Invalid attachment session.")?;
    let session_id = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("Invalid attachment session.")?;
    if super::paths::session_directory(data_dir, session_id)
        .map_err(|_| "Invalid attachment session.".to_owned())?
        != root
    {
        return Err("Invalid attachment session.".to_owned());
    }
    if root != sessions.join(session_id) || parent != root.join("attachments") {
        return Err("Invalid attachment file reference.".to_owned());
    }
    let metadata =
        std::fs::symlink_metadata(path).map_err(|_| "Invalid attachment file reference.")?;
    if !metadata.is_file() || metadata.len() > MAX_ATTACHMENT_BYTES as u64 {
        return Err("The stored attachment failed its integrity check.".to_owned());
    }
    let bytes =
        std::fs::read(path).map_err(|_| "The stored attachment failed its integrity check.")?;
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if actual != expected {
        return Err("The stored attachment failed its integrity check.".to_owned());
    }
    Ok(())
}

/// `basename(name.replace(/\\/g, '/'))` + control chars replaced + 200
/// char slice, with the `attachment` fallback.
fn sanitize_name(name: &str) -> String {
    let normalized = name.replace('\\', "/");
    let base = normalized.rsplit('/').next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .map(|ch| if ch.is_control() { '_' } else { ch })
        .take(200)
        .collect();
    if cleaned.is_empty() {
        "attachment".to_owned()
    } else {
        cleaned
    }
}

/// Image types get their canonical extension; other names keep a plain
/// alphanumeric extension or fall back to `.bin`.
fn extension_for(sniffed: Option<&str>, safe_name: &str) -> String {
    match sniffed {
        Some("image/jpeg") => ".jpg".to_owned(),
        Some(other) => format!(".{}", &other[6..]),
        None => {
            let name = Path::new(safe_name);
            let extension = name
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or("");
            let plain =
                extension.len() <= 10 && extension.bytes().all(|byte| byte.is_ascii_alphanumeric());
            if plain && !extension.is_empty() {
                format!(".{}", extension.to_ascii_lowercase())
            } else {
                ".bin".to_owned()
            }
        }
    }
}

fn attachment_digest(file_name: &str) -> Option<String> {
    let (digest, extension) = file_name.split_once('.')?;
    if digest.len() == 64
        && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        && !extension.is_empty()
        && extension.len() <= 10
        && extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        Some(digest.to_ascii_lowercase())
    } else {
        None
    }
}

fn attachments_directory(data_dir: &Path, session_id: &str) -> Result<PathBuf, String> {
    let root = session_directory(data_dir, session_id)
        .map_err(|_| "Invalid attachment session.".to_owned())?;
    Ok(root.join("attachments"))
}

fn attachment_fields(
    digest: &str,
    path: &Path,
    name: &str,
    media_type: &str,
    bytes: &[u8],
) -> FileAttachment {
    FileAttachment {
        id: digest.to_owned(),
        path: path.display().to_string(),
        name: name.to_owned(),
        media_type: media_type.to_owned(),
        size_bytes: bytes.len() as i64,
        sha256: digest.to_owned(),
    }
}

fn unique_token() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| error.to_string())?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
