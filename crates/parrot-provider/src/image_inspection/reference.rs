//! Common image-reference formatting shared by image-capable wire adapters.
//!
//! Resolving an existing link never touches the image file. Inline references
//! read bytes lazily only when a provider actually needs the data URI.
use crate::wire::encode_base64;

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
        encode_base64(&read_image()?)
    ))
}
