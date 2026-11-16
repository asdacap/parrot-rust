//! Wire-format helpers shared by the image-capable and body-bounding adapters.

use base64::Engine as _;

pub(crate) fn encode_base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Bounds an error body to `MAX_ERROR_BYTES` without splitting a UTF-8 character.
pub(crate) fn bound_body(body: &str) -> String {
    const MARKER: &str = "\n… [truncated]";
    let limit = crate::http_stream::MAX_ERROR_BYTES;
    if body.len() <= limit {
        return body.to_owned();
    }
    let mut end = limit - MARKER.len();
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{MARKER}", &body[..end])
}
