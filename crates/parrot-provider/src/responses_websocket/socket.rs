use super::{ResponsesWebSocket, SocketErrorCode, WebSocketError};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use std::{sync::Arc, time::Duration};

pub type Headers = Vec<(String, String)>;

pub fn has_authority(endpoint: &str, authority: &str) -> bool {
    let Some((scheme, remainder)) = endpoint.trim().split_once(':') else {
        return false;
    };
    let scheme = match scheme.to_ascii_lowercase().as_str() {
        "wss" => "https".to_owned(),
        "ws" => "http".to_owned(),
        _ => scheme.to_owned(),
    };
    let Ok(endpoint) = crate::http_stream::Endpoint::parse(&format!("{scheme}:{remainder}")) else {
        return false;
    };
    fn ordinal_fold(character: char) -> char {
        let mut upper = character.to_uppercase();
        let first = upper.next().unwrap_or(character);
        if upper.next().is_none() {
            first
        } else {
            character
        }
    }
    endpoint
        .authority()
        .chars()
        .map(ordinal_fold)
        .eq(authority.chars().map(ordinal_fold))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocketState {
    Connecting,
    Open,
    Closed,
    Aborted,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameKind {
    Text,
    Binary,
    Close,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceivedFrame {
    pub count: usize,
    pub kind: FrameKind,
    pub end_of_message: bool,
    pub close_status: Option<u16>,
    pub close_description: Option<String>,
}
pub trait Socket: Send {
    fn state(&self) -> SocketState;
    fn send<'a>(
        &'a mut self,
        data: &'a [u8],
        end_of_message: bool,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), WebSocketError>>;
    fn receive<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ReceivedFrame, WebSocketError>>;
    fn abort(&mut self);
    fn dispose(&mut self) {
        self.abort();
    }
}
pub struct ConnectedSocket {
    pub socket: Box<dyn Socket>,
    pub response_headers: Headers,
}
pub trait Connector: Send + Sync {
    fn connect<'a>(
        &'a self,
        endpoint: &'a str,
        headers: &'a Headers,
        timeout: Option<Duration>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ConnectedSocket, WebSocketError>>;
}

pub struct WebSocketClient {
    connector: Arc<dyn Connector>,
    endpoint: String,
    connect_timeout: Option<Duration>,
    idle_timeout: Option<Duration>,
    maximum_request_bytes: usize,
}
impl WebSocketClient {
    pub fn new(
        connector: Arc<dyn Connector>,
        http_endpoint: &str,
        connect_timeout: Option<Duration>,
        idle_timeout: Option<Duration>,
        maximum_request_bytes: usize,
    ) -> Result<Self, WebSocketError> {
        Ok(Self {
            connector,
            endpoint: Self::endpoint(http_endpoint)?,
            connect_timeout,
            idle_timeout,
            maximum_request_bytes,
        })
    }
    pub fn endpoint(http_endpoint: &str) -> Result<String, WebSocketError> {
        let endpoint = crate::http_stream::Endpoint::parse(http_endpoint)
            .map_err(|error| WebSocketError::InvalidRequest(error.to_string()))?;
        let scheme = match endpoint.as_url().scheme() {
            "https" => "wss",
            "http" => "ws",
            _ => {
                return Err(WebSocketError::InvalidRequest(
                    "provider: websocket endpoint must derive from HTTP or HTTPS".into(),
                ));
            }
        };
        Ok(format!(
            "{scheme}:{}",
            endpoint.as_str().split_once(':').expect("absolute URL").1
        ))
    }
    pub async fn connect(
        &self,
        headers: &Headers,
        cancellation: &dyn Cancellation,
    ) -> Result<ResponsesWebSocket, WebSocketError> {
        let mut headers = headers.clone();
        if let Some((_, value)) = headers
            .iter_mut()
            .find(|(name, _)| name.eq_ignore_ascii_case("OpenAI-Beta"))
        {
            *value = "responses_websockets=2026-02-06".into();
        } else {
            headers.push((
                "OpenAI-Beta".into(),
                "responses_websockets=2026-02-06".into(),
            ));
        }
        let connected = self
            .connector
            .connect(&self.endpoint, &headers, self.connect_timeout, cancellation)
            .await
            .map_err(|error| {
                if matches!(
                    error,
                    WebSocketError::Socket {
                        code: SocketErrorCode::NotAWebSocket,
                        ..
                    }
                ) {
                    WebSocketError::TransportFailure {
                        message: error.to_string(),
                        cause: Box::new(error),
                    }
                } else {
                    error
                }
            })?;
        Ok(ResponsesWebSocket::new(
            connected.socket,
            connected.response_headers,
            self.idle_timeout,
            self.maximum_request_bytes,
        ))
    }
}
