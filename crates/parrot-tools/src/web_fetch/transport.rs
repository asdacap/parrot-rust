use super::{
    FetchError, Method, Options, Request, Resolver, Response, ResponseBody, Transport,
    TransportSession,
};
use parrot_domain::contracts::async_ops::BoxFuture;
use reqwest::header::{
    ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_TYPE, COOKIE, LOCATION, SET_COOKIE, USER_AGENT,
};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use url::Url;

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemResolver;

impl Resolver for SystemResolver {
    fn resolve<'a>(&'a self, host: &'a str) -> BoxFuture<'a, io::Result<Vec<IpAddr>>> {
        Box::pin(async move {
            Ok(tokio::net::lookup_host((host, 0))
                .await?
                .map(|address| address.ip())
                .collect())
        })
    }
}

/// A standards-compliant jar supplies cookies for the exact URL and processes Set-Cookie.
pub trait CookieStore: Send + Sync {
    fn request_header(&self, address: &Url) -> Option<String>;
    fn store_response(&self, address: &Url, set_cookie: &[String]);
}

/// Creates a fresh, unshared-across-fetches cookie container.
pub trait CookieStoreFactory: Send + Sync {
    fn create(&self) -> Arc<dyn CookieStore>;
}

pub struct ReqwestTransport {
    cookies: Arc<dyn CookieStoreFactory>,
}

impl ReqwestTransport {
    pub fn new(cookies: Arc<dyn CookieStoreFactory>) -> Self {
        Self { cookies }
    }
}

impl Transport for ReqwestTransport {
    fn session(&self) -> Result<Box<dyn TransportSession + '_>, FetchError> {
        Ok(Box::new(ReqwestSession {
            cookies: self.cookies.create(),
        }))
    }
}

struct ReqwestSession {
    cookies: Arc<dyn CookieStore>,
}

impl TransportSession for ReqwestSession {
    fn send<'a>(
        &'a mut self,
        request: &'a Request,
        addresses: &'a [SocketAddr],
        options: &'a Options,
    ) -> BoxFuture<'a, Result<Response, FetchError>> {
        Box::pin(async move {
            let host = request.address.host_str().ok_or_else(request_failed)?;
            if addresses.is_empty() {
                return Err(FetchError::rejected(
                    "web fetch attempted to connect to an unvalidated host",
                ));
            }
            // Clients are not reused across pins; the logical fetch session retains only its jar.
            let client = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .no_gzip()
                .no_brotli()
                .no_deflate()
                .no_zstd()
                .connect_timeout(options.timeout)
                .pool_max_idle_per_host(0)
                .resolve_to_addrs(host.trim_matches(['[', ']']), addresses)
                .build()
                .map_err(|_| request_failed())?;
            let method = match request.method {
                Method::Get => reqwest::Method::GET,
                Method::Head => reqwest::Method::HEAD,
            };
            let mut outgoing = client
                .request(method, request.address.clone())
                .header(USER_AGENT, &options.user_agent)
                .header(ACCEPT_ENCODING, "gzip");
            if let Some(cookies) = self.cookies.request_header(&request.address) {
                outgoing = outgoing.header(COOKIE, cookies);
            }
            let response = outgoing.send().await.map_err(|_| request_failed())?;
            let set_cookie = response
                .headers()
                .get_all(SET_COOKIE)
                .iter()
                .filter_map(|value| value.to_str().ok().map(str::to_owned))
                .collect::<Vec<_>>();
            self.cookies.store_response(&request.address, &set_cookie);
            let location = response
                .headers()
                .get(LOCATION)
                .map(|value| {
                    value
                        .to_str()
                        .map(str::to_owned)
                        .map_err(|_| request_failed())
                })
                .transpose()?;
            // SocketsHttpHandler's default response-header decoder is Latin-1.
            let content_type = response.headers().get(CONTENT_TYPE).map(|value| {
                value
                    .as_bytes()
                    .iter()
                    .map(|byte| char::from(*byte))
                    .collect::<String>()
            });
            let mut content_encodings = Vec::new();
            for value in response.headers().get_all(CONTENT_ENCODING) {
                let value = value.to_str().map_err(|_| request_failed())?;
                content_encodings
                    .extend(value.split(',').map(|encoding| encoding.trim().to_owned()));
            }
            Ok(Response {
                status_code: response.status().as_u16(),
                location,
                content_type,
                content_encodings,
                body: Box::new(ReqwestBody {
                    response,
                    pending: Vec::new(),
                    position: 0,
                }),
            })
        })
    }
}

struct ReqwestBody {
    response: reqwest::Response,
    pending: Vec<u8>,
    position: usize,
}

impl ResponseBody for ReqwestBody {
    fn read<'a>(&'a mut self, output: &'a mut [u8]) -> BoxFuture<'a, Result<usize, FetchError>> {
        Box::pin(async move {
            if output.is_empty() {
                return Ok(0);
            }
            while self.position == self.pending.len() {
                match self.response.chunk().await.map_err(|_| request_failed())? {
                    Some(chunk) => {
                        self.pending = chunk.to_vec();
                        self.position = 0;
                    }
                    None => return Ok(0),
                }
            }
            let length = output.len().min(self.pending.len() - self.position);
            output[..length].copy_from_slice(&self.pending[self.position..self.position + length]);
            self.position += length;
            Ok(length)
        })
    }
}

fn request_failed() -> FetchError {
    FetchError::rejected("web fetch request failed")
}
