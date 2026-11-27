//! Command safety classification boundaries for shell execution gating.

use super::async_ops::{BoxFuture, Cancellation};
use std::sync::Arc;

/// One command's classification outcome. An unsafe verdict carries the reason
/// the classifier rejected the command, suitable for a permission prompt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SafetyVerdict {
    Safe,
    Unsafe { reason: String },
}

/// Why a classification attempt failed. Callers treat every variant as
/// "unclassified" and must fail safe rather than execute.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SafetyClassifierError {
    /// The request never reached the classifier or timed out.
    Transport(String),
    /// The classifier service refused the request.
    Api { status: u16, message: String },
    /// The classifier replied but the answer could not be interpreted.
    InvalidResponse(String),
}
impl SafetyClassifierError {
    pub fn message(&self) -> &str {
        match self {
            Self::Transport(message)
            | Self::InvalidResponse(message)
            | Self::Api { message, .. } => message,
        }
    }
}
impl std::fmt::Display for SafetyClassifierError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            Self::Transport(_) => "classifier transport failure",
            Self::Api { .. } => "classifier api failure",
            Self::InvalidResponse(_) => "classifier response was not interpretable",
        };
        write!(formatter, "{label}: {}", self.message())
    }
}
impl std::error::Error for SafetyClassifierError {}

/// Decides whether a shell command is safe to execute. The verdict is external
/// judgment, never a local rule; a failed classification is an error so the
/// caller can fail safe. Implementations must honor cancellation and must not
/// block the executor's thread.
pub trait CommandSafetyClassifier: Send + Sync {
    fn classify<'a>(
        &'a self,
        command: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<SafetyVerdict, SafetyClassifierError>>;
}

/// Why a command may not start. The reason is shown to both the user in the
/// permission dialog and the model in the tool error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SafetyGateRefusal {
    pub reason: String,
}
impl SafetyGateRefusal {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// Guards one agent session's exec_command invocations. An approval resolves
/// inside the guard; a refusal names why the command did not start. The guard
/// never executes the command itself.
pub trait CommandSafetyGate: Send + Sync {
    fn check<'a>(
        &'a self,
        command: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SafetyGateRefusal>>;
}

/// Resolves one provider-qualified decision model id to its classifier.
/// None means the model's provider is not a configured decisions provider,
/// which leaves commands unclassified rather than blocked.
pub trait CommandClassifierResolver: Send + Sync {
    fn resolve(&self, model: &str) -> Result<Option<Arc<dyn CommandSafetyClassifier>>, String>;
}
