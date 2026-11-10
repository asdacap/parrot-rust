use super::{TaskDiagnostic, TaskDiagnostics, TaskSession};
use crate::children::{ChildError, ChildResult, Operation};
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    values::agent_state::TaskNotificationSnapshot,
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;

pub(crate) struct Notification {
    pub value: TaskNotificationSnapshot,
    pub session: Arc<dyn TaskSession>,
    pub admit:
        Box<dyn Fn(TaskNotificationSnapshot, bool) -> BoxFuture<'static, ChildResult<()>> + Send>,
    pub finish: Box<dyn FnOnce(ChildResult<()>) -> BoxFuture<'static, ChildResult<()>> + Send>,
}
struct State {
    sender: Option<mpsc::UnboundedSender<Notification>>,
    delivery: Option<Operation<()>>,
}
pub(crate) struct TaskNotifier {
    diagnostics: Arc<dyn TaskDiagnostics>,
    lifetime: Arc<dyn Cancellation>,
    state: Mutex<State>,
    settling: AtomicBool,
    suspended: AtomicBool,
}
impl TaskNotifier {
    pub fn new(
        diagnostics: Arc<dyn TaskDiagnostics>,
        lifetime: Arc<dyn Cancellation>,
    ) -> Arc<Self> {
        Arc::new(Self {
            diagnostics,
            lifetime,
            state: Mutex::new(State {
                sender: None,
                delivery: None,
            }),
            settling: AtomicBool::new(false),
            suspended: AtomicBool::new(false),
        })
    }
    pub fn enqueue(self: &Arc<Self>, notification: Notification) -> bool {
        let mut state = self.state.lock().expect("task notifier poisoned");
        if self.suspended.load(Ordering::Acquire) {
            return false;
        }
        if state.delivery.is_none() {
            let (sender, receiver) = mpsc::unbounded_channel();
            state.sender = Some(sender);
            let notifier = self.clone();
            state.delivery = Some(Operation::start(
                async move { notifier.deliver(receiver).await },
            ));
        }
        state
            .sender
            .as_ref()
            .is_some_and(|sender| sender.send(notification).is_ok())
    }
    pub fn begin_settlement(&self) {
        self.settling.store(true, Ordering::Release);
    }
    pub fn suspend(self: &Arc<Self>) -> Operation<()> {
        self.suspended.store(true, Ordering::Release);
        self.settle()
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
            if self.suspended.load(Ordering::Acquire) {
                break;
            }
            let mut delivered = Err(ChildError::Cancelled);
            for attempt in 1..=3 {
                if self.suspended.load(Ordering::Acquire) {
                    break;
                }
                let wake = notification.value.wake
                    && !self.settling.load(Ordering::Acquire)
                    && !self.lifetime.is_cancelled();
                delivered = (notification.admit)(notification.value.clone(), wake).await;
                if delivered.is_ok() || self.suspended.load(Ordering::Acquire) {
                    break;
                }
                if attempt < 3 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
            if self.suspended.load(Ordering::Acquire)
                && delivered.as_ref().is_err_and(|error| {
                    error == &ChildError::Cancelled
                        || error
                            == &ChildError::from(
                                crate::agent_turn::lifecycle::LifecycleError::ShuttingDown,
                            )
                })
            {
                break;
            }
            if let Err(error) = &delivered {
                self.diagnostics.record(TaskDiagnostic {
                    operation: "delivery",
                    session_id: notification.session.identity().session.session_id.clone(),
                    correlation_id: String::new(),
                    outcome: "failed",
                    duration_milliseconds: None,
                    error_code: Some(error.code()),
                });
            }
            let finished = (notification.finish)(delivered).await;
            finished?;
        }
        Ok(())
    }
}
