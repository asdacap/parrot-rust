use super::parser::{PromptAttachmentIntent, parse};
use parrot_domain::{
    contracts::{
        async_ops::{Cancellation, OperationError},
        filesystem::{
            AuthorizedReadService, FileReadError, FileReadErrorKind, LinkPolicy, ReadRequest,
        },
    },
    protocol::messages::parrot as wire,
    values::identifiers::Identifier,
};
use parrot_host::web::router::WebClient;
use parrot_runtime::plan_mode::ModeRegistry;
use parrot_storage::artifacts::ImageInspection;
use std::{fmt, io, io::Write, path::PathBuf, sync::Arc};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio_stream::wrappers::ReceiverStream;

pub const CHUNK_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub enum AttachmentError {
    Cancelled,
    Argument(&'static str),
    Protocol(&'static str),
    Mode(String),
    File(FileReadError),
    Io(io::Error),
    Rpc(tonic::Status),
}
impl fmt::Display for AttachmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("operation cancelled"),
            Self::Argument(message) | Self::Protocol(message) => formatter.write_str(message),
            Self::Mode(message) => formatter.write_str(message),
            Self::File(error) => error.fmt(formatter),
            Self::Io(error) => error.fmt(formatter),
            Self::Rpc(error) => write!(
                formatter,
                "Status(StatusCode=\"{:?}\", Detail=\"{}\")",
                error.code(),
                error.message()
            ),
        }
    }
}
impl std::error::Error for AttachmentError {}
impl From<io::Error> for AttachmentError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl AttachmentError {
    fn caught_by_prepare(&self) -> bool {
        match self {
            Self::Io(_) | Self::Rpc(_) => true,
            Self::File(error) => {
                error.is_io()
                    || matches!(
                        error.kind,
                        FileReadErrorKind::Denied | FileReadErrorKind::InvalidOperation
                    )
            }
            _ => false,
        }
    }
}

/// Send a borrowed source through the real generated client. Producer and RPC
/// are polled together: no detached task retains a reader/request after return.
/// Header, nonempty <=1MiB chunks, terminal description, then end-of-stream.
pub async fn send<R: AsyncRead + Unpin + Send>(
    client: &mut WebClient,
    user_session_id: &str,
    source: &mut R,
    display_name: &str,
    media_type: &str,
    cancellation: &dyn Cancellation,
) -> Result<wire::ArtifactReference, AttachmentError> {
    cancellation
        .check()
        .map_err(|_| AttachmentError::Cancelled)?;
    let upload_id = Identifier::event_id()?;
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    let produce = async {
        let send_frame = |payload| {
            sender.send(wire::AttachmentUploadFrame {
                payload: Some(payload),
                ..Default::default()
            })
        };
        send_frame(wire::attachment_upload_frame::Payload::Header(
            wire::AttachmentUploadHeader {
                user_session_id: user_session_id.into(),
                upload_id,
                ..Default::default()
            },
        ))
        .await
        .map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "attachment upload stream closed")
        })?;
        let mut buffer = vec![0; CHUNK_BYTES];
        let mut byte_length = 0i64;
        loop {
            let read = source.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            byte_length = byte_length.checked_add(read as i64).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "attachment byte length overflow",
                )
            })?;
            send_frame(wire::attachment_upload_frame::Payload::Chunk(
                buffer[..read].to_vec(),
            ))
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::BrokenPipe, "attachment upload stream closed")
            })?;
        }
        send_frame(wire::attachment_upload_frame::Payload::Description(
            wire::AttachmentUploadDescription {
                display_name: display_name.into(),
                media_type: media_type.into(),
                byte_length,
                ..Default::default()
            },
        ))
        .await
        .map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "attachment upload stream closed")
        })?;
        drop(sender);
        Ok::<_, AttachmentError>(())
    };
    let upload = async {
        client
            .upload_attachment(ReceiverStream::new(receiver))
            .await
            .map_err(AttachmentError::Rpc)
    };
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(AttachmentError::Cancelled),
        result = async { tokio::try_join!(produce, upload) } => {
            let (_, response) = result?;
            response.into_inner().artifact.into_option().ok_or(
                AttachmentError::Protocol("attachment upload response has no artifact")
            )
        }
    }
}

/// Uses the existing workspace-bound authorized reader and immutable mode
/// policy. There is deliberately no unrestricted filesystem fallback. If the
/// production authorized-read backend is unavailable, construction must fail
/// closed in the caller; fixtures do not establish physical grants or T36.
pub struct PromptAttachmentUploader<R: AuthorizedReadService + ?Sized>
where
    R::Security: 'static,
{
    reader: Arc<R>,
    modes: Arc<ModeRegistry<R::Security>>,
}
impl<R: AuthorizedReadService + ?Sized> PromptAttachmentUploader<R>
where
    R::Security: 'static,
{
    pub fn new(reader: Arc<R>, modes: Arc<ModeRegistry<R::Security>>) -> Self {
        Self { reader, modes }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn prepare(
        &self,
        client: &mut WebClient,
        user_session_id: &str,
        mode: &str,
        prompt: &str,
        delivery: wire::Delivery,
        error: &mut (dyn Write + Send),
        cancellation: &dyn Cancellation,
    ) -> Result<Option<wire::SendMessageRequest>, AttachmentError> {
        if user_session_id.trim().is_empty() {
            return Err(AttachmentError::Argument(
                "user session id must not be blank",
            ));
        }
        if mode.trim().is_empty() {
            return Err(AttachmentError::Argument("mode must not be blank"));
        }
        let profile = self
            .modes
            .resolve(mode)
            .map_err(|error| AttachmentError::Mode(error.to_string()))?;
        let mut request = wire::SendMessageRequest {
            user_session_id: user_session_id.into(),
            delivery: delivery.into(),
            ..Default::default()
        };
        for intent in parse(prompt) {
            let content = match intent {
                PromptAttachmentIntent::Text(text) => {
                    wire::message_content_part::Content::Text(text)
                }
                PromptAttachmentIntent::Path(path) => {
                    match self
                        .upload(
                            client,
                            user_session_id,
                            &path,
                            profile.security().as_ref(),
                            cancellation,
                        )
                        .await
                    {
                        Ok(artifact) => {
                            wire::message_content_part::Content::ArtifactId(artifact.artifact_id)
                        }
                        Err(failure) if failure.caught_by_prepare() => {
                            cancellation
                                .check()
                                .map_err(|_| AttachmentError::Cancelled)?;
                            writeln!(error, "parrot: cannot attach '{path}': {failure}")?;
                            return Ok(None);
                        }
                        Err(failure) => return Err(failure),
                    }
                }
            };
            request.parts.push(wire::MessageContentPart {
                content: Some(content),
                ..Default::default()
            });
        }
        // Preparing never admits a prompt. Earlier successful image uploads stay
        // stored if a later reference fails, matching the pinned sequential loop.
        Ok(Some(request))
    }

    async fn upload(
        &self,
        client: &mut WebClient,
        session: &str,
        path: &str,
        security: &R::Security,
        cancellation: &dyn Cancellation,
    ) -> Result<wire::ArtifactReference, AttachmentError> {
        cancellation
            .check()
            .map_err(|_| AttachmentError::Cancelled)?;
        let request = ReadRequest {
            requested: PathBuf::from(path),
            expected_physical: None,
            links: LinkPolicy::Follow,
            max_bytes: None,
        };
        let read = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AttachmentError::Cancelled),
            result = self.reader.read(&request, security, cancellation) => result,
        }
        .map_err(|error| match error {
            OperationError::Cancelled => AttachmentError::Cancelled,
            OperationError::Failed(error) => AttachmentError::File(error),
        })?;
        let header = &read.bytes[..read.bytes.len().min(ImageInspection::HEADER_LENGTH)];
        let media_type = ImageInspection::detect_media_type(header).ok_or_else(|| {
            AttachmentError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "Only PNG, JPEG, GIF, and WebP images are supported.",
            ))
        })?;
        let display_name = path
            .rsplit(std::path::MAIN_SEPARATOR)
            .next()
            .unwrap_or_default();
        send(
            client,
            session,
            &mut std::io::Cursor::new(read.bytes),
            display_name,
            media_type,
            cancellation,
        )
        .await
    }
}
