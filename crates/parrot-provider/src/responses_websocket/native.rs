use super::{
    ConnectedSocket, Connector, FrameKind, Headers, ReceivedFrame, Socket, SocketErrorCode,
    SocketState, WebSocketError,
};
use crate::http_stream::{Endpoint, ProviderHttpClient};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use std::{io::Cursor, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tungstenite::{
    handshake::{client::generate_key, derive_accept_key},
    protocol::frame::{
        Frame, FrameHeader,
        coding::{Data, OpCode},
    },
};

/// Uses the provider's existing authority-scoped HTTP client for the upgrade.
#[derive(Clone)]
pub struct NativeConnector {
    client: ProviderHttpClient,
}
impl NativeConnector {
    pub fn new(client: ProviderHttpClient) -> Self {
        Self { client }
    }

    async fn upgrade(
        &self,
        endpoint: &str,
        headers: &Headers,
    ) -> Result<ConnectedSocket, WebSocketError> {
        let (scheme, rest) = endpoint.trim().split_once(':').ok_or_else(|| {
            WebSocketError::InvalidRequest("provider: invalid websocket endpoint".into())
        })?;
        let scheme = match scheme.to_ascii_lowercase().as_str() {
            "ws" => "http",
            "wss" => "https",
            _ => {
                return Err(WebSocketError::InvalidRequest(
                    "provider: websocket endpoint must use ws or wss".into(),
                ));
            }
        };
        let endpoint = Endpoint::parse(&format!("{scheme}:{rest}"))
            .map_err(|error| WebSocketError::InvalidRequest(error.to_string()))?;
        let client = self
            .client
            .client_for(&endpoint)
            .map_err(|error| WebSocketError::Transport(error.to_string()))?;
        let headers = crate::http_stream::validate_headers(headers)
            .map_err(|error| WebSocketError::InvalidRequest(error.to_string()))?;
        let key = generate_key();
        let mut request = client
            .get(endpoint.as_url().clone())
            .version(reqwest::Version::HTTP_11);
        for (name, value) in &headers {
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "connection"
                    | "upgrade"
                    | "sec-websocket-key"
                    | "sec-websocket-version"
                    | "sec-websocket-extensions"
                    | "sec-websocket-protocol"
            ) {
                return Err(WebSocketError::InvalidRequest(format!(
                    "provider: reserved websocket header {name}"
                )));
            }
            request = request.header(name, value);
        }
        let response = request
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", &key)
            .send()
            .await
            .map_err(|error| WebSocketError::Transport(error.to_string()))?;
        let status = response.status().as_u16();
        if status != 101 {
            return Err(WebSocketError::Upgrade {
                status,
                message: format!("provider: websocket upgrade returned HTTP {status}"),
            });
        }
        let response_headers = response.headers();
        let expected = derive_accept_key(key.as_bytes());
        let accepts: Vec<_> = response_headers
            .get_all("sec-websocket-accept")
            .iter()
            .collect();
        if !has_token(response_headers, "connection", "upgrade")
            || !has_token(response_headers, "upgrade", "websocket")
            || accepts.len() != 1
            || accepts[0].as_bytes() != expected.as_bytes()
            || response_headers.contains_key("sec-websocket-extensions")
            || response_headers.contains_key("sec-websocket-protocol")
        {
            return Err(WebSocketError::Upgrade {
                status,
                message: "provider: invalid websocket upgrade response".into(),
            });
        }
        let mut collected = Vec::new();
        for name in response_headers.keys() {
            let values = response_headers
                .get_all(name)
                .iter()
                .map(|value| value.to_str().map(str::to_owned))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| WebSocketError::Transport(error.to_string()))?;
            collected.push((name.to_string(), values.join(", ")));
        }
        let io = response
            .upgrade()
            .await
            .map_err(|error| WebSocketError::Transport(error.to_string()))?;
        Ok(ConnectedSocket {
            socket: Box::new(NativeSocket::new(io)),
            response_headers: collected,
        })
    }
}
impl Connector for NativeConnector {
    fn connect<'a>(
        &'a self,
        endpoint: &'a str,
        headers: &'a Headers,
        timeout: Option<Duration>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ConnectedSocket, WebSocketError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(WebSocketError::Cancelled);
            }
            let operation = async {
                tokio::select! { biased;
                    _ = cancellation.cancelled() => Err(WebSocketError::Cancelled),
                    _ = self.client.closed() => Err(WebSocketError::Disposed),
                    result = self.upgrade(endpoint, headers) => result,
                }
            };
            match timeout.filter(|timeout| !timeout.is_zero()) {
                Some(timeout) => tokio::time::timeout(timeout, operation)
                    .await
                    .unwrap_or_else(|_| {
                        if cancellation.is_cancelled() {
                            Err(WebSocketError::Cancelled)
                        } else {
                            Err(WebSocketError::Transport(format!(
                                "provider: websocket upgrade did not return headers within {}",
                                time_span(timeout)
                            )))
                        }
                    }),
                None => operation.await,
            }
        })
    }
}
fn time_span(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let days = seconds / 86400;
    let mut value = if days == 0 {
        String::new()
    } else {
        format!("{days}.")
    };
    value.push_str(&format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60
    ));
    let ticks = duration.subsec_nanos() / 100;
    if ticks != 0 {
        value.push_str(&format!(".{ticks:07}"));
    }
    value
}

fn has_token(headers: &reqwest::header::HeaderMap, name: &str, token: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|value| value.trim().eq_ignore_ascii_case(token))
}
fn random(buffer: &mut [u8]) -> Result<(), WebSocketError> {
    getrandom::getrandom(buffer).map_err(|error| WebSocketError::Transport(error.to_string()))
}
fn protocol(message: &str) -> WebSocketError {
    WebSocketError::Socket {
        code: SocketErrorCode::Faulted,
        message: format!("provider: invalid websocket frame: {message}"),
    }
}
fn io_error(error: std::io::Error) -> WebSocketError {
    WebSocketError::Socket {
        code: SocketErrorCode::ConnectionClosedPrematurely,
        message: error.to_string(),
    }
}

#[derive(Default)]
struct Utf8 {
    tail: [u8; 4],
    count: usize,
}
impl Utf8 {
    fn push(&mut self, mut bytes: &[u8], final_chunk: bool) -> Result<(), WebSocketError> {
        if self.count > 0 {
            while self.count < 4 && !bytes.is_empty() {
                self.tail[self.count] = bytes[0];
                self.count += 1;
                bytes = &bytes[1..];
                match std::str::from_utf8(&self.tail[..self.count]) {
                    Ok(_) => {
                        self.count = 0;
                        break;
                    }
                    Err(error) if error.error_len().is_none() => {}
                    Err(_) => return Err(protocol("invalid UTF-8")),
                }
            }
        }
        if self.count == 0 {
            match std::str::from_utf8(bytes) {
                Ok(_) => {}
                Err(error) if error.error_len().is_none() => {
                    let tail = &bytes[error.valid_up_to()..];
                    self.tail[..tail.len()].copy_from_slice(tail);
                    self.count = tail.len();
                }
                Err(_) => return Err(protocol("invalid UTF-8")),
            }
        }
        if final_chunk && self.count != 0 {
            return Err(protocol("incomplete UTF-8"));
        }
        Ok(())
    }
}
struct DataFrame {
    remaining: u64,
    kind: FrameKind,
    final_frame: bool,
}
struct NativeSocket {
    io: Option<reqwest::Upgraded>,
    state: SocketState,
    incoming: Option<DataFrame>,
    fragmented: Option<FrameKind>,
    outgoing_fragmented: bool,
    utf8: Utf8,
}
impl NativeSocket {
    fn new(io: reqwest::Upgraded) -> Self {
        Self {
            io: Some(io),
            state: SocketState::Open,
            incoming: None,
            fragmented: None,
            outgoing_fragmented: false,
            utf8: Utf8::default(),
        }
    }
    async fn write_frame(
        &mut self,
        opcode: u8,
        final_frame: bool,
        data: &[u8],
    ) -> Result<(), WebSocketError> {
        let mut frame = if opcode == 10 {
            Frame::pong(data.to_vec())
        } else {
            Frame::message(
                data.to_vec(),
                OpCode::Data(if opcode == 0 {
                    Data::Continue
                } else {
                    Data::Text
                }),
                final_frame,
            )
        };
        let mut mask = [0; 4];
        random(&mut mask)?;
        frame.header_mut().mask = Some(mask);
        let mut encoded = Vec::with_capacity(frame.len());
        frame
            .format(&mut encoded)
            .map_err(|error| protocol(&error.to_string()))?;
        let io = self.io.as_mut().ok_or(WebSocketError::Disposed)?;
        io.write_all(&encoded).await.map_err(io_error)?;
        io.flush().await.map_err(io_error)
    }
    async fn header(&mut self) -> Result<Option<ReceivedFrame>, WebSocketError> {
        let io = self.io.as_mut().ok_or(WebSocketError::Disposed)?;
        // Parse only the bounded header. Payload receipt belongs to the caller's
        // receive unit, not tungstenite's whole-frame/message receive APIs.
        let mut bytes = [0; 14];
        let mut used = 0;
        let (header, length) = loop {
            let mut cursor = Cursor::new(&bytes[..used]);
            if let Some(header) =
                FrameHeader::parse(&mut cursor).map_err(|error| protocol(&error.to_string()))?
            {
                break header;
            }
            if used == bytes.len() {
                return Err(protocol("oversized header"));
            }
            io.read_exact(&mut bytes[used..used + 1])
                .await
                .map_err(io_error)?;
            used += 1;
        };
        if header.rsv1 || header.rsv2 || header.rsv3 {
            return Err(protocol("reserved bits"));
        }
        if header.mask.is_some() {
            return Err(protocol("masked server frame"));
        }
        let opcode: u8 = header.opcode.into();
        let final_frame = header.is_final;
        let marker = bytes[1] & 0x7f;
        if (marker == 126 && length < 126) || (marker == 127 && length < 65536) {
            return Err(protocol("nonminimal length"));
        }
        if length >> 63 != 0 {
            return Err(protocol("invalid extended length"));
        }
        if opcode >= 8 {
            if !final_frame || length > 125 {
                return Err(protocol("invalid control frame"));
            }
            if !matches!(opcode, 8..=10) {
                return Err(protocol("reserved opcode"));
            }
            let mut payload = [0; 125];
            io.read_exact(&mut payload[..length as usize])
                .await
                .map_err(io_error)?;
            let payload = &payload[..length as usize];
            if opcode == 9 {
                self.write_frame(10, true, payload).await?;
            }
            if opcode == 8 {
                if length == 1 {
                    return Err(protocol("invalid close payload"));
                }
                let (status, reason) = if payload.is_empty() {
                    (Some(1000), Some(String::new()))
                } else {
                    let status = u16::from_be_bytes([payload[0], payload[1]]);
                    if !(matches!(status, 1000..=1003 | 1007..=1014 | 3000..=4999)) {
                        return Err(protocol("invalid close status"));
                    }
                    let reason = std::str::from_utf8(&payload[2..])
                        .map_err(|_| protocol("invalid close reason"))?
                        .to_owned();
                    (Some(status), Some(reason))
                };
                self.state = SocketState::Closed;
                return Ok(Some(ReceivedFrame {
                    count: 0,
                    kind: FrameKind::Close,
                    end_of_message: true,
                    close_status: status,
                    close_description: reason,
                }));
            }
            return Ok(None);
        }
        let kind = match opcode {
            0 => self
                .fragmented
                .ok_or_else(|| protocol("unexpected continuation"))?,
            1 | 2 => {
                if self.fragmented.is_some() {
                    return Err(protocol("interleaved data messages"));
                }
                if opcode == 1 {
                    self.utf8 = Utf8::default();
                    FrameKind::Text
                } else {
                    FrameKind::Binary
                }
            }
            _ => return Err(protocol("reserved opcode")),
        };
        self.fragmented = if final_frame { None } else { Some(kind) };
        self.incoming = Some(DataFrame {
            remaining: length,
            kind,
            final_frame,
        });
        Ok(None)
    }
    async fn receive_inner(&mut self, buffer: &mut [u8]) -> Result<ReceivedFrame, WebSocketError> {
        loop {
            if self.incoming.is_none() {
                if let Some(close) = self.header().await? {
                    return Ok(close);
                }
                if self.incoming.is_none() {
                    continue;
                }
            }
            let frame = self.incoming.as_mut().expect("data header");
            let limit = frame.remaining.min(buffer.len() as u64) as usize;
            // ClientWebSocket completes at the frame boundary or fills the
            // supplied buffer. Partial internal TCP reads share this operation's
            // outer idle deadline and are not counted as successful receives.
            self.io
                .as_mut()
                .ok_or(WebSocketError::Disposed)?
                .read_exact(&mut buffer[..limit])
                .await
                .map_err(io_error)?;
            let count = limit;
            frame.remaining -= count as u64;
            let exhausted = frame.remaining == 0;
            let end = exhausted && frame.final_frame;
            let kind = frame.kind;
            if kind == FrameKind::Text {
                self.utf8.push(&buffer[..count], end)?;
            }
            if exhausted {
                self.incoming = None;
            }
            return Ok(ReceivedFrame {
                count,
                kind,
                end_of_message: end,
                close_status: None,
                close_description: None,
            });
        }
    }
}
/// Dropping an in-flight read/write must not leave a partially consumed frame reusable.
struct Operation<'a> {
    socket: &'a mut NativeSocket,
    completed: bool,
}
impl Drop for Operation<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.socket.abort();
        }
    }
}
impl Socket for NativeSocket {
    fn state(&self) -> SocketState {
        self.state
    }
    fn send<'a>(
        &'a mut self,
        data: &'a [u8],
        end_of_message: bool,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), WebSocketError>> {
        Box::pin(async move {
            if self.state != SocketState::Open {
                return Err(protocol("socket is not open"));
            }
            let mut operation = Operation {
                socket: self,
                completed: false,
            };
            let opcode = if operation.socket.outgoing_fragmented {
                0
            } else {
                1
            };
            let result = tokio::select! { biased;
                _ = cancellation.cancelled() => Err(WebSocketError::Cancelled),
                result = operation.socket.write_frame(opcode, end_of_message, data) => result,
            };
            if result.is_ok() {
                operation.socket.outgoing_fragmented = !end_of_message;
                operation.completed = true;
            }
            result
        })
    }
    fn receive<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ReceivedFrame, WebSocketError>> {
        Box::pin(async move {
            if self.state != SocketState::Open {
                return Err(protocol("socket is not open"));
            }
            if buffer.is_empty() {
                return Err(WebSocketError::InvalidRequest(
                    "provider: websocket receive buffer is empty".into(),
                ));
            }
            let mut operation = Operation {
                socket: self,
                completed: false,
            };
            let result = tokio::select! { biased;
                _ = cancellation.cancelled() => Err(WebSocketError::Cancelled),
                result = operation.socket.receive_inner(buffer) => result,
            };
            if result.is_ok() {
                operation.completed = true;
            }
            result
        })
    }
    fn abort(&mut self) {
        self.io.take();
        self.state = SocketState::Aborted;
    }
    fn dispose(&mut self) {
        self.io.take();
        self.state = SocketState::Closed;
    }
}

#[cfg(test)]
mod tests {
    use super::time_span;
    use std::time::Duration;
    #[test]
    fn timeout_uses_source_timespan_display() {
        for (duration, expected) in [
            (Duration::ZERO, "00:00:00"),
            (Duration::from_millis(20), "00:00:00.0200000"),
            (Duration::from_millis(1250), "00:00:01.2500000"),
            (Duration::from_secs(90061), "1.01:01:01"),
            (Duration::from_nanos(100), "00:00:00.0000001"),
        ] {
            assert_eq!(time_span(duration), expected);
        }
    }
}
