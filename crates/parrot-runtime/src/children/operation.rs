use crate::agent_turn::{lifecycle::LifecycleError, services::TurnError};
use parrot_domain::contracts::async_ops::Cancellation;
use std::{
    fmt,
    future::Future,
    sync::{Arc, Mutex},
};
use tokio::sync::Notify;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChildError {
    Registry(String),
    NameConflict(String),
    Cancelled,
    Suspended,
    Failure { code: &'static str, message: String },
}
pub type ChildResult<T> = Result<T, ChildError>;
impl ChildError {
    pub fn registry(message: impl Into<String>) -> Self {
        Self::Registry(message.into())
    }
    pub fn failure(code: &'static str, message: impl Into<String>) -> Self {
        Self::Failure {
            code,
            message: message.into(),
        }
    }
    pub fn code(&self) -> &'static str {
        match self {
            Self::Registry(_) | Self::NameConflict(_) => "invalid_operation",
            Self::Cancelled | Self::Suspended => "cancelled",
            Self::Failure { code, .. } => match *code {
                "cancelled" | "access_denied" | "io" | "timeout" | "http" | "argument"
                | "invalid_operation" => code,
                _ => "unexpected",
            },
        }
    }
}
impl fmt::Display for ChildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registry(message)
            | Self::NameConflict(message)
            | Self::Failure { message, .. } => f.write_str(message),
            Self::Cancelled => f.write_str("operation cancelled"),
            Self::Suspended => f.write_str("agent admission is suspended"),
        }
    }
}
impl std::error::Error for ChildError {}
impl From<LifecycleError> for ChildError {
    fn from(error: LifecycleError) -> Self {
        match error {
            LifecycleError::Cancelled => Self::Cancelled,
            LifecycleError::ShuttingDown => stopped(),
            LifecycleError::Failed(message) => Self::failure("unexpected", message),
        }
    }
}
impl From<TurnError> for ChildError {
    fn from(error: TurnError) -> Self {
        if error.is_cancelled() {
            Self::Cancelled
        } else {
            Self::failure(error.diagnostic_code(), error.to_string())
        }
    }
}
impl From<parrot_storage::events::EventError> for ChildError {
    fn from(error: parrot_storage::events::EventError) -> Self {
        TurnError::Storage(error).into()
    }
}
impl From<parrot_storage::conversation::ForkError> for ChildError {
    fn from(error: parrot_storage::conversation::ForkError) -> Self {
        match error {
            parrot_storage::conversation::ForkError::Repository(error) => error.into(),
            error => Self::failure("argument", error.to_string()),
        }
    }
}
pub(crate) fn stopped() -> ChildError {
    ChildError::registry("the user session is shutting down")
}
pub(crate) fn first(result: &mut ChildResult<()>, next: ChildResult<()>) {
    if result.is_ok() {
        *result = next;
    }
}

struct Completion<T> {
    value: Mutex<Option<ChildResult<T>>>,
    changed: Notify,
}
/// An observer of already-owned work. Creation starts the task synchronously.
pub struct Operation<T: Clone> {
    completion: Arc<Completion<T>>,
}
impl<T: Clone> Clone for Operation<T> {
    fn clone(&self) -> Self {
        Self {
            completion: self.completion.clone(),
        }
    }
}
impl<T: Clone + Send + 'static> Operation<T> {
    pub fn start(work: impl Future<Output = ChildResult<T>> + Send + 'static) -> Self {
        let completion = Arc::new(Completion {
            value: Mutex::new(None),
            changed: Notify::new(),
        });
        let result = completion.clone();
        // The supervisor retains the join and makes a panic observable to every observer.
        let task = tokio::spawn(work);
        tokio::spawn(async move {
            let outcome = match task.await {
                Ok(value) => value,
                Err(_) => Err(ChildError::failure(
                    "unexpected",
                    "owned child operation failed",
                )),
            };
            *result.value.lock().expect("child completion poisoned") = Some(outcome);
            result.changed.notify_waiters();
        });
        Self { completion }
    }
    pub fn is_finished(&self) -> bool {
        self.completion
            .value
            .lock()
            .expect("child completion poisoned")
            .is_some()
    }
    pub async fn join(&self) -> ChildResult<T> {
        loop {
            let changed = self.completion.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(value) = self
                .completion
                .value
                .lock()
                .expect("child completion poisoned")
                .clone()
            {
                return value;
            }
            changed.await;
        }
    }
    pub async fn wait(&self, cancellation: &dyn Cancellation) -> ChildResult<T> {
        if cancellation.is_cancelled() {
            return Err(ChildError::Cancelled);
        }
        tokio::select! { result = self.join() => result, _ = cancellation.cancelled() => Err(ChildError::Cancelled) }
    }
}

pub(crate) struct LinkedLifetime {
    pub parent: Arc<dyn Cancellation>,
    pub stop: Arc<crate::agent_turn::lifecycle::Signal>,
}
impl Cancellation for LinkedLifetime {
    fn is_cancelled(&self) -> bool {
        self.parent.is_cancelled() || self.stop.is_cancelled()
    }
    fn cancelled(&self) -> parrot_domain::contracts::async_ops::BoxFuture<'_, ()> {
        Box::pin(async move {
            tokio::select! { _ = self.parent.cancelled() => {}, _ = self.stop.cancelled() => {} }
        })
    }
}
