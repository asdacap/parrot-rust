use parrot_core::{CompactionFailure, PlanningError};
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::messages::parrot::CompactionProgressEvent,
};
use parrot_provider::sessions::{self, Event};
use std::fmt;

/// Publishes provider-supplied retry and progress observations before execution continues.
pub trait CompactionObserver: Send + Sync {
    /// Publishes a summary-provider retry without consuming further provider events.
    fn observe<'a>(
        &'a self,
        event: &'a Event,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), sessions::Error>>;

    /// Publishes aggregate progress without exposing generated summary content.
    fn observe_progress<'a>(
        &'a self,
        progress: CompactionProgressEvent,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), sessions::Error>>;
}

#[derive(Debug)]
pub enum CompactionError {
    Planning(PlanningError),
    Provider(sessions::Error),
    Core(parrot_core::Error),
    Template(String),
    InvalidOperation(String),
    Cancelled,
}

impl CompactionError {
    pub fn response_body(&self) -> &str {
        match self {
            Self::Provider(error) => error.response_body(),
            Self::Core(error) => &error.response_body,
            _ => "",
        }
    }
}

impl fmt::Display for CompactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Planning(error) => error.fmt(formatter),
            Self::Provider(error) => error.fmt(formatter),
            Self::Core(error) => error.fmt(formatter),
            Self::Template(message) | Self::InvalidOperation(message) => {
                formatter.write_str(message)
            }
            Self::Cancelled => formatter.write_str("The operation was canceled."),
        }
    }
}

impl std::error::Error for CompactionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Planning(error) => Some(error),
            Self::Provider(error) => Some(error),
            Self::Core(error) => Some(error),
            _ => None,
        }
    }
}

impl From<PlanningError> for CompactionError {
    fn from(error: PlanningError) -> Self {
        Self::Planning(error)
    }
}

impl From<sessions::Error> for CompactionError {
    fn from(error: sessions::Error) -> Self {
        Self::Provider(error)
    }
}

impl From<CompactionFailure> for CompactionError {
    fn from(failure: CompactionFailure) -> Self {
        match failure {
            CompactionFailure::Cancelled => Self::Cancelled,
            CompactionFailure::GroupExceedsBudget => {
                Self::Planning(PlanningError::GroupExceedsBudget)
            }
            CompactionFailure::NoSummary(reason) => Self::InvalidOperation(reason),
            _ => Self::InvalidOperation(failure.to_string()),
        }
    }
}

impl From<parrot_core::Error> for CompactionError {
    fn from(error: parrot_core::Error) -> Self {
        use parrot_core::{ErrorDetail, ErrorKind};
        match error.detail {
            Some(ErrorDetail::Planning(error)) => error.into(),
            Some(ErrorDetail::Compaction(error)) => error.into(),
            Some(ErrorDetail::Template) => Self::Template(error.message),
            None if error.kind == ErrorKind::Cancelled => Self::Cancelled,
            None => Self::Core(error),
        }
    }
}

pub(crate) struct NativeErrors<E> {
    original: std::sync::Mutex<Option<E>>,
}

impl<E> NativeErrors<E> {
    pub(crate) fn new() -> Self {
        Self {
            original: std::sync::Mutex::new(None),
        }
    }

    pub(crate) fn capture(&self, error: E, diagnostic: parrot_core::Error) -> parrot_core::Error {
        *self.original.lock().expect("compaction errors poisoned") = Some(error);
        diagnostic
    }

    pub(crate) fn restore(
        &self,
        error: parrot_core::Error,
        convert: impl FnOnce(parrot_core::Error) -> E,
    ) -> E {
        self.original
            .lock()
            .expect("compaction errors poisoned")
            .take()
            .unwrap_or_else(|| convert(error))
    }
}

pub(crate) fn describe_error(error: &CompactionError) -> parrot_core::Error {
    use parrot_core::{Error, ErrorCode};
    let mut diagnostic = match error {
        CompactionError::Core(error) => error.clone(),
        CompactionError::Planning(error) => Error::from(*error),
        CompactionError::Cancelled => Error::cancelled(),
        CompactionError::InvalidOperation(message) => Error {
            code: ErrorCode::InvalidOperation,
            ..Error::failed(message.clone())
        },
        _ => Error::failed(error.to_string()),
    };
    diagnostic.response_body = error.response_body().to_owned();
    diagnostic
}
