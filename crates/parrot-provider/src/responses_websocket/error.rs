use crate::responses::ResponsesError;
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocketErrorCode {
    NotAWebSocket,
    ConnectionClosedPrematurely,
    Faulted,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WebSocketError {
    Cancelled,
    Upgrade {
        status: u16,
        message: String,
    },
    Transport(String),
    TransportFailure {
        message: String,
        cause: Box<WebSocketError>,
    },
    Socket {
        code: SocketErrorCode,
        message: String,
    },
    Wire(String),
    RequestTooLarge {
        maximum: usize,
    },
    MessageTooLarge,
    Response(ResponsesError),
    InvalidRequest(String),
    InvalidOperation(String),
    Disposed,
}
impl fmt::Display for WebSocketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("A task was canceled."),
            Self::Upgrade { message, .. }
            | Self::TransportFailure { message, .. }
            | Self::Socket { message, .. }
            | Self::Transport(message)
            | Self::Wire(message)
            | Self::InvalidRequest(message)
            | Self::InvalidOperation(message) => f.write_str(message),
            Self::RequestTooLarge { maximum } => {
                write!(f, "provider: request exceeds {maximum} bytes")
            }
            Self::MessageTooLarge => write!(
                f,
                "WebSocket request exceeds {} bytes.",
                super::MAXIMUM_MESSAGE_BYTES
            ),
            Self::Response(error) => error.fmt(f),
            Self::Disposed => f.write_str("Cannot access a disposed provider session."),
        }
    }
}
impl std::error::Error for WebSocketError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TransportFailure { cause, .. } => Some(cause.as_ref()),
            Self::Response(error) => Some(error),
            _ => None,
        }
    }
}
impl From<ResponsesError> for WebSocketError {
    fn from(error: ResponsesError) -> Self {
        Self::Response(error)
    }
}
