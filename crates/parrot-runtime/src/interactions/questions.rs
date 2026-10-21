use std::sync::Arc;
use std::time::Duration;

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, Clock, OperationError};
use parrot_domain::contracts::interactions::QuestionBroker as QuestionBrokerContract;
use parrot_domain::values::identifiers::Identifier;

use super::pending::Store;
use super::*;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingQuestion {
    pub id: String,
    pub questions: Vec<QuestionDefinition>,
    pub remaining_milliseconds: Option<u128>,
}

pub struct QuestionBroker {
    pending: Store<Vec<QuestionDefinition>, QuestionReply>,
    timeout: Option<Duration>,
    clock: Arc<dyn Clock>,
    diagnostics: Arc<dyn InteractionDiagnostics>,
}

impl QuestionBroker {
    pub fn new(
        timeout: Option<Duration>,
        diagnostics: Arc<dyn InteractionDiagnostics>,
    ) -> Result<Self, InteractionError> {
        Self::with_clock(timeout, default_clock(), diagnostics)
    }

    pub fn with_clock(
        timeout: Option<Duration>,
        clock: Arc<dyn Clock>,
        diagnostics: Arc<dyn InteractionDiagnostics>,
    ) -> Result<Self, InteractionError> {
        validate_timeout(timeout, "question")?;
        Ok(Self {
            pending: Store::new(),
            timeout,
            clock,
            diagnostics,
        })
    }

    pub async fn ask(
        &self,
        questions: Vec<QuestionDefinition>,
        cancellation: &dyn Cancellation,
    ) -> Result<QuestionReply, OperationError<InteractionError>> {
        let id = Identifier::question_request_id().map_err(|error| {
            OperationError::Failed(InteractionError::new(
                InteractionErrorKind::Identifier,
                error.to_string(),
            ))
        })?;
        let started = self.clock.now();
        self.diagnostics.write(BrokerDiagnostic {
            severity: DiagnosticSeverity::Information,
            category: "question",
            event: "wait_started",
            correlation_id: id.clone(),
            agent_session_id: None,
            outcome: None,
            duration_milliseconds: None,
            error_code: None,
        });
        let result = async {
            validate_questions(&questions).map_err(OperationError::Failed)?;
            let entry = self
                .pending
                .insert(id.clone(), questions, self.clock.now())
                .map_err(OperationError::Failed)?;
            let deadline = self.timeout.map(|timeout| entry.started + timeout);
            self.pending
                .wait(
                    &id,
                    entry,
                    cancellation,
                    deadline,
                    self.clock.as_ref(),
                    QuestionReply::user_away(),
                )
                .await
        }
        .await;
        let (outcome, severity, error_code) = match &result {
            Ok(reply) => (
                if reply.kind == QuestionReplyKind::UserAway {
                    "timeout"
                } else {
                    "answered"
                },
                DiagnosticSeverity::Information,
                None,
            ),
            Err(OperationError::Cancelled) => (
                "cancelled",
                DiagnosticSeverity::Information,
                Some("cancelled"),
            ),
            Err(OperationError::Failed(error))
                if error.kind == InteractionErrorKind::QuestionRejected =>
            {
                (
                    "rejected",
                    DiagnosticSeverity::Information,
                    Some("unexpected"),
                )
            }
            Err(OperationError::Failed(error)) => (
                "failed",
                DiagnosticSeverity::Error,
                Some(failure_code(error)),
            ),
        };
        self.diagnostics.write(BrokerDiagnostic {
            severity,
            category: "question",
            event: "wait_completed",
            correlation_id: id,
            agent_session_id: None,
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

    pub fn pending(&self) -> Vec<PendingQuestion> {
        let now = self.clock.now();
        self.pending.snapshot(|id, entry| PendingQuestion {
            id: id.to_owned(),
            questions: entry.input.clone(),
            remaining_milliseconds: self.timeout.map(|timeout| {
                let remaining =
                    timeout.saturating_sub(now.saturating_duration_since(entry.started));
                remaining.as_nanos().div_ceil(1_000_000)
            }),
        })
    }

    fn missing(id: &str) -> InteractionError {
        InteractionError::new(
            InteractionErrorKind::QuestionNotFound,
            format!("question request not found: {id}"),
        )
    }

    pub fn reply(&self, id: &str, reply: QuestionReply) -> Result<(), InteractionError> {
        require_nonempty(id, "requestId")?;
        self.pending.settle(id, Self::missing(id), |questions| {
            validate_reply(questions, &reply)?;
            Ok(reply)
        })
    }

    pub fn reject(&self, id: &str) -> Result<(), InteractionError> {
        require_nonempty(id, "requestId")?;
        self.pending.fail(
            id,
            Self::missing(id),
            InteractionError::rejected("question request rejected"),
        )
    }

    pub fn withdraw(&self, id: &str) -> Result<(), InteractionError> {
        self.reject(id)
    }

    pub fn close(&self) {
        self.pending
            .close(InteractionError::rejected("question session closed"));
    }
}

impl QuestionBrokerContract for QuestionBroker {
    type Request = Vec<QuestionDefinition>;
    type Reply = QuestionReply;
    type Pending = Vec<PendingQuestion>;
    type RequestId = String;
    type Error = InteractionError;

    fn ask<'a>(
        &'a self,
        request: Self::Request,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Self::Reply, OperationError<Self::Error>>> {
        Box::pin(self.ask(request, cancellation))
    }
    fn pending(&self) -> Result<Self::Pending, Self::Error> {
        Ok(self.pending())
    }
    fn reply(&self, id: &String, reply: Self::Reply) -> Result<(), Self::Error> {
        self.reply(id, reply)
    }
    fn reject(&self, id: &String) -> Result<(), Self::Error> {
        self.reject(id)
    }
    fn withdraw(&self, id: &String) -> Result<(), Self::Error> {
        self.withdraw(id)
    }
}
