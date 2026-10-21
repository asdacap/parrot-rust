use crate::{
    http_stream::{ByteStream, Endpoint, HttpError, HttpResponse, ProviderHttpClient},
    sessions::Error,
};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};

/// Content headers remain separate: AWS signs request headers, not content headers.
pub struct ImageHttpRequest {
    pub method: String,
    pub endpoint: Endpoint,
    pub headers: Vec<(String, String)>,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

pub trait ImageTransport: Send + Sync {
    fn send<'a>(
        &'a self,
        request: ImageHttpRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, Error>>;
}

pub struct ReqwestImageTransport {
    client: ProviderHttpClient,
}
impl ReqwestImageTransport {
    pub fn new(client: ProviderHttpClient) -> Self {
        Self { client }
    }
}
impl ImageTransport for ReqwestImageTransport {
    fn send<'a>(
        &'a self,
        request: ImageHttpRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, Error>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let client = self.client.client_for(&request.endpoint)?;
            let method = reqwest::Method::from_bytes(request.method.as_bytes())
                .map_err(|e| Error::Provider(e.to_string()))?;
            let mut outbound = client
                .request(method, request.endpoint.into_url())
                .body(request.body);
            if let Some(content_type) = request.content_type {
                outbound = outbound.header("Content-Type", content_type);
            }
            for (name, value) in request.headers {
                outbound = outbound.header(name, value);
            }
            let response = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(Error::Cancelled),
                _ = self.client.closed() => return Err(Error::Disposed),
                result = outbound.send() => result.map_err(|e| Error::Http(HttpError::Transport(e.to_string())))?,
            };
            let status = response.status().as_u16();
            let reason = response
                .status()
                .canonical_reason()
                .unwrap_or_default()
                .to_owned();
            let headers = response
                .headers()
                .iter()
                .map(|(name, value)| {
                    (
                        name.to_string(),
                        value.to_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect();
            Ok(HttpResponse {
                status,
                reason,
                headers,
                body: Box::new(ResponseBody {
                    client: self.client.clone(),
                    response,
                    pending: Vec::new(),
                    offset: 0,
                }),
            })
        })
    }
}
struct ResponseBody {
    client: ProviderHttpClient,
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
            if self.client.is_disposed() {
                return Err(HttpError::Disposed);
            }
            if buffer.is_empty() {
                return Ok(0);
            }
            if self.offset == self.pending.len() {
                let chunk = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return Err(HttpError::Cancelled),
                    _ = self.client.closed() => return Err(HttpError::Disposed),
                    result = self.response.chunk() => result.map_err(|e| HttpError::Transport(e.to_string()))?,
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
