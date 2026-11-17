//! The `session.send` message contract: `parseQueuedMessage` verbatim
//! (daemon-host.ts L4104), with one loud deviation — well-formed images
//! are refused instead of being stored as attachments, because that port
//! (storeAttachment + references) has not landed and silent loss is not
//! an option.

use serde_json::Value;

use crate::agent::QueuedImage;

/// `MAX_MESSAGE_TEXT_BYTES` — UTF-8 bytes, like `Buffer.byteLength`.
pub const MAX_MESSAGE_TEXT_BYTES: usize = 8 * 1024 * 1024;
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
        if fields.get("mediaType").and_then(Value::as_str) != Some("image/png")
            || !fields.get("base64").is_some_and(Value::is_string)
        {
            return Err(PNG_ERROR.to_owned());
        }
        parsed_images.push(QueuedImage {
            base64: fields["base64"].as_str().unwrap_or_default().to_owned(),
            media_type: "image/png".to_owned(),
        });
    }
    Ok(ParsedMessage {
        text: text.to_owned(),
        images: parsed_images,
    })
}
