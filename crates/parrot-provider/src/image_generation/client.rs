use super::transport::{ImageHttpRequest, ImageTransport};
use crate::{http_stream::Endpoint, image_inspection, sessions::Error};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use std::{fmt::Write, sync::Arc, time::Duration};

pub const MAXIMUM_REFERENCES: usize = 5;
pub const MAXIMUM_REFERENCE_BYTES: usize = 10 << 20;
pub const MAXIMUM_REFERENCE_BATCH_BYTES: usize = 50 << 20;
pub const MAXIMUM_REQUEST_BYTES: usize = 80 << 20;
pub const MAXIMUM_RESPONSE_BYTES: usize = 64 << 20;
pub const MAXIMUM_OUTPUT_BYTES: usize = 32 << 20;
pub const OPERATION_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageGenerationReference {
    pub media_type: String,
    pub data: Vec<u8>,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ImageGenerationRequest {
    pub prompt: String,
    pub references: Vec<ImageGenerationReference>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageGenerationResult {
    pub data: Vec<u8>,
}

/// Resolved for each image operation, after request validation. Providers retain
/// their existing credential and header policy; image generation does not retry.
pub trait ImageHeaders: Send + Sync {
    fn resolve<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<(String, String)>, Error>>;
}

pub struct ImageGenerationClient {
    transport: Arc<dyn ImageTransport>,
    generation_endpoint: Endpoint,
    edit_endpoint: Endpoint,
    headers: Arc<dyn ImageHeaders>,
}
impl ImageGenerationClient {
    pub fn new(
        transport: Arc<dyn ImageTransport>,
        generation_endpoint: Endpoint,
        edit_endpoint: Endpoint,
        headers: Arc<dyn ImageHeaders>,
    ) -> Self {
        Self {
            transport,
            generation_endpoint,
            edit_endpoint,
            headers,
        }
    }
    pub fn generate<'a>(
        &'a self,
        request: ImageGenerationRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ImageGenerationResult, Error>> {
        Box::pin(async move {
            let token = Deadline {
                caller: cancellation,
                until: tokio::time::Instant::now() + OPERATION_TIMEOUT,
            };
            if token.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let body = prepare(&request)?;
            let headers = self.headers.resolve(&token).await?;
            let endpoint = if request.references.is_empty() {
                &self.generation_endpoint
            } else {
                &self.edit_endpoint
            };
            let response = self
                .transport
                .send(
                    ImageHttpRequest {
                        method: "POST".into(),
                        endpoint: super::uri::normalize_endpoint(endpoint)?,
                        headers,
                        content_type: Some("application/json".into()),
                        body,
                    },
                    &token,
                )
                .await?;
            if !(200..300).contains(&response.status) {
                return Err(provider(format!(
                    "provider: image generation failed (HTTP {})",
                    response.status
                )));
            }
            if response.headers.iter().any(|(name, value)| {
                name.eq_ignore_ascii_case("content-length")
                    && value
                        .parse::<u64>()
                        .is_ok_and(|n| n > MAXIMUM_RESPONSE_BYTES as u64)
            }) {
                return Err(provider("provider: image response exceeds byte limit"));
            }
            use crate::http_stream::{BoundedStream, ResponseByteCounter};
            let stream = BoundedStream::new(
                response.body,
                MAXIMUM_RESPONSE_BYTES as i64,
                ResponseByteCounter::default(),
            );
            let parsed = read_response(stream, &token).await?;
            finish_response(parsed)
        })
    }
}

fn provider(message: impl Into<String>) -> Error {
    Error::Provider(message.into())
}

pub fn validate_image(data: &[u8], require_png: bool) -> Result<&'static str, Error> {
    let media_type = match image_inspection::detect_media_type(data) {
        Some("image/png") => "image/png",
        Some("image/jpeg") if !require_png => "image/jpeg",
        Some("image/webp") if !require_png => "image/webp",
        _ => return Err(provider("provider: unsupported image format")),
    };
    let image =
        image_inspection::inspect(data).map_err(|_| provider("provider: invalid image content"))?;
    if image.width > 8192
        || image.height > 8192
        || i64::from(image.width) * i64::from(image.height) > 40_000_000
        || image.frame_count > 1
    {
        return Err(provider(
            "provider: image dimensions or frames exceed supported limits",
        ));
    }
    Ok(media_type)
}

pub fn prepare(request: &ImageGenerationRequest) -> Result<Vec<u8>, Error> {
    if request.prompt.trim().is_empty() || request.references.len() > MAXIMUM_REFERENCES {
        return Err(provider(
            "provider: image generation requires a prompt and at most five references",
        ));
    }
    if request.prompt.encode_utf16().count() > MAXIMUM_REQUEST_BYTES {
        return Err(provider("provider: image request exceeds byte limit"));
    }
    let mut references = Vec::new();
    let mut total = 0;
    for reference in &request.references {
        total += reference.data.len();
        if reference.data.len() > MAXIMUM_REFERENCE_BYTES || total > MAXIMUM_REFERENCE_BATCH_BYTES {
            return Err(provider("provider: image reference exceeds byte limit"));
        }
        let media_type = validate_image(&reference.data, false)?;
        if media_type != reference.media_type {
            return Err(provider(
                "provider: image reference media type does not match its content",
            ));
        }
        references.push(image_inspection::image_reference::<Error>(
            None,
            media_type,
            || Ok(reference.data.clone()),
        )?);
    }
    let mut body = format!("{{\"prompt\":{}", quote(&request.prompt));
    if !references.is_empty() {
        body.push_str(",\"images\":[");
        for (index, reference) in references.iter().enumerate() {
            if index != 0 {
                body.push(',');
            }
            write!(body, "{{\"image_url\":{}}}", quote(reference)).unwrap();
        }
        body.push(']');
    }
    body.push_str(",\"model\":\"gpt-image-2\",\"background\":\"auto\",\"quality\":\"auto\",\"size\":\"auto\"}");
    if body.len() > MAXIMUM_REQUEST_BYTES {
        return Err(provider("provider: image request exceeds byte limit"));
    }
    Ok(body.into_bytes())
}

pub fn decode_response(bytes: &[u8]) -> Result<ImageGenerationResult, Error> {
    let parsed = serde_json::from_slice(bytes)
        .map_err(|_| provider("provider: invalid image response JSON"))?;
    finish_response(parsed)
}

fn finish_response(response: Option<WireResponse>) -> Result<ImageGenerationResult, Error> {
    let mut encoded_images: Vec<Option<String>> = response
        .and_then(|response| response.data)
        .unwrap_or_default()
        .into_iter()
        .map(|image| image.and_then(|image| image.encoded))
        .collect();
    let encoded = if encoded_images.len() == 1 {
        encoded_images
            .pop()
            .flatten()
            .filter(|value| !value.is_empty())
    } else {
        None
    }
    .ok_or_else(|| provider("provider: image response must contain exactly one base64 image"))?;
    if encoded.encode_utf16().count() > MAXIMUM_OUTPUT_BYTES.div_ceil(3) * 4 {
        return Err(provider("provider: generated image exceeds byte limit"));
    }
    let data = decode_base64(&encoded)
        .ok_or_else(|| provider("provider: invalid image response base64"))?;
    if data.len() > MAXIMUM_OUTPUT_BYTES {
        return Err(provider("provider: generated image exceeds byte limit"));
    }
    validate_image(&data, true)?;
    Ok(ImageGenerationResult { data })
}

struct WireResponse {
    data: Option<Vec<Option<WireImage>>>,
}
struct WireImage {
    encoded: Option<String>,
}

impl<'de> serde::Deserialize<'de> for WireResponse {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = WireResponse;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an image response object")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                let mut data = None;
                while let Some(name) = map.next_key::<String>()? {
                    if name == "data" {
                        data = map.next_value()?;
                    } else {
                        map.next_value::<serde::de::IgnoredAny>()?;
                    }
                }
                Ok(WireResponse { data })
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}
impl<'de> serde::Deserialize<'de> for WireImage {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = WireImage;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an image object")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                let mut encoded = None;
                while let Some(name) = map.next_key::<String>()? {
                    if name == "b64_json" {
                        encoded = map.next_value()?;
                    } else {
                        map.next_value::<serde::de::IgnoredAny>()?;
                    }
                }
                Ok(WireImage { encoded })
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

// Serde's typed reader validates members as bytes arrive. The demand bridge
// keeps asynchronous transport ownership here: parsing never polls or prefetches
// the response, and an already-determinable JSON failure stops further reads.
async fn read_response(
    mut stream: impl crate::http_stream::ByteStream,
    cancellation: &dyn Cancellation,
) -> Result<Option<WireResponse>, Error> {
    let (request, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let (chunks, receiver) = std::sync::mpsc::sync_channel(1);
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let parser_started = started.clone();
    let mut parser = tokio::task::spawn_blocking(move || {
        parser_started.store(true, std::sync::atomic::Ordering::Release);
        serde_json::from_reader::<_, Option<WireResponse>>(JsonBodyReader {
            request,
            receiver,
            current: std::io::Cursor::new(Vec::new()),
            ended: false,
        })
    });
    let _abort_parser = AbortParser(parser.abort_handle());
    let mut buffer = [0; 16384];
    let parsed = loop {
        tokio::select! {
            biased;
            result = &mut parser => break result,
            _ = cancellation.cancelled() => {
                drop(chunks);
                parser.abort();
                if started.load(std::sync::atomic::Ordering::Acquire) { let _ = parser.await; }
                return Err(Error::Cancelled);
            }
            request = requests.recv() => {
                if request.is_none() { break parser.await; }
                // DeserializeAsync fills its buffer (or sees EOF) before parsing.
                // A short transport chunk is not itself a parsing boundary.
                let fill = async {
                    let mut count = 0;
                    while count < buffer.len() {
                        let read = stream.read(&mut buffer[count..], cancellation).await?;
                        if read == 0 { return Ok((count, true)); }
                        count += read;
                    }
                    Ok::<_, crate::http_stream::HttpError>((count, false))
                };
                let filled = tokio::select! {
                    biased;
                    result = fill => result,
                    _ = cancellation.cancelled() => Err(crate::http_stream::HttpError::Cancelled),
                };
                match filled {
                    Ok((count, ended)) => {
                        if chunks.send(BodyChunk { bytes: buffer[..count].to_vec(), ended }).is_err() { break parser.await; }
                    }
                    Err(error) => {
                        drop(chunks);
                        parser.abort();
                        if started.load(std::sync::atomic::Ordering::Acquire) { let _ = parser.await; }
                        return Err(match error { crate::http_stream::HttpError::Wire(message) => Error::Wire(message), other => Error::Http(other) });
                    }
                }
            }
        }
    };
    parsed
        .map_err(|error| Error::Io(format!("image response reader failed: {error}")))?
        .map_err(|_| provider("provider: invalid image response JSON"))
}
struct AbortParser(tokio::task::AbortHandle);
impl Drop for AbortParser {
    fn drop(&mut self) {
        self.0.abort();
    }
}
struct BodyChunk {
    bytes: Vec<u8>,
    ended: bool,
}
struct JsonBodyReader {
    request: tokio::sync::mpsc::UnboundedSender<()>,
    receiver: std::sync::mpsc::Receiver<BodyChunk>,
    current: std::io::Cursor<Vec<u8>>,
    ended: bool,
}
impl std::io::Read for JsonBodyReader {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.current.position() as usize == self.current.get_ref().len() && !self.ended {
            self.request.send(()).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "image response read stopped",
                )
            })?;
            let chunk = self.receiver.recv().map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "image response read stopped",
                )
            })?;
            self.ended = chunk.ended;
            self.current = std::io::Cursor::new(chunk.bytes);
        }
        std::io::Read::read(&mut self.current, output)
    }
}

fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let bytes: Vec<u8> = text
        .bytes()
        .filter(|byte| !matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        .collect();
    if bytes.len() % 4 != 0 {
        return None;
    }
    let mut result = Vec::with_capacity(bytes.len() / 4 * 3);
    for (index, chunk) in bytes.chunks_exact(4).enumerate() {
        let digit = |byte| match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        };
        let a = digit(chunk[0])?;
        let b = digit(chunk[1])?;
        let final_chunk = index + 1 == bytes.len() / 4;
        result.push((a << 2) | (b >> 4));
        if chunk[2] == b'=' {
            if !final_chunk || chunk[3] != b'=' {
                return None;
            }
        } else {
            let c = digit(chunk[2])?;
            result.push((b << 4) | (c >> 2));
            if chunk[3] == b'=' {
                if !final_chunk {
                    return None;
                }
            } else {
                result.push((c << 6) | digit(chunk[3])?);
            }
        }
    }
    Some(result)
}

// System.Text.Json's default encoder used by ImageGenerationJsonContext.
fn quote(value: &str) -> String {
    let mut result = String::from("\"");
    for character in value.chars() {
        match character {
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            '\u{8}' => result.push_str("\\b"),
            '\u{c}' => result.push_str("\\f"),
            ' '..='~' if !matches!(character, '"' | '\'' | '<' | '>' | '&' | '+' | '`') => {
                result.push(character)
            }
            _ => {
                for unit in character.encode_utf16(&mut [0; 2]) {
                    write!(result, "\\u{unit:04X}").unwrap();
                }
            }
        }
    }
    result.push('"');
    result
}

struct Deadline<'a> {
    caller: &'a dyn Cancellation,
    until: tokio::time::Instant,
}
impl Cancellation for Deadline<'_> {
    fn is_cancelled(&self) -> bool {
        self.caller.is_cancelled() || tokio::time::Instant::now() >= self.until
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            tokio::select! { _ = self.caller.cancelled() => {}, _ = tokio::time::sleep_until(self.until) => {} }
        })
    }
}
