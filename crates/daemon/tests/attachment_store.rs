//! The attachment store: decode + magic validation, content-addressed
//! storage, integrity verification.
//!
//! Sources: legacy `src/image-data.ts` (decode + magic) and
//! `src/attachments.ts` (`storeAttachment`, `verifyAttachmentPath`),
//! read 2026-11-17.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{temp_data_dir, uuid};
use openbutler_daemon::state::{decode_base64_image, store_attachment, verify_attachment_path};

/// Eight PNG magic bytes plus filler — `imageTypeFromBytes` only sniffs.
fn png_bytes(extra: usize) -> Vec<u8> {
    let mut bytes = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.extend(std::iter::repeat_n(b'x', extra));
    bytes
}

fn encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[test]
fn decoding_rejects_empty_invalid_and_oversized_payloads() {
    assert_eq!(
        decode_base64_image("   ", "image/png", 1024).unwrap_err(),
        "An attached image is empty."
    );
    assert_eq!(
        decode_base64_image("!!not base64!!", "image/png", 1024).unwrap_err(),
        "The image data is not valid base64."
    );
    // Over the pre-decode string budget for 1 KiB: ceil(1024*4/3)+4.
    let long = "A".repeat(2_000);
    assert_eq!(
        decode_base64_image(&long, "image/png", 1024).unwrap_err(),
        "The image data exceeded its size limit."
    );
    // Valid base64 that decodes to something else than the declared type.
    let not_png = encode(b"definitely not a png");
    assert_eq!(
        decode_base64_image(&not_png, "image/png", 1024).unwrap_err(),
        "An attached image is not a valid image/png."
    );
    // Whitespace from an encoder is incidental and accepted.
    let spaced = encode(&png_bytes(4)).replace('A', "A\n");
    let bytes = decode_base64_image(&spaced, "image/png", 1024).expect("decodes");
    assert_eq!(bytes.len(), 12);
}

#[test]
fn storing_is_content_addressed_and_idempotent() {
    let dir = temp_data_dir("attach-store");
    let session = uuid(9);
    let bytes = png_bytes(64);

    let first =
        store_attachment(&dir, &session, &bytes, "shot.png", "image/png").expect("first store");
    assert_eq!(first.id, first.sha256);
    assert_eq!(first.size_bytes, bytes.len() as i64);
    assert_eq!(first.media_type, "image/png");
    assert_eq!(first.name, "shot.png");
    assert!(first.path.ends_with(&format!("{}.png", first.sha256)));

    // The same bytes land on the same path and stay verified.
    let again =
        store_attachment(&dir, &session, &bytes, "other.png", "image/png").expect("second store");
    assert_eq!(again.path, first.path, "content addressing is stable");
    verify_attachment_path(&dir, std::path::Path::new(&first.path)).expect("verifies");
}

#[test]
fn storing_refuses_mismatched_declared_types_and_oversized_images() {
    let dir = temp_data_dir("attach-guard");
    let session = uuid(10);

    let jpeg_magic = [0xFF, 0xD8, 0xFF, 0x00];
    assert_eq!(
        store_attachment(&dir, &session, &jpeg_magic, "shot.png", "image/png").unwrap_err(),
        "The attachment image type does not match its data."
    );

    // 5 MB'ın üstünde ama 50 MB'ın altında: image cap devreye girer.
    let too_big = png_bytes(5 * 1024 * 1024);
    assert_eq!(
        store_attachment(&dir, &session, &too_big, "big.png", "image/png").unwrap_err(),
        "Images must be 5 MB or smaller."
    );

    let empty: Vec<u8> = Vec::new();
    assert_eq!(
        store_attachment(
            &dir,
            &session,
            &empty,
            "none.bin",
            "application/octet-stream"
        )
        .unwrap_err(),
        "Attachment must contain between 1 byte and 50 MB."
    );
}

#[test]
fn verification_fails_on_a_tampered_file() {
    let dir = temp_data_dir("attach-verify");
    let session = uuid(11);
    let stored =
        store_attachment(&dir, &session, &png_bytes(32), "shot.png", "image/png").expect("store");

    std::fs::write(&stored.path, b"tampered").expect("overwrite");
    let error = verify_attachment_path(&dir, std::path::Path::new(&stored.path)).unwrap_err();
    assert!(
        error.contains("integrity"),
        "a modified file is refused: {error}"
    );
}

#[test]
fn names_are_sanitised_and_extensionless_files_fall_back() {
    let dir = temp_data_dir("attach-name");
    let session = uuid(12);
    let stored = store_attachment(
        &dir,
        &session,
        b"plain payload".as_ref(),
        "weird\u{0001}name",
        "application/octet-stream",
    )
    .expect("store");
    assert_eq!(stored.media_type, "application/octet-stream");
    assert!(
        !stored.name.contains('\u{0001}'),
        "control characters are replaced: {}",
        stored.name
    );
    assert!(
        stored.path.ends_with(".bin"),
        "no usable extension falls back to .bin: {}",
        stored.path
    );
}
