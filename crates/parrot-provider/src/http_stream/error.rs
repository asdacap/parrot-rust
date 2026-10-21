use std::{fmt, time::Duration};

/// Structured non-success HTTP response, retained for provider classifiers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpFailure {
    pub status: u16,
    pub error_type: String,
    pub error_code: String,
    pub detail: String,
    pub response_body: String,
}

impl fmt::Display for HttpFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "provider request failed with HTTP {}", self.status)?;
        if !self.error_type.is_empty() {
            write!(f, " ({})", self.error_type)?;
        }
        if !self.error_code.is_empty() {
            write!(f, " [{}]", self.error_code)?;
        }
        if !self.detail.is_empty() {
            write!(f, ": {}", self.detail)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HttpError {
    Cancelled,
    Http(String),
    /// Provider policy/bounded-HTTP failure without a response status.
    ProviderHttp(String),
    /// Request-transport cancellation with its actual transport-provided message.
    RequestCancelled(String),
    Provider(String),
    Response(HttpFailure),
    Wire(String),
    InvalidJsonString(String),
    RequestTooLarge(usize),
    HeaderTimeout(Duration),
    Transport(String),
    Disposed,
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("operation cancelled"),
            Self::Http(message)
            | Self::ProviderHttp(message)
            | Self::RequestCancelled(message)
            | Self::Provider(message)
            | Self::InvalidJsonString(message)
            | Self::Wire(message)
            | Self::Transport(message) => f.write_str(message),
            Self::Response(error) => error.fmt(f),
            Self::RequestTooLarge(limit) => write!(f, "provider: request exceeds {limit} bytes"),
            Self::HeaderTimeout(timeout) => {
                let seconds = timeout.as_secs();
                write!(f, "provider: response headers timed out after ")?;
                if seconds >= 86400 {
                    write!(f, "{}.", seconds / 86400)?;
                }
                write!(
                    f,
                    "{:02}:{:02}:{:02}",
                    (seconds / 3600) % 24,
                    (seconds / 60) % 60,
                    seconds % 60
                )?;
                let ticks = timeout.subsec_nanos() / 100;
                if ticks != 0 {
                    write!(f, ".{ticks:07}")?;
                }
                Ok(())
            }
            Self::Disposed => f.write_str("HTTP client has been disposed"),
        }
    }
}

impl std::error::Error for HttpError {}
