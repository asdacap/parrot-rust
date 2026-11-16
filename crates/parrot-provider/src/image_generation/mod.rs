//! Image generation and content-addressed image upload/linking.

pub mod bucket;
pub mod client;
pub mod linking;
pub mod signer;
pub(crate) mod uri;

pub use client::{
    ImageGenerationClient, ImageGenerationReference, ImageGenerationRequest, ImageGenerationResult,
    ImageHeaders,
};

/// Preserves the image paths' error surface over the shared HTTP transport:
/// cancellation and disposal keep their dedicated session variants.
pub(crate) fn transport_error(error: crate::http_stream::HttpError) -> crate::sessions::Error {
    use crate::http_stream::HttpError;
    use crate::sessions::Error;
    match error {
        HttpError::Cancelled => Error::Cancelled,
        HttpError::Disposed => Error::Disposed,
        error => Error::Http(error),
    }
}
