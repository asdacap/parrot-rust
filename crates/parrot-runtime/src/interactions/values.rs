use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, Cancelled, Clock};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InteractionErrorKind {
    Question,
    QuestionNotFound,
    QuestionRejected,
    Permission,
    PermissionNotFound,
    Disposed,
    Registry,
    Delivery,
    InvalidTimeout,
    Identifier,
    Argument,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InteractionError {
    pub kind: InteractionErrorKind,
    pub message: String,
}

impl InteractionError {
    pub fn new(kind: InteractionErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn question(message: impl Into<String>) -> Self {
        Self::new(InteractionErrorKind::Question, message)
    }

    pub(crate) fn rejected(message: impl Into<String>) -> Self {
        Self::new(InteractionErrorKind::QuestionRejected, message)
    }

    pub(crate) fn permission(message: impl Into<String>) -> Self {
        Self::new(InteractionErrorKind::Permission, message)
    }
}

impl fmt::Display for InteractionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for InteractionError {}

pub use parrot_domain::contracts::interactions::{
    QuestionDefinition, QuestionOption, QuestionReply, QuestionReplyKind,
};

pub fn validate_questions(questions: &[QuestionDefinition]) -> Result<(), InteractionError> {
    if !(1..=32).contains(&questions.len()) {
        return Err(InteractionError::question(
            "question requests require between 1 and 32 questions",
        ));
    }
    for question in questions {
        if question.prompt.trim().is_empty() {
            return Err(InteractionError::question(
                "question prompts cannot be empty",
            ));
        }
        let mut options = HashSet::new();
        for option in &question.options {
            if option.label.trim().is_empty() {
                return Err(InteractionError::question(
                    "question options cannot be empty",
                ));
            }
            if !options.insert(&option.label) {
                return Err(InteractionError::question(format!(
                    "duplicate question option: {}",
                    option.label
                )));
            }
        }
        if question.options.is_empty() && !question.custom {
            return Err(InteractionError::question(
                "question requires options or custom answers",
            ));
        }
    }
    Ok(())
}

pub fn validate_reply(
    questions: &[QuestionDefinition],
    reply: &QuestionReply,
) -> Result<(), InteractionError> {
    if reply.kind != QuestionReplyKind::Answered {
        return Err(InteractionError::question(
            "question replies require user answers",
        ));
    }
    if questions.len() != reply.answers.len() {
        return Err(InteractionError::question(
            "question replies must contain one answer for every question in order",
        ));
    }
    if reply.answers.iter().any(|answer| answer.trim().is_empty()) {
        return Err(InteractionError::question(
            "question answers cannot be empty",
        ));
    }
    Ok(())
}

/// Safe metadata only: question text, answers, paths, and permission reasons are
/// deliberately not part of the diagnostic event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticSeverity {
    Information,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrokerDiagnostic {
    pub severity: DiagnosticSeverity,
    pub category: &'static str,
    pub event: &'static str,
    pub correlation_id: String,
    pub agent_session_id: Option<String>,
    pub outcome: Option<&'static str>,
    pub duration_milliseconds: Option<u128>,
    pub error_code: Option<&'static str>,
}

pub trait InteractionDiagnostics: Send + Sync {
    fn write(&self, event: BrokerDiagnostic);
}

/// The normal Tokio adapter for the existing domain monotonic-clock contract.
/// Tests can inject another Clock without changing the original deadline.
pub struct TokioClock;

impl Clock for TokioClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn utc_now(&self) -> SystemTime {
        SystemTime::now()
    }
    fn sleep_until<'a>(
        &'a self,
        deadline: Instant,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Cancelled>> {
        Box::pin(async move {
            tokio::select! {
                _ = cancellation.cancelled() => Err(Cancelled),
                _ = tokio::time::sleep_until(deadline.into()) => Ok(()),
            }
        })
    }
}

pub(crate) fn validate_timeout(
    timeout: Option<Duration>,
    category: &str,
) -> Result<(), InteractionError> {
    if timeout == Some(Duration::ZERO) {
        return Err(InteractionError::new(
            InteractionErrorKind::InvalidTimeout,
            format!("The {category} request timeout must be positive or infinite."),
        ));
    }
    Ok(())
}

pub(crate) fn default_clock() -> Arc<dyn Clock> {
    Arc::new(TokioClock)
}

pub(crate) fn failure_code(error: &InteractionError) -> &'static str {
    match error.kind {
        InteractionErrorKind::Argument | InteractionErrorKind::InvalidTimeout => "argument",
        InteractionErrorKind::Disposed => "invalid_operation",
        _ => "unexpected",
    }
}

pub(crate) fn require_nonempty(value: &str, name: &str) -> Result<(), InteractionError> {
    if value.is_empty() {
        Err(InteractionError::new(
            InteractionErrorKind::Argument,
            format!("{name} cannot be empty"),
        ))
    } else {
        Ok(())
    }
}
