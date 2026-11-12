use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationResult},
        security::AccessSnapshot,
    },
    values::progress::ProgressUpdate,
};
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureKind {
    Json,
    Format,
    Argument,
    Io,
    Unauthorized,
    Permission,
    InvalidOperation,
    Provider,
    Authentication,
    Network,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Failure {
    pub kind: FailureKind,
    pub message: String,
}

impl Failure {
    pub fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}
impl std::error::Error for Failure {}

/// One retained agent's source operations. The production adapter owns and joins admitted mutations.
pub trait SessionServices: Send + Sync {
    type Security: AccessSnapshot + 'static;
    type Selection: Send + Sync + 'static;

    fn set_reminder<'a>(
        &'a self,
        title: &'a str,
        description: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), Failure>>;

    fn clear_reminder<'a>(
        &'a self,
        title: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<bool, Failure>>;

    /// Replaces or explicitly closes this agent's progress. False means it was
    /// already closed; admitted durable mutations must be retained and joined.
    fn update_progress<'a>(
        &'a self,
        update: ProgressUpdate,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<bool, Failure>>;

    fn reminder_titles(&self) -> BoxFuture<'_, Vec<String>>;

    fn runtime_status<'a>(
        &'a self,
        selection: &'a Self::Selection,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<String, Failure>>;

    fn statistics_status<'a>(
        &'a self,
        selection: &'a Self::Selection,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<String, Failure>>;
}
