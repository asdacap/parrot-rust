//! Bounded, cancellable HTTP bodies and incremental SSE framing.
//!
//! The transport boundary is injectable for offline protocol tests. Production
//! requests use the same body/header machinery through `ReqwestTransport`.

mod client;
mod error;
mod events;
mod policy;
mod sse;
mod stream;
mod transport;

pub use client::{ProviderHttpClient, ProviderHttpClientCatalog};
pub use error::{HttpError, HttpFailure};
pub use events::{HttpEventStream, HttpStreamEvent};
pub use policy::{Endpoint, bearer_headers, endpoint_url, has_authority, validate_headers};
pub use sse::{SseDecoder, SseEvent};
pub use stream::{BoundedStream, ByteStream, IdleTimeoutStream, ResponseByteCounter, SseStream};
pub use transport::{
    HttpMethod, HttpRequest, HttpResponse, HttpTransport, ReqwestTransport, StreamOptions,
    StreamingHttpResponse, get, open_stream,
};

pub const MAX_ERROR_BYTES: usize = 64 << 10;
pub const MAX_EVENT_BYTES: usize = 4 << 20;
pub const MAX_STREAM_BYTES: i64 = 64 << 20;
pub const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
pub const MODELS_REFRESH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
