//! Wire-format helpers shared by the image-capable and body-bounding adapters.

pub(crate) fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let bits = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        output.push(char::from(ALPHABET[((bits >> 18) & 63) as usize]));
        output.push(char::from(ALPHABET[((bits >> 12) & 63) as usize]));
        output.push(if chunk.len() > 1 {
            char::from(ALPHABET[((bits >> 6) & 63) as usize])
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            char::from(ALPHABET[(bits & 63) as usize])
        } else {
            '='
        });
    }
    output
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
