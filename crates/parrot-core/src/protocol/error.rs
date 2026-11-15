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
    Planning(PlanningError),
    Compaction(CompactionFailure),
}

impl fmt::Display for CompactionFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Cancelled => "The operation was canceled.",
            Self::GroupExceedsBudget => {
                "A complete conversation group exceeds the compaction input budget."
            }
            Self::InputLimitExceeded | Self::FittedInputLimitExceeded => {
                "The compacted conversation exceeds the selected model input limit."
            }
            Self::NoSafeBoundary => {
                "The conversation has no safe compaction boundary before the input limit."
            }
            Self::NoSummary(reason) => reason,
        })
    }
}

impl std::error::Error for CompactionFailure {}

impl From<CompactionFailure> for Error {
    fn from(failure: CompactionFailure) -> Self {
        if failure == CompactionFailure::Cancelled {
            return Self::cancelled();
        }
        let message = failure.to_string();
        Self {
            code: ErrorCode::InvalidOperation,
            detail: Some(ErrorDetail::Compaction(failure)),
            ..Self::failed(message)
        }
    }
}

impl fmt::Display for PlanningError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NoCompactionCapacity => {
                "The selected model leaves no room for a compaction request."
            }
            Self::GroupExceedsBudget => {
                "A complete conversation group exceeds the compaction input budget."
            }
            Self::TokenOverflow => "Token estimate overflow.",
        })
    }
}

impl std::error::Error for PlanningError {}

impl From<PlanningError> for Error {
    fn from(error: PlanningError) -> Self {
        Self {
            code: if error == PlanningError::TokenOverflow {
                ErrorCode::Unexpected
            } else {
                ErrorCode::InvalidOperation
            },
            detail: Some(ErrorDetail::Planning(error)),
            ..Self::failed(error.to_string())
        }
    }
}
