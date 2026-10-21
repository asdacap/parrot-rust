use std::sync::Arc;
use std::time::Duration;

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, Clock, OperationError};
use parrot_domain::contracts::interactions::PermissionBroker as PermissionBrokerContract;
use parrot_domain::contracts::storage::Committed;
use parrot_domain::protocol::messages::parrot::Event;
use parrot_domain::values::identifiers::Identifier;
use tokio::sync::Mutex;

use super::pending::Store;
use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionDecision {
    Grant,
    Reject,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionReplyKind {
    Decided,
    UserAway,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionReply {
    pub kind: PermissionReplyKind,
    pub decision: PermissionDecision,
    pub reason: String,
}

impl PermissionReply {
    pub fn decided(decision: PermissionDecision, reason: impl Into<String>) -> Self {
        Self {
            kind: PermissionReplyKind::Decided,
            decision,
            reason: reason.into(),
        }
    }
    pub fn user_away() -> Self {
        Self {
            kind: PermissionReplyKind::UserAway,
            decision: PermissionDecision::Reject,
            reason: String::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionChoice {
    pub value: &'static str,
    pub label: &'static str,
    pub decision: PermissionDecision,
    pub requires_reason: bool,
}

pub const PERMISSION_CHOICES: [PermissionChoice; 3] = [
    PermissionChoice {
        value: "grant",
        label: "Grant",
        decision: PermissionDecision::Grant,
        requires_reason: false,
    },
    PermissionChoice {
        value: "reject",
        label: "Reject",
        decision: PermissionDecision::Reject,
        requires_reason: false,
    },
    PermissionChoice {
        value: "reject with reason",
        label: "Reject with reason",
        decision: PermissionDecision::Reject,
        requires_reason: true,
    },
];

/// The requesting session's concrete security adapter owns target validation and
/// approval. This broker neither resolves targets nor grants filesystem access.
pub trait TargetApproval<T>: Send + Sync {
    fn validate(&self, targets: &[T]) -> Result<(), InteractionError>;
    fn approve(&self, targets: &[T]) -> Result<(), InteractionError>;
}

pub struct PermissionRequest<T> {
    pub agent_session_id: String,
    pub reason: String,
    pub targets: Vec<T>,
    pub approval: Arc<dyn TargetApproval<T>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingPermission<T> {
    pub id: String,
    pub agent_session_id: String,
    pub reason: String,
    pub targets: Vec<T>,
    pub choices: Vec<PermissionChoice>,
}

/// Appending precedes publication of the same committed event. If either operation
/// fails, no pending waiter remains. A successful append is not rolled back when
/// publication fails. Callbacks must not reenter this permission broker.
///
/// These are borrowed producer futures, not independently owned tasks. The host
/// must retain admitted request execution after observer drop and join publication
/// before completing the event broker or closing its repository.
pub trait PermissionEvents<T>: Send + Sync {
    fn append<'a>(
        &'a self,
        pending: &'a PendingPermission<T>,
    ) -> BoxFuture<'a, Result<Committed<Event>, InteractionError>>;
    fn publish<'a>(
        &'a self,
        event: &'a Committed<Event>,
    ) -> BoxFuture<'a, Result<(), InteractionError>>;
}

pub struct PermissionBroker<T> {
    pending: Store<PermissionRequest<T>, PermissionReply>,
    gate: Mutex<()>,
    interactive: bool,
    timeout: Option<Duration>,
    clock: Arc<dyn Clock>,
    diagnostics: Arc<dyn InteractionDiagnostics>,
    events: Arc<dyn PermissionEvents<T>>,
}

impl<T: Clone + Send + Sync + 'static> PermissionBroker<T> {
    pub fn new(
        interactive: bool,
        timeout: Option<Duration>,
        diagnostics: Arc<dyn InteractionDiagnostics>,
        events: Arc<dyn PermissionEvents<T>>,
    ) -> Result<Self, InteractionError> {
        Self::with_clock(interactive, timeout, default_clock(), diagnostics, events)
    }
    pub fn with_clock(
        interactive: bool,
        timeout: Option<Duration>,
        clock: Arc<dyn Clock>,
        diagnostics: Arc<dyn InteractionDiagnostics>,
        events: Arc<dyn PermissionEvents<T>>,
    ) -> Result<Self, InteractionError> {
        validate_timeout(timeout, "permission")?;
        Ok(Self {
            pending: Store::new(),
            gate: Mutex::new(()),
            interactive,
            timeout,
            clock,
            diagnostics,
            events,
        })
    }

    pub async fn request(
        &self,
        request: PermissionRequest<T>,
        cancellation: &dyn Cancellation,
    ) -> Result<PermissionReply, OperationError<InteractionError>> {
        let id = Identifier::permission_request_id().map_err(|error| {
            OperationError::Failed(InteractionError::new(
                InteractionErrorKind::Identifier,
                error.to_string(),
            ))
        })?;
        let session = request.agent_session_id.clone();
        let started = self.clock.now();
        self.diagnostics.write(BrokerDiagnostic {
            severity: DiagnosticSeverity::Information,
            category: "permission",
            event: "request_started",
            correlation_id: id.clone(),
            agent_session_id: Some(session.clone()),
            outcome: None,
            duration_milliseconds: None,
            error_code: None,
        });
        let result = async {
            if request.reason.trim().is_empty() {
                return Err(OperationError::Failed(InteractionError::new(
                    InteractionErrorKind::Argument,
                    "permission reason cannot be empty",
                )));
            }
            if request.targets.is_empty() {
                return Err(OperationError::Failed(InteractionError::permission(
                    "permission requests require at least one target",
                )));
            }
            request
                .approval
                .validate(&request.targets)
                .map_err(OperationError::Failed)?;
            self.pending.check_open().map_err(OperationError::Failed)?;
            if !self.interactive {
                return Ok(PermissionReply::decided(PermissionDecision::Reject, ""));
            }
            let gate = self.gate.lock().await;
            let entry = self
                .pending
                .insert(id.clone(), request, self.clock.now())
                .map_err(OperationError::Failed)?;
            let _withdraw = self.pending.withdraw_on_drop(&id, &entry);
            let pending = Self::describe(&id, &entry.input);
            let committed = self
                .events
                .append(&pending)
                .await
                .map_err(OperationError::Failed)?;
            self.events
                .publish(&committed)
                .await
                .map_err(OperationError::Failed)?;
            drop(gate);
            // Source WaitAsync starts only after durable append and publication.
            self.pending
                .wait(
                    &id,
                    Arc::clone(&entry),
                    cancellation,
                    self.timeout.map(|timeout| self.clock.now() + timeout),
                    self.clock.as_ref(),
                    PermissionReply::user_away(),
                )
                .await
        }
        .await;
        let (outcome, severity, error_code) = match &result {
            Ok(reply) => (
                if reply.kind == PermissionReplyKind::UserAway {
                    "timeout"
                } else if reply.decision == PermissionDecision::Grant {
                    "granted"
                } else {
                    "rejected"
                },
                DiagnosticSeverity::Information,
                None,
            ),
            Err(OperationError::Cancelled) => (
                "cancelled",
                DiagnosticSeverity::Information,
                Some("cancelled"),
            ),
            Err(OperationError::Failed(error)) => (
                "failed",
                DiagnosticSeverity::Error,
                Some(failure_code(error)),
            ),
        };
        self.diagnostics.write(BrokerDiagnostic {
            severity,
            category: "permission",
            event: "request_completed",
            correlation_id: id,
            agent_session_id: Some(session),
            outcome: Some(outcome),
            duration_milliseconds: Some(
                self.clock
                    .now()
                    .saturating_duration_since(started)
                    .as_millis(),
            ),
            error_code,
        });
        result
    }

    fn describe(id: &str, request: &PermissionRequest<T>) -> PendingPermission<T> {
        PendingPermission {
            id: id.to_owned(),
            agent_session_id: request.agent_session_id.clone(),
            reason: request.reason.clone(),
            targets: request.targets.clone(),
            choices: PERMISSION_CHOICES.to_vec(),
        }
    }
    pub async fn pending(&self) -> Vec<PendingPermission<T>> {
        let _gate = self.gate.lock().await;
        self.pending
            .snapshot(|id, entry| Self::describe(id, &entry.input))
    }
    fn missing(id: &str) -> InteractionError {
        InteractionError::new(
            InteractionErrorKind::PermissionNotFound,
            format!("permission request not found: {id}"),
        )
    }
    pub async fn reply(
        &self,
        id: &str,
        choice: &str,
        reason: &str,
    ) -> Result<(), InteractionError> {
        require_nonempty(id, "requestId")?;
        require_nonempty(choice, "choiceValue")?;
        let _gate = self.gate.lock().await;
        self.pending.settle(id, Self::missing(id), |request| {
            let choice = PERMISSION_CHOICES
                .iter()
                .find(|candidate| candidate.value == choice)
                .ok_or_else(|| {
                    InteractionError::permission(format!("unknown permission choice: {choice}"))
                })?;
            let reason = reason.trim();
            if choice.requires_reason && reason.is_empty() {
                return Err(InteractionError::permission(
                    "the selected permission choice requires a reason",
                ));
            }
            if !choice.requires_reason && !reason.is_empty() {
                return Err(InteractionError::permission(
                    "the selected permission choice does not accept a reason",
                ));
            }
            if choice.decision == PermissionDecision::Grant {
                request.approval.approve(&request.targets)?;
            }
            Ok(PermissionReply::decided(choice.decision, reason))
        })
    }
    pub async fn reject(&self, id: &str) -> Result<(), InteractionError> {
        self.reply(id, "reject", "").await
    }
    pub async fn withdraw(&self, id: &str) -> Result<(), InteractionError> {
        let _gate = self.gate.lock().await;
        self.pending.fail(
            id,
            Self::missing(id),
            InteractionError::permission("permission request withdrawn"),
        )
    }
    pub async fn close(&self) {
        let _gate = self.gate.lock().await;
        self.pending
            .close(InteractionError::permission("permission session closed"));
    }
}

impl<T: Clone + Send + Sync + 'static> PermissionBrokerContract for PermissionBroker<T> {
    type Request = PermissionRequest<T>;
    type Reply = PermissionReply;
    type Pending = Vec<PendingPermission<T>>;
    type RequestId = String;
    type Error = InteractionError;

    fn request<'a>(
        &'a self,
        request: Self::Request,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Self::Reply, OperationError<Self::Error>>> {
        Box::pin(self.request(request, cancellation))
    }
    fn pending(&self) -> BoxFuture<'_, Result<Self::Pending, Self::Error>> {
        Box::pin(async move { Ok(self.pending().await) })
    }
    fn reply<'a>(
        &'a self,
        id: &'a String,
        reply: Self::Reply,
    ) -> BoxFuture<'a, Result<(), Self::Error>> {
        Box::pin(async move {
            if reply.kind != PermissionReplyKind::Decided {
                return Err(InteractionError::permission(
                    "permission replies require a decision",
                ));
            }
            let choice = match reply.decision {
                PermissionDecision::Grant => "grant",
                PermissionDecision::Reject if reply.reason.trim().is_empty() => "reject",
                PermissionDecision::Reject => "reject with reason",
            };
            self.reply(id, choice, &reply.reason).await
        })
    }
    fn reject<'a>(&'a self, id: &'a String) -> BoxFuture<'a, Result<(), Self::Error>> {
        Box::pin(self.reject(id))
    }
    fn withdraw<'a>(&'a self, id: &'a String) -> BoxFuture<'a, Result<(), Self::Error>> {
        Box::pin(self.withdraw(id))
    }
}
