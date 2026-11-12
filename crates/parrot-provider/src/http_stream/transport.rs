use super::{
    BoundedStream, ByteStream, Endpoint, HttpError, HttpFailure, IdleTimeoutStream,
    MAX_ERROR_BYTES, MAX_STREAM_BYTES, ProviderHttpClient, ResponseByteCounter,
};
use crate::anthropic::wire_json::selected_json_string;
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use serde_json::value::RawValue;
use std::{collections::BTreeMap, time::Duration};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HttpMethod {
    Get,
    Post,
}

pub struct HttpRequest {
    pub method: HttpMethod,
    pub endpoint: Endpoint,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub struct HttpResponse {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Box<dyn ByteStream>,
}

/// A response transfers ownership of its body; transport failures never retry.
pub trait HttpTransport: Send + Sync {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>>;
}

pub struct ReqwestTransport {
    client: ProviderHttpClient,
}
impl ReqwestTransport {
    pub fn new(client: ProviderHttpClient) -> Self {
        Self { client }
    }
}
impl HttpTransport for ReqwestTransport {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(HttpError::Cancelled);
            }
            let client = self.client.client_for(&request.endpoint)?;
            let method = match request.method {
                HttpMethod::Get => reqwest::Method::GET,
                HttpMethod::Post => reqwest::Method::POST,
            };
            let mut outbound = client
                .request(method, request.endpoint.into_url())
                .body(request.body);
            for (name, value) in request.headers {
                outbound = outbound.header(name, value);
            }
            let response = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(HttpError::Cancelled),
                _ = self.client.closed() => return Err(HttpError::Disposed),
                result = outbound.send() => result.map_err(|error| HttpError::Transport(error.to_string()))?,
            };
            let status = response.status().as_u16();
            let reason = response
                .status()
                .canonical_reason()
                .unwrap_or_default()
                .to_owned();
            let mut headers: Vec<(String, String)> = Vec::new();
            for name in response.headers().keys() {
                // HttpResponseMessage.Headers excludes the standard content
                // header collection even though both arrive on the same wire.
                if [
                    "allow",
                    "content-disposition",
                    "content-encoding",
                    "content-language",
                    "content-length",
                    "content-location",
                    "content-md5",
                    "content-range",
                    "content-type",
                    "expires",
                    "last-modified",
                ]
                .contains(&name.as_str())
                {
                    continue;
                }
                let values = response
                    .headers()
                    .get_all(name)
                    .iter()
                    .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
                    .collect::<Vec<_>>()
                    .join(", ");
                headers.push((name.to_string(), values));
            }
            Ok(HttpResponse {
                status,
                reason,
                headers,
                body: Box::new(ResponseBody {
                    owner: self.client.clone(),
                    response,
                    pending: Vec::new(),
                    offset: 0,
                }),
            })
        })
    }
}

struct ResponseBody {
    owner: ProviderHttpClient,
    response: reqwest::Response,
    pending: Vec<u8>,
    offset: usize,
}
impl ByteStream for ResponseBody {
    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(HttpError::Cancelled);
            }
            if buffer.is_empty() {
                return Ok(0);
            }
            while self.offset == self.pending.len() {
                let chunk = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return Err(HttpError::Cancelled),
                    _ = self.owner.closed() => return Err(HttpError::Disposed),
                    result = self.response.chunk() => result.map_err(|error| HttpError::Transport(error.to_string()))?,
                };
                let Some(chunk) = chunk else {
                    return Ok(0);
                };
                self.pending = chunk.to_vec();
                self.offset = 0;
            }
            let count = buffer.len().min(self.pending.len() - self.offset);
            buffer[..count].copy_from_slice(&self.pending[self.offset..self.offset + count]);
            self.offset += count;
            Ok(count)
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct StreamOptions {
    pub header_timeout: Duration,
    pub idle_timeout: Duration,
    pub maximum_request_bytes: usize,
}

impl StreamOptions {
    /// Converts configured source millisecond values without wrapping negatives.
    pub fn from_millis(
        header_timeout: i64,
        idle_timeout: i64,
        maximum_request_bytes: usize,
    ) -> Result<Self, HttpError> {
        if idle_timeout < 0 {
            return Err(HttpError::Provider(
                "provider: stream idle timeout cannot be negative".into(),
            ));
        }
        Ok(Self {
            header_timeout: Duration::from_millis(header_timeout.max(0) as u64),
            idle_timeout: Duration::from_millis(idle_timeout as u64),
            maximum_request_bytes,
        })
    }
}

pub struct StreamingHttpResponse {
    pub content: BoundedStream<IdleTimeoutStream<Box<dyn ByteStream>>>,
    pub headers: Vec<(String, String)>,
}
impl StreamingHttpResponse {
    pub fn turn_state(&self) -> Option<&str> {
        self.headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("x-codex-turn-state"))
            .map(|(_, value)| value.as_str())
    }
}

pub async fn open_stream(
    transport: &dyn HttpTransport,
    endpoint: impl Into<Endpoint>,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
    options: StreamOptions,
    counter: ResponseByteCounter,
    cancellation: &dyn Cancellation,
) -> Result<StreamingHttpResponse, HttpError> {
    if body.len() > options.maximum_request_bytes {
        return Err(HttpError::RequestTooLarge(options.maximum_request_bytes));
    }
    let mut outbound = vec![
        ("Content-Type".into(), "application/json".into()),
        ("Accept".into(), "text/event-stream".into()),
    ];
    outbound.extend(headers);
    let request = HttpRequest {
        method: HttpMethod::Post,
        endpoint: endpoint.into(),
        headers: outbound,
        body,
    };
    let response = if options.header_timeout.is_zero() {
        transport.send(request, cancellation).await?
    } else {
        tokio::select! {
            biased;
            result = transport.send(request, cancellation) => result?,
            _ = cancellation.cancelled() => return Err(HttpError::Cancelled),
            _ = tokio::time::sleep(options.header_timeout) => return Err(if cancellation.is_cancelled() { HttpError::Cancelled } else { HttpError::HeaderTimeout(options.header_timeout) }),
        }
    };
    if !(200..300).contains(&response.status) {
        return Err(parse_error(response, &counter, cancellation).await?);
    }
    Ok(StreamingHttpResponse {
        content: BoundedStream::new(
            IdleTimeoutStream::new(response.body, options.idle_timeout),
            MAX_STREAM_BYTES,
            counter,
        ),
        headers: response.headers,
    })
}

pub async fn get(
    transport: &dyn HttpTransport,
    endpoint: impl Into<Endpoint>,
    headers: Vec<(String, String)>,
    timeout: Duration,
    max_bytes: usize,
    cancellation: &dyn Cancellation,
) -> Result<String, HttpError> {
    let execute = async {
        let mut outbound = vec![("Accept".into(), "application/json".into())];
        outbound.extend(headers);
        let mut response = transport
            .send(
                HttpRequest {
                    method: HttpMethod::Get,
                    endpoint: endpoint.into(),
                    headers: outbound,
                    body: Vec::new(),
                },
                cancellation,
            )
            .await?;
        if !(200..300).contains(&response.status) {
            return Err(
                parse_error(response, &ResponseByteCounter::default(), cancellation).await?,
            );
        }
        let bytes = read_at_most(
            &mut *response.body,
            max_bytes.saturating_add(1),
            cancellation,
        )
        .await?;
        if bytes.len() > max_bytes {
            return Err(HttpError::ProviderHttp(format!(
                "provider: response exceeds {max_bytes} bytes"
            )));
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    };
    if timeout.is_zero() {
        execute.await
    } else {
        tokio::select! {
            biased;
            result = execute => result,
            _ = cancellation.cancelled() => Err(HttpError::Cancelled),
            _ = tokio::time::sleep(timeout) => Err(if cancellation.is_cancelled() { HttpError::Cancelled } else { HttpError::ProviderHttp("provider: request timed out".into()) }),
        }
    }
}

async fn read_at_most(
    stream: &mut dyn ByteStream,
    limit: usize,
    cancellation: &dyn Cancellation,
) -> Result<Vec<u8>, HttpError> {
    let mut bytes = vec![0; limit];
    let mut total = 0;
    while total < limit {
        let count = stream.read(&mut bytes[total..], cancellation).await?;
        if count == 0 {
            break;
        }
        total += count;
    }
    bytes.truncate(total);
    Ok(bytes)
}

async fn parse_error(
    mut response: HttpResponse,
    counter: &ResponseByteCounter,
    cancellation: &dyn Cancellation,
) -> Result<HttpError, HttpError> {
    let bytes = match read_at_most(&mut *response.body, MAX_ERROR_BYTES + 4, cancellation).await {
        Ok(bytes) => bytes,
        Err(error) if cancellation.is_cancelled() => return Err(error),
        Err(_) => {
            return Ok(HttpError::Response(HttpFailure {
                status: response.status,
                error_type: String::new(),
                error_code: String::new(),
                detail: "unable to read provider error".into(),
                response_body: String::new(),
            }));
        }
    };
    counter.add(bytes.len());
    let body = String::from_utf8_lossy(&bytes).into_owned();
    let fields = extract_error(&body)?;
    let detail = if !fields.2.is_empty() {
        fields.2
    } else {
        let sanitized = sanitize(&body, 1024);
        if sanitized.is_empty() {
            response.reason
        } else {
            sanitized
        }
    };
    Ok(HttpError::Response(HttpFailure {
        status: response.status,
        error_type: sanitize(&fields.0, 128),
        error_code: sanitize(&fields.1, 128),
        detail,
        response_body: crate::wire::bound_body(&body),
    }))
}

fn extract_error(body: &str) -> Result<(String, String, String), HttpError> {
    let Ok(root) = serde_json::from_str::<BTreeMap<String, Box<RawValue>>>(body) else {
        return Ok(Default::default());
    };
    let nested = root.get("error").and_then(|value| {
        serde_json::from_str::<BTreeMap<String, Box<RawValue>>>(value.get()).ok()
    });
    let scope = nested.as_ref().unwrap_or(&root);
    let string = |name: &str| match scope.get(name).map(|value| value.get()) {
        Some(value) if value.starts_with('"') => {
            selected_json_string(value).map_err(HttpError::InvalidJsonString)
        }
        _ => Ok(String::new()),
    };
    // The source extracts these fields in order. Unused JSON strings are opaque;
    // only a selected string can raise the native malformed UTF-16 failure.
    let error_type = string("type")?;
    let code = match scope.get("code").map(|raw| raw.get()) {
        Some(value) if value.starts_with('"') => {
            selected_json_string(value).map_err(HttpError::InvalidJsonString)?
        }
        Some(value)
            if value == "true"
                || value == "false"
                || value.starts_with('-')
                || value.as_bytes().first().is_some_and(u8::is_ascii_digit) =>
        {
            value.to_owned()
        }
        _ => String::new(),
    };
    Ok((error_type, code, sanitize(&string("message")?, 1024)))
}

fn sanitize(value: &str, limit: usize) -> String {
    let cleaned: String = value
        .chars()
        .filter(|c| !c.is_control() || *c == '\t')
        .collect();
    String::from_utf16_lossy(
        &cleaned
            .trim()
            .encode_utf16()
            .take(limit)
            .collect::<Vec<_>>(),
    )
}

#[cfg(test)]
mod original_helper_tests {
    use crate::wire::bound_body;

    #[test]
    fn source_response_bodies_are_bounded_without_splitting_utf8() {
        let body = "界".repeat(30_000);
        let bounded = bound_body(&body);
        let within_limit = bounded.len() <= 64 << 10;
        let has_suffix = bounded.ends_with("… [truncated]");
        let no_replacement_character = !bounded.contains('�');
        println!(
            "T25_ORIGINAL_HELPER_BOUND input_repetitions=30000 input_utf8_bytes={} output_utf8_bytes={} within_64_kib={within_limit} suffix={has_suffix} no_replacement_character={no_replacement_character}",
            body.len(),
            bounded.len()
        );
        assert!(within_limit);
        assert!(has_suffix);
        assert!(no_replacement_character);
    }
}
