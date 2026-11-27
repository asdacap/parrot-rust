//! Command safety broker behavior: classify, then gate unsafe commands
//! through the shared permission machinery.

use parrot_core::NeverCancelled;
use parrot_domain::contracts::safety::{
    CommandSafetyClassifier, CommandSafetyGate, SafetyClassifierError, SafetyVerdict,
};
use parrot_runtime::interactions::{
    CommandSafetyBroker, CommandTarget, InteractionDiagnostics, PendingPermission,
    PermissionBroker, PermissionEvents,
};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Diagnostics;
impl InteractionDiagnostics for Diagnostics {
    fn write(&self, _: parrot_runtime::interactions::BrokerDiagnostic) {}
}

struct NoEvents;
impl PermissionEvents<CommandTarget> for NoEvents {
    fn append<'a>(
        &'a self,
        pending: &'a PendingPermission<CommandTarget>,
    ) -> parrot_domain::contracts::async_ops::BoxFuture<
        'a,
        Result<
            parrot_domain::contracts::storage::Committed<
                parrot_domain::protocol::messages::parrot::Event,
            >,
            parrot_runtime::interactions::InteractionError,
        >,
    > {
        Box::pin(async move {
            let mut event = parrot_domain::protocol::messages::parrot::Event::new();
            event.id = pending.id.clone();
            event.agent_session_id = pending.agent_session_id.clone();
            Ok(parrot_domain::contracts::storage::Committed::after_commit(
                event,
            ))
        })
    }
    fn publish<'a>(
        &'a self,
        _: &'a parrot_domain::contracts::storage::Committed<
            parrot_domain::protocol::messages::parrot::Event,
        >,
    ) -> parrot_domain::contracts::async_ops::BoxFuture<
        'a,
        Result<(), parrot_runtime::interactions::InteractionError>,
    > {
        Box::pin(async { Ok(()) })
    }
}

enum Scripted {
    Safe,
    Unsafe { reason: &'static str },
    Error(&'static str),
}

struct ScriptedClassifier(Mutex<Scripted>);
impl CommandSafetyClassifier for ScriptedClassifier {
    fn classify<'a>(
        &'a self,
        _: &'a str,
        _: &'a dyn parrot_domain::contracts::async_ops::Cancellation,
    ) -> parrot_domain::contracts::async_ops::BoxFuture<
        'a,
        Result<SafetyVerdict, SafetyClassifierError>,
    > {
        Box::pin(async {
            match &*self.0.lock().unwrap() {
                Scripted::Safe => Ok(SafetyVerdict::Safe),
                Scripted::Unsafe { reason } => Ok(SafetyVerdict::Unsafe {
                    reason: (*reason).to_owned(),
                }),
                Scripted::Error(message) => {
                    Err(SafetyClassifierError::Transport((*message).to_owned()))
                }
            }
        })
    }
}

fn broker(script: Scripted) -> (CommandSafetyBroker, Arc<PermissionBroker<CommandTarget>>) {
    let permissions = Arc::new(
        PermissionBroker::new(true, None, Arc::new(Diagnostics), Arc::new(NoEvents)).unwrap(),
    );
    let gate = CommandSafetyBroker::new(
        Arc::new(ScriptedClassifier(Mutex::new(script))),
        permissions.clone(),
        "agent-session-1",
    );
    (gate, permissions)
}

#[tokio::test]
async fn safe_verdict_runs_without_a_permission_request() {
    let (gate, permissions) = broker(Scripted::Safe);
    gate.check("ls", &NeverCancelled).await.unwrap();
    assert!(
        permissions.pending().await.is_empty(),
        "a safe command must not open a permission request"
    );
}

#[tokio::test]
async fn rejected_unsafe_command_refuses_with_a_reason() {
    let (gate, permissions) = broker(Scripted::Unsafe {
        reason: "destroys data",
    });
    let check = tokio::spawn(async move { gate.check("rm -rf /", &NeverCancelled).await });
    let pending = loop {
        let pending = permissions.pending().await;
        if !pending.is_empty() {
            break pending;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    };
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].targets.len(), 1);
    assert_eq!(pending[0].targets[0].command, "rm -rf /");
    assert!(pending[0].reason.contains("destroys data"));
    permissions
        .reply(&pending[0].id, "reject", "")
        .await
        .unwrap();
    let refusal = check.await.unwrap().unwrap_err();
    assert!(
        !refusal.reason.is_empty(),
        "the refusal carries the dialog outcome"
    );
}

#[tokio::test]
async fn classifier_failure_opens_a_permission_request() {
    let (gate, permissions) = broker(Scripted::Error("network down"));
    let check = tokio::spawn(async move { gate.check("curl example.com", &NeverCancelled).await });
    let pending = loop {
        let pending = permissions.pending().await;
        if !pending.is_empty() {
            break pending;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    };
    assert!(
        pending[0].reason.contains("could not be consulted"),
        "the dialog explains the classifier failure"
    );
    permissions
        .reply(&pending[0].id, "grant", "")
        .await
        .unwrap();
    check
        .await
        .unwrap()
        .expect("a grant after a classifier failure runs the command");
}

#[tokio::test]
async fn closed_broker_refuses_the_command() {
    let permissions = Arc::new(
        PermissionBroker::new(true, None, Arc::new(Diagnostics), Arc::new(NoEvents)).unwrap(),
    );
    permissions.close().await;
    let gate = CommandSafetyBroker::new(
        Arc::new(ScriptedClassifier(Mutex::new(Scripted::Unsafe {
            reason: "destroys data",
        }))),
        permissions,
        "agent-session-1",
    );
    let refusal = gate.check("rm -rf /", &NeverCancelled).await.unwrap_err();
    assert!(!refusal.reason.is_empty());
}
