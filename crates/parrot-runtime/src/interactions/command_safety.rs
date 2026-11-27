//! Command execution permission gating over the shared permission machinery.

use super::{
    InteractionError, InteractionErrorKind, PermissionBroker, PermissionDecision,
    PermissionReplyKind, PermissionRequest, TargetApproval,
};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationError};
use parrot_domain::contracts::safety::{
    CommandSafetyClassifier, CommandSafetyGate, SafetyGateRefusal, SafetyVerdict,
};
use std::sync::Arc;

/// One shell command offered for permission. The command text is the whole
/// target; approval grants this single execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandTarget {
    pub command: String,
}

/// Approving a command target grants nothing beyond this one execution, so
/// validation only requires the target to be the command that was classified.
struct CommandApproval;

impl TargetApproval<CommandTarget> for CommandApproval {
    fn validate(&self, targets: &[CommandTarget]) -> Result<(), InteractionError> {
        if targets.len() != 1 || targets[0].command.trim().is_empty() {
            return Err(InteractionError::new(
                InteractionErrorKind::Permission,
                "a command permission request requires exactly one command",
            ));
        }
        Ok(())
    }

    fn approve(&self, _: &[CommandTarget]) -> Result<(), InteractionError> {
        Ok(())
    }
}

/// Combines the configured classifier with one user session's command
/// permission broker. A safe verdict proceeds; an unsafe verdict or a
/// classifier failure raises a permission request whose rejection refuses the
/// command.
pub struct CommandSafetyBroker {
    classifier: Arc<dyn CommandSafetyClassifier>,
    broker: Arc<PermissionBroker<CommandTarget>>,
    agent_session_id: String,
}

impl CommandSafetyBroker {
    pub fn new(
        classifier: Arc<dyn CommandSafetyClassifier>,
        broker: Arc<PermissionBroker<CommandTarget>>,
        agent_session_id: impl Into<String>,
    ) -> Self {
        Self {
            classifier,
            broker,
            agent_session_id: agent_session_id.into(),
        }
    }

    async fn request_permission(
        &self,
        command: &str,
        reason: &str,
        cancellation: &dyn Cancellation,
    ) -> Result<(), SafetyGateRefusal> {
        let reply = self
            .broker
            .request(
                PermissionRequest {
                    agent_session_id: self.agent_session_id.clone(),
                    reason: reason.to_owned(),
                    targets: vec![CommandTarget {
                        command: command.to_owned(),
                    }],
                    approval: Arc::new(CommandApproval),
                },
                cancellation,
            )
            .await;
        match reply {
            Ok(reply) if reply.kind == PermissionReplyKind::Decided => match reply.decision {
                PermissionDecision::Grant => Ok(()),
                PermissionDecision::Reject => {
                    Err(SafetyGateRefusal::new(if reply.reason.is_empty() {
                        "the user rejected the command".to_owned()
                    } else {
                        reply.reason
                    }))
                }
            },
            Ok(_) => Err(SafetyGateRefusal::new(
                "the user is away and the command was not approved",
            )),
            Err(OperationError::Cancelled) => Err(SafetyGateRefusal::new("cancelled")),
            Err(OperationError::Failed(error)) => Err(SafetyGateRefusal::new(error.to_string())),
        }
    }
}

impl CommandSafetyGate for CommandSafetyBroker {
    fn check<'a>(
        &'a self,
        command: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SafetyGateRefusal>> {
        Box::pin(async move {
            match self.classifier.classify(command, cancellation).await {
                Ok(SafetyVerdict::Safe) => Ok(()),
                Ok(SafetyVerdict::Unsafe { reason }) => {
                    if reason.is_empty() {
                        self.request_permission(
                            command,
                            "the safety classifier rejected this command",
                            cancellation,
                        )
                        .await
                    } else {
                        self.request_permission(command, &reason, cancellation)
                            .await
                    }
                }
                Err(error) => {
                    self.request_permission(
                        command,
                        &format!("the safety classifier could not be consulted: {error}"),
                        cancellation,
                    )
                    .await
                }
            }
        })
    }
}
