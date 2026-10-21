//! Structural image metadata and provider token/reference helpers.
//!
//! Inspection reads only caller-provided bytes; callers remain responsible for
//! enforcing their own bounded file/transport read limits. Generation upload
//! limits and provider wire serializers live in their owning provider modules.

mod inspect;
mod reference;
mod token;

pub use inspect::{
    HEADER_LENGTH, ImageInspection, ImageInspectionError, detect_media_type, inspect,
};
pub use reference::image_reference;
pub use token::{FALLBACK_IMAGE_TOKENS, calculate_content_tokens, calculate_image_tokens};
