use crate::{http_stream::HttpError, responses_websocket::WebSocketError};
use std::fmt;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Error {
    Http(HttpError),
    Authentication(parrot_foundation::oauth::AuthError),
    WebSocketTransport {
        message: String,
        cause: Box<Error>,
    },
    Socket(WebSocketError),
    Provider(String),
    ProviderWithCause {
        message: String,
        cause: Box<Error>,
    },
    HeaderRetryLimit {
        maximum_retries: u32,
        attempts: u64,
        cause: Box<Error>,
    },
    Response(crate::responses::ResponsesError),
    ChatProvider(crate::chat_completions::ProviderFailure),
    Wire(String),
    Encode(String),
    Cancelled,
    Io(String),
    InvalidOperation(String),
    Disposed,
}
impl Error {
    /// Borrows the top-level provider response body, or returns an empty string.
    ///
    /// Exception causes are not inspected. The returned body is not redacted and
    /// must not be added to diagnostic metadata or logs implicitly.
    pub fn response_body(&self) -> &str {
        match self {
            Self::Http(HttpError::Response(error)) => &error.response_body,
            Self::Response(crate::responses::ResponsesError::ProviderResponse {
                response_body,
                ..
            })
            | Self::Socket(WebSocketError::Response(
                crate::responses::ResponsesError::ProviderResponse { response_body, .. },
            )) => response_body,
            Self::ChatProvider(error) => &error.response_body,
            _ => "",
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProviderWithCause { message, .. } | Self::WebSocketTransport { message, .. } => {
                f.write_str(message)
            }
            Self::Http(error) => error.fmt(f),
            Self::Authentication(error) => error.fmt(f),
            Self::Socket(error) => error.fmt(f),
            Self::Response(error) => error.fmt(f),
            Self::HeaderRetryLimit {
                maximum_retries,
                attempts,
                ..
            } => write!(
                f,
                "Provider header timeout retry limit exceeded ({maximum_retries} retries, {attempts} attempts)."
            ),
            Self::ChatProvider(error) => {
                let message = if error.message.is_empty() {
                    "provider error"
                } else {
                    &error.message
                };
                f.write_str(message)?;
                if !error.error_type.is_empty() {
                    write!(f, " ({})", error.error_type)?;
                }
                if !error.error_code.is_empty() {
                    write!(f, " [{}]", error.error_code)?;
                }
                Ok(())
            }
            Self::Provider(message)
            | Self::Wire(message)
            | Self::Encode(message)
            | Self::Io(message)
            | Self::InvalidOperation(message) => f.write_str(message),
            Self::Cancelled => f.write_str("The operation was canceled."),
            Self::Disposed => f.write_str("Cannot access a disposed provider session."),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::HeaderRetryLimit { cause, .. }
            | Self::ProviderWithCause { cause, .. }
            | Self::WebSocketTransport { cause, .. } => Some(cause.as_ref()),
            Self::Http(error) => Some(error),
            Self::Authentication(error) => Some(error),
            Self::Socket(error) => Some(error),
            Self::Response(error) => Some(error),
            _ => None,
        }
    }
}
impl From<HttpError> for Error {
    fn from(error: HttpError) -> Self {
        match error {
            HttpError::Cancelled => Self::Cancelled,
            error => Self::Http(error),
        }
    }
}
impl From<WebSocketError> for Error {
    fn from(error: WebSocketError) -> Self {
        Self::Socket(error)
    }
}

impl From<parrot_foundation::oauth::AuthError> for Error {
    fn from(error: parrot_foundation::oauth::AuthError) -> Self {
        Self::Authentication(error)
    }
}
impl From<parrot_domain::contracts::async_ops::OperationError<parrot_foundation::oauth::AuthError>>
    for Error
{
    fn from(
        error: parrot_domain::contracts::async_ops::OperationError<
            parrot_foundation::oauth::AuthError,
        >,
    ) -> Self {
        match error {
            parrot_domain::contracts::async_ops::OperationError::Cancelled => Self::Cancelled,
            parrot_domain::contracts::async_ops::OperationError::Failed(error) => {
                Self::Authentication(error)
            }
        }
    }
}

impl Error {
    pub(crate) fn wrap_websocket_transport(self) -> Self {
        let preserved = matches!(
            &self,
            Error::Cancelled
                | Error::Provider(_)
                | Error::ProviderWithCause { .. }
                | Error::WebSocketTransport { .. }
                | Error::ChatProvider(_)
                | Error::Response(crate::responses::ResponsesError::ProviderResponse { .. })
                | Error::Http(
                    crate::http_stream::HttpError::Cancelled
                        | crate::http_stream::HttpError::Provider(_)
                        | crate::http_stream::HttpError::RequestTooLarge(_)
                )
                | Error::Socket(
                    WebSocketError::Cancelled
                        | WebSocketError::RequestTooLarge { .. }
                        | WebSocketError::TransportFailure { .. }
                        | WebSocketError::Transport(_)
                        | WebSocketError::Upgrade { .. }
                        | WebSocketError::Response(
                            crate::responses::ResponsesError::ProviderResponse { .. }
                        )
                )
        );
        if preserved {
            self
        } else {
            Self::WebSocketTransport {
                message: "Responses WebSocket transport failed.".into(),
                cause: Box::new(self),
            }
        }
    }
}
