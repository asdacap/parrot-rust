//! Common image-reference formatting shared by image-capable wire adapters.
//!
//! Resolving an existing link never touches the image file. Inline references
//! read bytes lazily only when a provider actually needs the data URI.

/// Returns an existing public image reference, or constructs a base64 data URI.
///
/// The closure is not called for nonempty `url`, matching the source adapter's
/// lazy image-file read behavior. Media-type validation belongs to inspection.
pub fn image_reference<E>(
    url: Option<&str>,
    media_type: &str,
    read_image: impl FnOnce() -> Result<Vec<u8>, E>,
) -> Result<String, E> {
    if let Some(url) = url.filter(|url| !url.is_empty()) {
        return Ok(url.to_owned());
    }
    Ok(format!(
        "data:{media_type};base64,{}",
        base64(&read_image()?)
    ))
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = *chunk.get(1).unwrap_or(&0);
        let third = *chunk.get(2).unwrap_or(&0);
        output.push(TABLE[(first >> 2) as usize] as char);
        output.push(TABLE[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        output.push(if chunk.len() > 1 {
            TABLE[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            TABLE[(third & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    output
}
