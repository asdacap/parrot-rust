mod content_type;
mod cookies;
mod gzip;
mod policy;
mod text;
mod tool;
mod transport;

pub use cookies::FetchCookieStoreFactory;
pub use gzip::GzipStreamDecoder;
pub use policy::{AddressPolicy, PrivateAddressPolicy, PublicAddressPolicy};
pub use tool::{Invocation, Output, WebFetchTool};
pub use transport::{CookieStore, CookieStoreFactory, ReqwestTransport, SystemResolver};

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationError};
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use url::Url;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Method {
    Get,
    Head,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Head => "HEAD",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Request {
    pub address: Url,
    pub method: Method,
}

impl Request {
    pub fn parse(arguments_json: &str) -> Result<Self, FetchError> {
        let input: serde_json::Value = serde_json::from_str(arguments_json).map_err(|error| {
            FetchError::InvalidArguments(format!("invalid web fetch arguments: {error}"))
        })?;
        if !input.is_object() && !input.is_null() {
            return Err(FetchError::rejected("web fetch URL is required"));
        }
        let field = |name: &str| match input.get(name) {
            None | Some(serde_json::Value::Null) => Ok(""),
            Some(serde_json::Value::String(value)) => Ok(value.as_str()),
            _ => Err(FetchError::InvalidArguments(format!(
                "invalid web fetch arguments: {name} must be a string"
            ))),
        };
        let address = field("url")?;
        let method = field("method")?;
        let method = match method.trim().to_uppercase().as_str() {
            "" | "GET" => Method::Get,
            "HEAD" => Method::Head,
            _ => return Err(FetchError::rejected("web fetch supports only GET and HEAD")),
        };
        Ok(Self {
            address: normalize_address(address)?,
            method,
        })
    }
}

pub fn normalize_address(address: &str) -> Result<Url, FetchError> {
    let raw = address.trim();
    let raw = if raw.contains("://") {
        raw.to_owned()
    } else {
        format!("https://{raw}")
    };
    let mut address = Url::parse(&raw).map_err(|_| invalid_address())?;
    if !matches!(address.scheme(), "http" | "https")
        || address.host_str().is_none_or(str::is_empty)
        || !address.username().is_empty()
        || address.password().is_some()
    {
        return Err(invalid_address());
    }
    address.set_fragment(None);
    Ok(address)
}

fn invalid_address() -> FetchError {
    FetchError::rejected("web fetch URL must be HTTP or HTTPS without user information")
}

#[derive(Debug)]
pub enum FetchError {
    Rejected(String),
    InvalidArguments(String),
    Template(String),
    Io(io::Error),
}

impl FetchError {
    fn rejected(message: impl Into<String>) -> Self {
        Self::Rejected(message.into())
    }

    pub fn is_tool_error(&self) -> bool {
        matches!(self, Self::Rejected(_) | Self::InvalidArguments(_))
    }
}

impl fmt::Display for FetchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected(message) | Self::InvalidArguments(message) | Self::Template(message) => {
                formatter.write_str(message)
            }
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for FetchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FetchResult {
    pub final_address: Url,
    pub status_code: u16,
    pub content_type: String,
    pub text: String,
    pub truncated: bool,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub max_redirects: usize,
    pub max_body_bytes: usize,
    pub timeout: Duration,
    pub user_agent: String,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            max_redirects: 5,
            max_body_bytes: 2 << 20,
            timeout: Duration::from_secs(20),
            user_agent: "parrot-coder-webfetch/1".to_owned(),
        }
    }
}

/// Resolution is used only once per canonical host during a fetch.
pub trait Resolver: Send + Sync {
    fn resolve<'a>(&'a self, host: &'a str) -> BoxFuture<'a, io::Result<Vec<IpAddr>>>;
}

/// A response body is read incrementally into the caller's bounded buffer.
pub trait ResponseBody: Send {
    fn read<'a>(&'a mut self, output: &'a mut [u8]) -> BoxFuture<'a, Result<usize, FetchError>>;
}

pub struct Response {
    pub status_code: u16,
    pub location: Option<String>,
    pub content_type: Option<String>,
    pub content_encodings: Vec<String>,
    pub body: Box<dyn ResponseBody>,
}

/// Creates one HTTP session per fetch, retaining response cookies through redirects.
pub trait Transport: Send + Sync {
    fn session(&self) -> Result<Box<dyn TransportSession + '_>, FetchError>;
}

/// Sends only to supplied approved addresses, without proxy, redirect, or DNS fallback.
pub trait TransportSession: Send {
    fn send<'a>(
        &'a mut self,
        request: &'a Request,
        addresses: &'a [SocketAddr],
        options: &'a Options,
    ) -> BoxFuture<'a, Result<Response, FetchError>>;
}

/// Supplies streaming gzip decompression; the fetcher bounds decoded, not encoded bytes.
pub trait GzipDecoder: Send + Sync {
    fn decode(
        &self,
        compressed: Box<dyn ResponseBody>,
    ) -> Result<Box<dyn ResponseBody>, FetchError>;
}

pub struct WebFetcher {
    policy: Arc<dyn AddressPolicy>,
    resolver: Arc<dyn Resolver>,
    transport: Arc<dyn Transport>,
    gzip: Arc<dyn GzipDecoder>,
    options: Options,
}

impl WebFetcher {
    pub fn production(policy: Arc<dyn AddressPolicy>) -> Self {
        Self::standard(
            policy,
            Arc::new(GzipStreamDecoder),
            Arc::new(FetchCookieStoreFactory),
        )
    }

    pub fn new(
        policy: Arc<dyn AddressPolicy>,
        resolver: Arc<dyn Resolver>,
        transport: Arc<dyn Transport>,
        gzip: Arc<dyn GzipDecoder>,
        options: Options,
    ) -> Self {
        Self {
            policy,
            resolver,
            transport,
            gzip,
            options,
        }
    }

    pub fn standard(
        policy: Arc<dyn AddressPolicy>,
        gzip: Arc<dyn GzipDecoder>,
        cookies: Arc<dyn CookieStoreFactory>,
    ) -> Self {
        Self::new(
            policy,
            Arc::new(SystemResolver),
            Arc::new(ReqwestTransport::new(cookies)),
            gzip,
            Options::default(),
        )
    }

    pub async fn fetch(
        &self,
        request: Request,
        cancellation: &dyn Cancellation,
    ) -> Result<FetchResult, OperationError<FetchError>> {
        let operation = self.fetch_core(request);
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(OperationError::Cancelled),
            result = tokio::time::timeout(self.options.timeout, operation) => {
                match result {
                    Ok(result) => result.map_err(OperationError::Failed),
                    Err(_) if cancellation.is_cancelled() => Err(OperationError::Cancelled),
                    Err(_) => Err(OperationError::Failed(FetchError::rejected("web fetch timed out"))),
                }
            }
        }
    }

    async fn fetch_core(&self, mut request: Request) -> Result<FetchResult, FetchError> {
        let mut pins = HashMap::<String, Vec<IpAddr>>::new();
        let mut session = self.transport.session()?;
        for redirect in 0.. {
            let addresses = self.pin(&request.address, &mut pins).await?;
            let response = session.send(&request, &addresses, &self.options).await?;
            if matches!(response.status_code, 301 | 302 | 303 | 307 | 308) {
                if let Some(location) = &response.location {
                    let next = redirect_address(&request.address, location)?;
                    if redirect >= self.options.max_redirects {
                        return Err(FetchError::rejected(
                            "web fetch followed too many redirects",
                        ));
                    }
                    request.address = next;
                    continue;
                }
            }
            let content_type = accepted_content_type(response.content_type.as_deref())?;
            let mut body = match response.content_encodings.as_slice() {
                [] => response.body,
                [encoding] if encoding.eq_ignore_ascii_case("identity") => response.body,
                [encoding] if encoding.eq_ignore_ascii_case("gzip") => {
                    self.gzip.decode(response.body)?
                }
                _ => {
                    return Err(FetchError::rejected(
                        "web fetch response used an unsupported content encoding",
                    ));
                }
            };
            let maximum =
                self.options.max_body_bytes.checked_add(1).ok_or_else(|| {
                    FetchError::Io(io::Error::other("web fetch body limit overflow"))
                })?;
            let mut buffer = vec![0; maximum];
            let mut length = 0;
            while length < maximum {
                let read = body.read(&mut buffer[length..]).await?;
                if read == 0 {
                    break;
                }
                length += read;
            }
            let truncated = length > self.options.max_body_bytes;
            let decoded =
                String::from_utf8_lossy(&buffer[..length.min(self.options.max_body_bytes)]);
            let text = if content_type == "text/html" {
                text::extract_html(&decoded)
            } else {
                text::strip_controls(&decoded)
            };
            return Ok(FetchResult {
                final_address: request.address,
                status_code: response.status_code,
                content_type,
                text,
                truncated,
            });
        }
        unreachable!("unbounded redirect counter")
    }

    async fn pin(
        &self,
        address: &Url,
        pins: &mut HashMap<String, Vec<IpAddr>>,
    ) -> Result<Vec<SocketAddr>, FetchError> {
        if !matches!(address.scheme(), "http" | "https") {
            return Err(FetchError::rejected(
                "web fetch redirect used a forbidden scheme",
            ));
        }
        let host = policy::canonical_host(
            address
                .host_str()
                .unwrap_or_default()
                .trim_matches(['[', ']']),
        );
        if host.is_empty() || !self.policy.allows_host(&host) {
            return Err(FetchError::rejected(
                "web fetch local or empty host is forbidden",
            ));
        }
        if !pins.contains_key(&host) {
            let resolved = if let Ok(literal) = host.parse::<IpAddr>() {
                vec![literal]
            } else {
                self.resolver.resolve(&host).await.map_err(|_| {
                    FetchError::rejected(format!("web fetch could not resolve {host}"))
                })?
            };
            let mut allowed: Vec<_> = resolved
                .into_iter()
                .map(policy::canonical_address)
                .filter(|address| self.policy.allows(*address))
                .collect();
            allowed.sort_unstable_by(|left, right| {
                left.is_ipv6()
                    .cmp(&right.is_ipv6())
                    .then_with(|| left.to_string().cmp(&right.to_string()))
            });
            allowed.dedup();
            if allowed.is_empty() {
                return Err(FetchError::rejected(
                    "web fetch host resolves only to forbidden addresses",
                ));
            }
            pins.insert(host.clone(), allowed);
        }
        let port = address
            .port_or_known_default()
            .expect("HTTP schemes have ports");
        Ok(pins[&host]
            .iter()
            .map(|address| SocketAddr::new(*address, port))
            .collect())
    }
}

fn accepted_content_type(value: Option<&str>) -> Result<String, FetchError> {
    let media_type = content_type::media_type(value.unwrap_or_default()).ok_or_else(|| {
        FetchError::rejected("web fetch response used an unsupported content type")
    })?;
    let parsed = media_type
        .parse::<mime::Mime>()
        .map_err(|_| FetchError::rejected("web fetch response used an unsupported content type"))?;
    let media_type = parsed.essence_str().to_ascii_lowercase();
    if !matches!(
        media_type.as_str(),
        "text/plain"
            | "text/html"
            | "application/json"
            | "text/json"
            | "text/markdown"
            | "text/x-markdown"
            | "application/markdown"
    ) {
        return Err(FetchError::rejected(
            "web fetch response used an unsupported content type",
        ));
    }
    Ok(media_type)
}

fn redirect_address(current: &Url, location: &str) -> Result<Url, FetchError> {
    let mut target = current
        .join(location)
        .map_err(|_| FetchError::rejected("web fetch redirect used a forbidden address"))?;
    if !matches!(target.scheme(), "http" | "https")
        || !target.username().is_empty()
        || target.password().is_some()
        || target.host_str().is_none_or(str::is_empty)
    {
        return Err(FetchError::rejected(
            "web fetch redirect used a forbidden address",
        ));
    }
    target.set_fragment(None);
    Ok(target)
}
