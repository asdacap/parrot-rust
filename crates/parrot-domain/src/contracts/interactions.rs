//! Distinct per-user-session brokers supplied by the host, never globals.
//!
//! Wrong or stale requests must return explicit errors, never plausible replies
//! or successful no-ops.

use super::async_ops::{BoxFuture, Cancellation, OperationError};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuestionDefinition {
    pub header: String,
    pub prompt: String,
    pub options: Vec<QuestionOption>,
    pub multiple: bool,
    pub custom: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuestionReplyKind {
    Answered,
    UserAway,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuestionReply {
    pub kind: QuestionReplyKind,
    pub answers: Vec<String>,
}

impl QuestionReply {
    pub fn answered(answers: Vec<String>) -> Self {
        Self {
            kind: QuestionReplyKind::Answered,
            answers,
        }
    }

    pub fn user_away() -> Self {
        Self {
            kind: QuestionReplyKind::UserAway,
            answers: Vec::new(),
        }
    }
}

/// Already scoped to the actual invoking agent by the host. The tool cannot
/// choose a child identity or parent scope; this is not the user-session broker.
pub trait QuestionRequester: Send + Sync {
    type Error: Send + Sync + 'static;

    fn ask<'a>(
        &'a self,
        questions: Vec<QuestionDefinition>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<QuestionReply, OperationError<Self::Error>>>;
}

/// Caller-scoped plain-text communication with agents in the same user session.
pub trait AgentCommunication: Send + Sync {
    type Error: Send + Sync + 'static;

    /// Resolves an agent path and waits for its reply or cancellation.
    fn ask<'a>(
        &'a self,
        name: &'a str,
        message: String,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<String, OperationError<Self::Error>>>;

    /// Immediately settles an ask addressed to this agent; stale IDs are errors.
    fn reply(&self, request_id: &str, message: String) -> Result<(), Self::Error>;
}

pub trait QuestionBroker: Send + Sync {
    type Request: Send + 'static;
    type Reply: Send + 'static;
    type Pending: Send + 'static;
    type RequestId: Send + Sync + 'static;
    type Error: Send + Sync + 'static;

    /// Registers and awaits a question within this broker's session scope.
    fn ask<'a>(
        &'a self,
        request: Self::Request,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Self::Reply, OperationError<Self::Error>>>;

    fn pending(&self) -> Result<Self::Pending, Self::Error>;
    fn reply(&self, id: &Self::RequestId, reply: Self::Reply) -> Result<(), Self::Error>;

    /// Rejects a pending question explicitly; wrong/stale IDs are errors.
    fn reject(&self, id: &Self::RequestId) -> Result<(), Self::Error>;

    /// Removes a caller-withdrawn question and settles its waiter.
    fn withdraw(&self, id: &Self::RequestId) -> Result<(), Self::Error>;
}

pub trait PermissionBroker: Send + Sync {
    type Request: Send + 'static;
    type Reply: Send + 'static;
    type Pending: Send + 'static;
    type RequestId: Send + Sync + 'static;
    type Error: Send + Sync + 'static;

    /// Registers and awaits permission; cancellation cannot fabricate approval.
    fn request<'a>(
        &'a self,
        request: Self::Request,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Self::Reply, OperationError<Self::Error>>>;

    fn pending(&self) -> BoxFuture<'_, Result<Self::Pending, Self::Error>>;
    fn reply<'a>(
        &'a self,
        id: &'a Self::RequestId,
        reply: Self::Reply,
    ) -> BoxFuture<'a, Result<(), Self::Error>>;

    /// Rejects a pending permission explicitly; wrong/stale IDs are errors.
    fn reject<'a>(&'a self, id: &'a Self::RequestId) -> BoxFuture<'a, Result<(), Self::Error>>;

    /// Removes a caller-withdrawn permission request and settles its waiter.
    fn withdraw<'a>(&'a self, id: &'a Self::RequestId) -> BoxFuture<'a, Result<(), Self::Error>>;
}
