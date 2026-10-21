use super::{TaskDiagnostic, TaskDiagnostics, TaskSession};
use crate::{
    agent_turn::lifecycle::IncomingActivity,
    children::{ChildError, ChildResult, Operation},
};
use parrot_domain::{protocol::messages::parrot::Delivery, values::identifiers::Identifier};
use parrot_storage::{
    artifacts::output_blobs::{ToolOutputBlobStore, is_tool_output_oversized},
    events::{ConversationPart, NeverCancelled},
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;

struct Notification {
    text: String,
    cause: &'static str,
    wake: bool,
}
struct State {
    sender: Option<mpsc::UnboundedSender<Notification>>,
    delivery: Option<Operation<()>>,
}
pub(crate) struct TaskNotifier {
    session: Arc<dyn TaskSession>,
    output_blobs: Arc<ToolOutputBlobStore>,
    diagnostics: Arc<dyn TaskDiagnostics>,
    state: Mutex<State>,
    settling: AtomicBool,
}
impl TaskNotifier {
    pub fn new(
        session: Arc<dyn TaskSession>,
        output_blobs: Arc<ToolOutputBlobStore>,
        diagnostics: Arc<dyn TaskDiagnostics>,
    ) -> Arc<Self> {
        Arc::new(Self {
            session,
            output_blobs,
            diagnostics,
            state: Mutex::new(State {
                sender: None,
                delivery: None,
            }),
            settling: AtomicBool::new(false),
        })
    }
    pub fn enqueue(self: &Arc<Self>, text: String, cause: &'static str, wake: bool) {
        let mut state = self.state.lock().expect("task notifier poisoned");
        if self.settling.load(Ordering::Acquire) {
            return;
        }
        if state.delivery.is_none() {
            let (sender, receiver) = mpsc::unbounded_channel();
            state.sender = Some(sender);
            let notifier = self.clone();
            state.delivery = Some(Operation::start(
                async move { notifier.deliver(receiver).await },
            ));
        }
        if let Some(sender) = &state.sender {
            let _ = sender.send(Notification { text, cause, wake });
        }
    }
    pub fn settle(self: &Arc<Self>) -> Operation<()> {
        let mut state = self.state.lock().expect("task notifier poisoned");
        self.settling.store(true, Ordering::Release);
        state.sender = None;
        state
            .delivery
            .get_or_insert_with(|| Operation::start(async { Ok(()) }))
            .clone()
    }
    async fn deliver(&self, mut pending: mpsc::UnboundedReceiver<Notification>) -> ChildResult<()> {
        tokio::task::yield_now().await;
        while let Some(notification) = pending.recv().await {
            let text = if is_tool_output_oversized(&notification.text) {
                self.output_blobs
                    .persist(&notification.text, &NeverCancelled)
                    .map_err(|error| ChildError::failure("io", error.to_string()))?
            } else {
                notification.text
            };
            let message_id = Identifier::message_id()
                .map_err(|error| ChildError::failure("unexpected", error.to_string()))?;
            for attempt in 1..=3 {
                let parts = vec![ConversationPart::text(text.clone())];
                let result = if notification.wake && !self.settling.load(Ordering::Acquire) {
                    self.session
                        .send(
                            parts,
                            message_id.clone(),
                            Delivery::DELIVERY_STEER,
                            IncomingActivity {
                                name: "agent-tasks".into(),
                                cause: Some(notification.cause.into()),
                                ..IncomingActivity::default()
                            },
                        )
                        .await
                } else {
                    self.session
                        .record(parts, message_id.clone(), Delivery::DELIVERY_STEER)
                        .await
                };
                match result {
                    Ok(()) => break,
                    Err(_) if attempt < 3 => tokio::time::sleep(Duration::from_secs(1)).await,
                    Err(error) => {
                        self.diagnostics.record(TaskDiagnostic {
                            operation: "delivery",
                            session_id: self.session.identity().session.session_id.clone(),
                            correlation_id: String::new(),
                            outcome: "failed",
                            duration_milliseconds: None,
                            error_code: Some(error.code()),
                        });
                        break;
                    }
                }
            }
        }
        Ok(())
    }
}
