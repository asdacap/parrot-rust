use super::compaction::{CompactionFailure, PlanningError};
use std::{fmt, io};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    Failed,
    Cancelled,
    RequestTooLarge,
}

/// Bounded diagnostic metadata, separate from error messages and provider bodies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorCode {
    Cancelled,
    AccessDenied,
    Io,
    Timeout,
    Http,
    Argument,
    InvalidOperation,
    WebSocketClosed,
    WebSocket,
    WireProtocol,
    WebSocketUpgrade,
    WebSocketMessageTooLarge,
    RequestTooLarge,
    WebSocketTransport,
    Unexpected,
}

impl ErrorCode {
    pub fn code(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::AccessDenied => "access_denied",
            Self::Io => "io",
            Self::Timeout => "timeout",
            Self::Http => "http",
            Self::Argument => "argument",
            Self::InvalidOperation => "invalid_operation",
            Self::WebSocketClosed => "websocket_closed",
            Self::WebSocket => "websocket",
            Self::WireProtocol => "wire_protocol",
            Self::WebSocketUpgrade => "websocket_upgrade",
            Self::WebSocketMessageTooLarge => "websocket_message_too_large",
            Self::RequestTooLarge => "request_too_large",
            Self::WebSocketTransport => "websocket_transport",
            Self::Unexpected => "unexpected",
        }
    }

    /// Classifies a filesystem or socket failure that carries no provider detail.
    pub fn from_io(error: &io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::PermissionDenied => Self::AccessDenied,
            io::ErrorKind::InvalidInput => Self::Argument,
            _ => Self::Io,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Error {
    pub kind: ErrorKind,
    pub code: ErrorCode,
    pub message: String,
    pub response_body: String,
    pub detail: Option<ErrorDetail>,
}

impl Error {
    pub fn template(message: impl Into<String>) -> Self {
        Self {
            detail: Some(ErrorDetail::Template),
            ..Self::failed(message)
        }
    }

    pub fn prompt_context(message: impl Into<String>) -> Self {
        Self {
            detail: Some(ErrorDetail::PromptContext),
            ..Self::failed(message)
        }
    }

    pub fn unsupported(operation: &str) -> Self {
        Self {
            code: ErrorCode::InvalidOperation,
            ..Self::failed(format!("{operation} is not supported"))
        }
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Failed,
            code: ErrorCode::Unexpected,
            message: message.into(),
            response_body: String::new(),
            detail: None,
        }
    }

    pub fn request_too_large(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::RequestTooLarge,
            code: ErrorCode::RequestTooLarge,
            message: message.into(),
            response_body: String::new(),
            detail: None,
        }
    }

    pub fn cancelled() -> Self {
        Self {
            kind: ErrorKind::Cancelled,
            code: ErrorCode::Cancelled,
            message: "The operation was canceled.".into(),
            response_body: String::new(),
            detail: None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ErrorDetail {
    Template,
    PromptContext,
    Planning(PlanningError),
    Compaction(CompactionFailure),
}
