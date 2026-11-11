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
    pub fn user_away() -> Self {
        Self {
            kind: QuestionReplyKind::UserAway,
            answers: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentAskReply {
    Replied(String),
    TimedOut,
}

/// Caller-scoped user questioning. The host rejects non-root invoking agents.
pub trait QuestionRequester: Send + Sync {
    type Error: Send + Sync + 'static;

    /// Waits for the user's answers without routing to another agent.
    fn ask<'a>(
        &'a self,
        questions: Vec<QuestionDefinition>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<QuestionReply, OperationError<Self::Error>>>;
}

/// Caller-scoped plain-text communication with agents in the same user session.
pub trait AgentCommunication: Send + Sync {
    type Error: Send + Sync + 'static;

    /// Resolves an agent path and waits for its reply, timeout or cancellation.
    fn ask<'a>(
        &'a self,
        name: &'a str,
        message: String,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<AgentAskReply, OperationError<Self::Error>>>;

    /// Immediately settles an ask addressed to this agent; stale IDs are errors.
    fn reply(&self, request_id: &str, message: String) -> Result<(), Self::Error>;
}
