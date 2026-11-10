//! Provider image token/reference helpers shared by image-capable wire adapters.
//!
//! Structural inspection lives in [`parrot_media`]; callers remain responsible
//! for enforcing their own bounded file/transport read limits. Generation upload
//! limits and provider wire serializers live in their owning provider modules.

mod reference;
mod token;

pub use parrot_media::ImageInspection;
pub use reference::image_reference;
pub use token::{FALLBACK_IMAGE_TOKENS, calculate_content_tokens, calculate_image_tokens};
