//! Executable attachment admission when the authorized-read supplier is absent.

use std::io::Write;

use parrot_domain::{contracts::async_ops::Cancellation, protocol::messages::parrot as wire};

use crate::attachments::parser::{PromptAttachmentIntent, parse};

use super::DispatchError;

/// Preserve ordinary text and the existing attachment parser's escapes, but do
/// not admit any prompt containing a file reference without its required reader.
/// This is an explicit unavailable capability, not an authorization backend.
pub fn prepare_without_read_backend(
    session: &str,
    prompt: &str,
    delivery: wire::Delivery,
    error: &mut (dyn Write + Send),
    cancellation: &dyn Cancellation,
) -> Result<Option<wire::SendMessageRequest>, DispatchError> {
    cancellation.check().map_err(|_| DispatchError::Cancelled)?;
    let mut request = wire::SendMessageRequest {
        user_session_id: session.into(),
        delivery: delivery.into(),
        ..Default::default()
    };
    for intent in parse(prompt) {
        match intent {
            PromptAttachmentIntent::Text(text) => request.parts.push(wire::MessageContentPart {
                content: Some(wire::message_content_part::Content::Text(text)),
                ..Default::default()
            }),
            PromptAttachmentIntent::Path(path) => {
                writeln!(
                    error,
                    "parrot: cannot attach '{path}': authorized attachment reads are unavailable"
                )?;
                return Ok(None);
            }
        }
    }
    Ok(Some(request))
}
