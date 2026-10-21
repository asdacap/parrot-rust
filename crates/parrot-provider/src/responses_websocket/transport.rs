use super::observer::WebSocketAttemptObserver;
use super::{FrameKind, Headers, Socket, SocketState, WebSocketError};
use crate::responses::{ResponsesEvent, ResponsesParser};
use parrot_domain::contracts::async_ops::Cancellation;
use std::{collections::VecDeque, future::Future, time::Duration};
use tokio::time::Instant;

pub const FRAME_BYTES: usize = 1 << 20;
pub const MAXIMUM_MESSAGE_BYTES: usize = 16 << 20;
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
pub const REUSE_LIMIT: Duration = Duration::from_secs(60);
const MAX_EVENT_BYTES: usize = 4 << 20;
const MAX_STREAM_BYTES: u64 = 64 << 20;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExchangeStats {
    pub request_bytes: u64,
    pub response_bytes: Option<u64>,
    pub close_status: Option<u16>,
}

pub struct ResponsesWebSocket {
    socket: Box<dyn Socket>,
    response_headers: Headers,
    idle_timeout: Option<Duration>,
    maximum_request_bytes: usize,
    last_exchange: Instant,
    last_stats: Option<ExchangeStats>,
}
impl ResponsesWebSocket {
    pub fn new(
        socket: Box<dyn Socket>,
        response_headers: Headers,
        idle_timeout: Option<Duration>,
        maximum_request_bytes: usize,
    ) -> Self {
        Self {
            socket,
            response_headers,
            idle_timeout,
            maximum_request_bytes,
            last_exchange: Instant::now(),
            last_stats: None,
        }
    }
    pub fn state(&self) -> SocketState {
        self.socket.state()
    }
    pub fn response_headers(&self) -> &Headers {
        &self.response_headers
    }
    pub fn stale(&self) -> bool {
        self.last_exchange.elapsed() >= REUSE_LIMIT
    }
    pub fn stats(&self) -> Option<&ExchangeStats> {
        self.last_stats.as_ref()
    }
    pub fn abort(&mut self) {
        self.socket.abort();
    }
    pub fn send(
        &mut self,
        request: Vec<u8>,
        provider_id: &str,
    ) -> Result<Exchange<'_>, WebSocketError> {
        let state = self.begin(request, provider_id)?;
        Ok(Exchange {
            connection: self,
            state,
        })
    }
    pub(crate) fn begin(
        &mut self,
        request: Vec<u8>,
        provider_id: &str,
    ) -> Result<ExchangeState, WebSocketError> {
        self.begin_observed(request, provider_id, None)
    }
    pub(crate) fn begin_observed(
        &mut self,
        request: Vec<u8>,
        provider_id: &str,
        observer: Option<&mut (dyn WebSocketAttemptObserver + 'static)>,
    ) -> Result<ExchangeState, WebSocketError> {
        self.last_stats = None;
        if request.len() > self.maximum_request_bytes {
            return Err(WebSocketError::RequestTooLarge {
                maximum: self.maximum_request_bytes,
            });
        }
        if let Some(observer) = observer {
            observer.record_request_bytes(request.len() as u64);
        }
        let stats = ExchangeStats {
            request_bytes: request.len() as u64,
            ..ExchangeStats::default()
        };
        self.last_stats = Some(stats.clone());
        if request.len() > MAXIMUM_MESSAGE_BYTES {
            return Err(WebSocketError::MessageTooLarge);
        }
        Ok(ExchangeState {
            request,
            sent: false,
            parser: ResponsesParser::new(provider_id),
            pending: VecDeque::new(),
            done: false,
            total_bytes: 0,
            stats,
            turn_state: None,
            receive_buffer: vec![0; 8192],
        })
    }
    pub(crate) async fn next(
        &mut self,
        state: &mut ExchangeState,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<ResponsesEvent>, WebSocketError> {
        self.next_observed(state, cancellation, &mut None).await
    }
    pub(crate) async fn next_observed(
        &mut self,
        state: &mut ExchangeState,
        cancellation: &dyn Cancellation,
        observer: &mut Option<Box<dyn WebSocketAttemptObserver>>,
    ) -> Result<Option<ResponsesEvent>, WebSocketError> {
        let result = self.next_inner(state, cancellation, observer).await;
        self.last_stats = Some(state.stats.clone());
        if result.is_err() && !state.done {
            self.socket.abort();
        }
        result
    }
    async fn next_inner(
        &mut self,
        state: &mut ExchangeState,
        cancellation: &dyn Cancellation,
        observer: &mut Option<Box<dyn WebSocketAttemptObserver>>,
    ) -> Result<Option<ResponsesEvent>, WebSocketError> {
        if let Some(event) = state.pending.pop_front() {
            return Ok(Some(event));
        }
        if state.done {
            return Ok(None);
        }
        if !state.sent {
            for (index, frame) in state.request.chunks(FRAME_BYTES).enumerate() {
                let end = (index + 1) * FRAME_BYTES >= state.request.len();
                idle(
                    self.idle_timeout,
                    cancellation,
                    self.socket.send(frame, end, cancellation),
                )
                .await?;
            }
            state.sent = true;
        }
        loop {
            let mut written = 0;
            loop {
                if state.receive_buffer.len() - written < 8192 {
                    state
                        .receive_buffer
                        .resize(state.receive_buffer.len() * 2, 0);
                }
                let frame = idle(
                    self.idle_timeout,
                    cancellation,
                    self.socket
                        .receive(&mut state.receive_buffer[written..], cancellation),
                )
                .await?;
                if frame.kind != FrameKind::Close {
                    if let Some(observer) = observer {
                        observer.record_response_bytes(frame.count as u64);
                    }
                    *state.stats.response_bytes.get_or_insert(0) += frame.count as u64;
                }
                if frame.kind == FrameKind::Close {
                    state.stats.close_status = frame.close_status;
                    if let (Some(observer), Some(status)) = (observer.as_mut(), frame.close_status)
                    {
                        observer.record_close_status(status);
                    }
                    return Err(WebSocketError::Wire(format!(
                        "responses: websocket closed before a terminal event (status {}: {})",
                        frame
                            .close_status
                            .map(|status| status.to_string())
                            .unwrap_or_default(),
                        frame.close_description.unwrap_or_default()
                    )));
                }
                if frame.kind != FrameKind::Text {
                    return Err(WebSocketError::Wire(
                        "responses: unexpected binary websocket event".into(),
                    ));
                }
                written += frame.count;
                if written > MAX_EVENT_BYTES {
                    return Err(WebSocketError::Wire(format!(
                        "responses: websocket event exceeds {MAX_EVENT_BYTES} bytes"
                    )));
                }
                if frame.end_of_message {
                    break;
                }
            }
            state.total_bytes += written as u64;
            if state.total_bytes > MAX_STREAM_BYTES {
                return Err(WebSocketError::Wire(format!(
                    "responses: provider stream exceeds {MAX_STREAM_BYTES} bytes"
                )));
            }
            let data = String::from_utf8_lossy(&state.receive_buffer[..written]);
            let events = state.parser.push_data(&data)?;
            capture_turn_state(&data, &mut state.turn_state);
            if events
                .iter()
                .any(|event| matches!(event, ResponsesEvent::Completed(_)))
            {
                state.done = true;
                self.last_exchange = Instant::now();
            }
            state.pending.extend(events);
            if let Some(event) = state.pending.pop_front() {
                return Ok(Some(event));
            }
            if state.done {
                return Ok(None);
            }
        }
    }
}
impl Drop for ResponsesWebSocket {
    fn drop(&mut self) {
        self.socket.abort();
        self.socket.dispose();
    }
}

pub(crate) struct ExchangeState {
    request: Vec<u8>,
    sent: bool,
    parser: ResponsesParser,
    pending: VecDeque<ResponsesEvent>,
    pub(crate) done: bool,
    total_bytes: u64,
    pub(crate) stats: ExchangeStats,
    pub(crate) turn_state: Option<String>,
    receive_buffer: Vec<u8>,
}
pub struct Exchange<'a> {
    connection: &'a mut ResponsesWebSocket,
    state: ExchangeState,
}
impl Exchange<'_> {
    pub async fn next(
        &mut self,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<ResponsesEvent>, WebSocketError> {
        self.connection.next(&mut self.state, cancellation).await
    }
    pub fn stats(&self) -> &ExchangeStats {
        &self.state.stats
    }
    pub fn turn_state(&self) -> Option<&str> {
        self.state.turn_state.as_deref()
    }
}
impl Drop for Exchange<'_> {
    fn drop(&mut self) {
        if !self.state.done {
            self.connection.abort();
        }
    }
}

async fn idle<T>(
    timeout: Option<Duration>,
    cancellation: &dyn Cancellation,
    operation: impl Future<Output = Result<T, WebSocketError>>,
) -> Result<T, WebSocketError> {
    let Some(timeout) = timeout else {
        return tokio::select! { biased; result = operation => result, _ = cancellation.cancelled() => Err(WebSocketError::Cancelled) };
    };
    tokio::select! {
        biased;
        result = operation => result,
        _ = cancellation.cancelled() => Err(WebSocketError::Cancelled),
        _ = tokio::time::sleep(timeout) => if cancellation.is_cancelled() { Err(WebSocketError::Cancelled) } else { Err(WebSocketError::Wire("responses: idle timeout waiting for websocket".into())) },
    }
}
fn capture_turn_state(data: &str, target: &mut Option<String>) {
    if target.is_some() {
        return;
    }
    let Ok(root) = serde_json::from_str::<RawProperties>(data) else {
        return;
    };
    let property = |name: &str| {
        root.0
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.get())
    };
    if property("type")
        .and_then(|value| serde_json::from_str::<String>(value).ok())
        .as_deref()
        != Some("response.metadata")
    {
        return;
    }
    let Some(headers) =
        property("headers").and_then(|value| serde_json::from_str::<RawProperties>(value).ok())
    else {
        return;
    };
    for (key, value) in headers.0 {
        if key.eq_ignore_ascii_case("x-codex-turn-state") {
            if let Ok(value) = serde_json::from_str::<String>(value.get()) {
                if !value.is_empty() {
                    *target = Some(value);
                    break;
                }
            }
        }
    }
}

// JsonElement enumerates duplicate header properties in source order and does
// not convert unrelated numeric literals into floating-point values.
struct RawProperties(Vec<(String, Box<serde_json::value::RawValue>)>);
impl<'de> serde::Deserialize<'de> for RawProperties {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct PropertiesVisitor;
        impl<'de> serde::de::Visitor<'de> for PropertiesVisitor {
            type Value = RawProperties;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON object")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                let mut properties = Vec::new();
                while let Some(property) = map.next_entry()? {
                    properties.push(property);
                }
                Ok(RawProperties(properties))
            }
        }
        deserializer.deserialize_map(PropertiesVisitor)
    }
}

#[cfg(test)]
#[path = "original_text_frame_tests.rs"]
mod original_text_frame_tests;
