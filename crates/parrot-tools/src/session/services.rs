use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationResult},
        security::AccessSnapshot,
    },
    values::context::{ContextSize, ContextSnapshot},
};
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureKind {
    Json,
    Format,
    Argument,
    InputConflict,
    Io,
    Unauthorized,
    Permission,
    InvalidOperation,
    Provider,
    Wire,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Compaction {
    pub reduced: bool,
    pub context: ContextSnapshot,
}

/// One retained agent's source operations. The production adapter must own and
/// join admitted mutations and route inline compaction through the current
/// invocation, not queue it behind the drain that is awaiting this tool.
pub trait SessionServices: Send + Sync {
    type Security: AccessSnapshot + 'static;
    type Selection: Send + Sync + 'static;

    fn checkpoint<'a>(
        &'a self,
        title: &'a str,
        assistant_sequence: i64,
        call_id: &'a str,
    ) -> BoxFuture<'a, Result<(), Failure>>;

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

    fn compact<'a>(
        &'a self,
        selection: &'a Self::Selection,
        target: Option<ContextSize>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Compaction, Failure>>;

    fn estimate_after_tool_result(
        &self,
        selection: &Self::Selection,
        call_id: &str,
        rendered_result: &str,
    ) -> Result<ContextSnapshot, Failure>;
}
