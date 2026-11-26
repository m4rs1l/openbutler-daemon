//! The `session.send` message contract: `parseQueuedMessage` verbatim
//! (daemon-host.ts L4104). The shape gates run here and so does the
//! byte-level image check the source performs inside the parser: every
//! image is decoded against its declared type and size before the send is
//! accepted, and the payload is re-encoded canonically, so a message that
//! cannot carry its picture is refused instead of accepted and noticed
//! later. Storing the bytes is the executor's pre-turn conversion.

use base64::Engine;
use serde_json::Value;

use crate::agent::QueuedImage;
use crate::state::{MAX_IMAGE_BYTES, decode_base64_image};

/// `MAX_MESSAGE_TEXT_BYTES` — UTF-8 bytes, like `Buffer.byteLength`.
pub const MAX_MESSAGE_TEXT_BYTES: usize = 8 * 1024 * 1024;
/// The per-message image cap (source: 8).
pub const MAX_IMAGES_PER_MESSAGE: usize = 8;

const TEXT_ERROR: &str = "message must contain text smaller than 8 MB.";
const IMAGES_ERROR: &str = "message may contain up to 8 images.";
const PNG_ERROR: &str = "Only base64 PNG images are supported over this IPC version.";

/// A validated message. The images ride along shape-checked; decoding
/// and storing them is the executor's pre-turn conversion (the legacy
/// `enqueueMessage` step), so a storage failure lands as a notice after
/// acceptance, exactly like the source.
pub struct ParsedMessage {
    pub text: String,
    pub images: Vec<QueuedImage>,
}

/// Validates the `message` payload with the source's wording and order:
/// record + text + byte cap first, then the image list, then each image's
/// shape. Shape errors keep the legacy sentences; a list that passes the
/// shape checks is refused explicitly rather than dropped.
pub fn parse_queued_message(value: &Value) -> Result<ParsedMessage, String> {
    let record = value.as_object().ok_or_else(|| TEXT_ERROR.to_owned())?;
    let text = record
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| TEXT_ERROR.to_owned())?;
    if text.len() > MAX_MESSAGE_TEXT_BYTES {
        return Err(TEXT_ERROR.to_owned());
    }
    let images = record
        .get("images")
        .and_then(Value::as_array)
        .ok_or_else(|| IMAGES_ERROR.to_owned())?;
    if images.len() > MAX_IMAGES_PER_MESSAGE {
        return Err(IMAGES_ERROR.to_owned());
    }
    let mut parsed_images = Vec::with_capacity(images.len());
    for image in images {
        let fields = image.as_object().ok_or_else(|| PNG_ERROR.to_owned())?;
        let media_type = fields.get("mediaType").and_then(Value::as_str);
        let base64 = fields.get("base64").and_then(Value::as_str);
        let (Some("image/png"), Some(base64)) = (media_type, base64) else {
            return Err(PNG_ERROR.to_owned());
        };
        // The source validates the decoded bytes here (`decodeBase64Image`)
        // and passes on the canonical encoding: the string's shape says
        // nothing about whether it is a PNG.
        let bytes = decode_base64_image(base64, "image/png", MAX_IMAGE_BYTES)?;
        parsed_images.push(QueuedImage {
            base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
            media_type: "image/png".to_owned(),
        });
    }
    Ok(ParsedMessage {
        text: text.to_owned(),
        images: parsed_images,
    })
}
