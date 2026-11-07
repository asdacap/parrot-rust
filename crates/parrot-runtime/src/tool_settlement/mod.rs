//! Retained tool materialization and explicit, call-ordered execution chunks.
mod model;
mod snapshot;
pub use model::*;
pub use snapshot::ToolSnapshot;

use crate::agent_turn::{
    services::{SelectionSource, TurnError, TurnResources, TurnSelection, TurnTools},
    usage::SessionUsageRecorder,
};
use crate::event_broker::BrokerError;
use parrot_core::{ToolCall, ToolExecutionRequest, ToolImage, ToolProcess, ToolResult, ToolStatus};
use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation},
        events::EventPublisher,
        security::AccessSnapshot,
        storage::Committed,
        tools::ParallelSafety,
    },
    protocol::messages::parrot::Event,
    values::{
        activity::AgentSessionActivity, identifiers::Identifier, tools::ToolCycleImageBudget,
    },
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_provider::sessions;
use parrot_storage::{
    artifacts::output_blobs::ToolOutputBlobStore,
    conversation::AgentHistoryView,
    diagnostics::{DiagnosticEvent, FileDiagnosticLog},
    events::NeverCancelled,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub trait ToolDiagnostics: Send + Sync {
    fn write(&self, event: &DiagnosticEvent);
}
impl ToolDiagnostics for FileDiagnosticLog {
    fn write(&self, event: &DiagnosticEvent) {
        FileDiagnosticLog::write(self, event);
    }
}
pub type ToolPublisher = dyn EventPublisher<Event = Event, Error = BrokerError>;
pub struct ToolSettlementConfig<S: AccessSnapshot + 'static> {
    pub factory: Arc<dyn ToolFactory<S>>,
    pub templates: Arc<PromptTemplateCatalog>,
    pub output_blobs: Arc<ToolOutputBlobStore>,
    pub publisher: Arc<ToolPublisher>,
    pub activity: Arc<AgentSessionActivity>,
    pub diagnostics: Arc<dyn ToolDiagnostics>,
    pub image_bytes_per_cycle: i32,
}
pub struct ToolSettlement<S: AccessSnapshot + 'static> {
    config: ToolSettlementConfig<S>,
    interrupted_result: String,
    execution: tokio::sync::Mutex<Option<Arc<ToolCycleImageBudget>>>,
}
struct InvocationResources<'a, S: AccessSnapshot + 'static> {
    history: &'a AgentHistoryView,
    usage: &'a SessionUsageRecorder,
    selection: &'a TurnSelection<S>,
    snapshot: &'a ToolSnapshot<S>,
    capture: &'a dyn SelectionSource<S>,
    assistant_sequence: i64,
    image_budget: Arc<ToolCycleImageBudget>,
    cancellation: &'a dyn Cancellation,
}
impl<S: AccessSnapshot + 'static> ToolSettlement<S> {
    pub fn new(config: ToolSettlementConfig<S>) -> Result<Self, TurnError> {
        let interrupted_result = config
            .templates
            .render("agent-session.interrupted-result", &[], &NeverCancelled)
            .map_err(|error| TurnError::Service(error.to_string()))?;
        Ok(Self {
            config,
            interrupted_result,
            execution: tokio::sync::Mutex::new(None),
        })
    }
    fn event(&self, history: &AgentHistoryView) -> Result<Event, TurnError> {
        Ok(Event {
            id: Identifier::event_id().map_err(|error| TurnError::Service(error.to_string()))?,
            agent_session_id: history.agent_session_id().into(),
            ..Event::new()
        })
    }
    async fn publish(&self, event: Event) -> Result<(), TurnError> {
        self.config
            .publisher
            .publish_committed(&Committed::after_commit(event), &NeverCancelled)
            .await
            .map_err(TurnError::Publication)
    }
    fn parallel_safe(snapshot: &ToolSnapshot<S>, sequence: i64, call: &ToolCall) -> bool {
        match snapshot.find(&call.name) {
            Some(ToolHandle::Ordinary(tool)) => {
                tool.parallel_safety(&ToolInvocation::new(
                    &call.id,
                    &call.arguments_json,
                    sequence,
                )) == ParallelSafety::ParallelSafe
            }
            Some(ToolHandle::Borrowed(_)) | None => false,
        }
    }
    async fn execute_inner(
        &self,
        request: ToolExecutionRequest,
        resources: TurnResources<'_>,
        selection: &TurnSelection<S>,
        snapshot: &ToolSnapshot<S>,
        capture: &dyn SelectionSource<S>,
        cancellation: &dyn Cancellation,
    ) -> Result<Vec<ToolResult>, TurnError> {
        let mut execution = self.execution.lock().await;
        let first = request
            .calls
            .first()
            .ok_or_else(|| TurnError::Service("tool execution requires explicit calls".into()))?;
        if request.starts_batch || execution.is_none() {
            let image_budget =
                Arc::new(ToolCycleImageBudget::new(self.config.image_bytes_per_cycle));
            *execution = Some(image_budget);
        }
        let image_budget = execution
            .as_ref()
            .expect("execution budget was initialized")
            .clone();
        if request.cancel_remaining || cancellation.is_cancelled() {
            return Ok(request.calls.iter().map(|call| self.cancel(call)).collect());
        }
        if !Self::parallel_safe(snapshot, request.assistant_sequence, first) {
            let invocation = InvocationResources {
                history: resources.history,
                usage: resources.usage,
                selection,
                snapshot,
                capture,
                assistant_sequence: request.assistant_sequence,
                image_budget,
                cancellation,
            };
            return self
                .invoke(invocation, Some(resources), first, None)
                .await
                .map(|result| vec![result]);
        }
        let mut executions = InvocationBatch::new();
        let mut admitted = 0;
        for (index, call) in request.calls.iter().enumerate() {
            if cancellation.is_cancelled()
                || (index > 0 && !Self::parallel_safe(snapshot, request.assistant_sequence, call))
            {
                break;
            }
            let launched = Arc::new(AtomicBool::new(false));
            executions.push(self.invoke(
                InvocationResources {
                    history: resources.history,
                    usage: resources.usage,
                    selection,
                    snapshot,
                    capture,
                    assistant_sequence: request.assistant_sequence,
                    image_budget: image_budget.clone(),
                    cancellation,
                },
                None,
                call,
                Some(launched.clone()),
            ));
            executions.wait_for_launch(&launched).await;
            admitted += 1;
        }
        let mut results: Vec<_> = executions
            .join()
            .await
            .into_iter()
            .collect::<Result<_, _>>()?;
        if cancellation.is_cancelled()
            || results
                .iter()
                .any(|result| result.status == ToolStatus::Cancelled)
        {
            results.extend(
                request.calls[admitted..]
                    .iter()
                    .map(|call| self.cancel(call)),
            );
        }
        Ok(results)
    }
}
impl<S: AccessSnapshot + 'static> TurnTools<S> for ToolSettlement<S> {
    type Snapshot = ToolSnapshot<S>;
    fn materialize(&self) -> Result<Self::Snapshot, TurnError> {
        ToolSnapshot::document(self.config.factory.materialize()?)
    }
    fn permitted(
        &self,
        snapshot: &Self::Snapshot,
        selection: &TurnSelection<S>,
    ) -> Result<Self::Snapshot, TurnError> {
        Ok(snapshot.permitted_by(&selection.profile))
    }
    fn definitions<'a>(&self, snapshot: &'a Self::Snapshot) -> &'a [sessions::Tool] {
        snapshot.definitions()
    }
    fn enabled_after_interruption(&self, snapshot: &Self::Snapshot) -> Self::Snapshot {
        snapshot.enabled_after_interruption()
    }
    fn execute<'a>(
        &'a self,
        request: ToolExecutionRequest,
        resources: TurnResources<'a>,
        selection: &'a TurnSelection<S>,
        snapshot: &'a Self::Snapshot,
        capture: &'a dyn SelectionSource<S>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<ToolResult>, TurnError>> {
        Box::pin(self.execute_inner(
            request,
            resources,
            selection,
            snapshot,
            capture,
            cancellation,
        ))
    }
}
fn reborrow<'a>(resources: &'a mut TurnResources<'_>) -> TurnResources<'a> {
    TurnResources {
        history: resources.history,
        conversation: resources.conversation,
        context: resources.context,
        prompt: resources.prompt,
        sessions: resources.sessions,
        status: resources.status,
        events: resources.events,
        usage: resources.usage,
    }
}
struct InvocationBatch<'a, T> {
    futures: Vec<Option<BoxFuture<'a, T>>>,
    results: Vec<Option<T>>,
}
impl<'a, T: Send + 'a> InvocationBatch<'a, T> {
    fn new() -> Self {
        Self {
            futures: Vec::new(),
            results: Vec::new(),
        }
    }
    fn push(&mut self, future: BoxFuture<'a, T>) {
        self.futures.push(Some(future));
        self.results.push(None);
    }
    fn poll_all(&mut self, cx: &mut std::task::Context<'_>) -> bool {
        let mut pending = false;
        for (future, result) in self.futures.iter_mut().zip(&mut self.results) {
            if let Some(running) = future {
                match running.as_mut().poll(cx) {
                    std::task::Poll::Ready(output) => {
                        *result = Some(output);
                        *future = None;
                    }
                    std::task::Poll::Pending => pending = true,
                }
            }
        }
        pending
    }
    async fn wait_for_launch(&mut self, launched: &AtomicBool) {
        std::future::poll_fn(|cx| {
            self.poll_all(cx);
            // Observe the marker only after polling the invocation: synchronous
            // code in Tool::invoke and its returned future runs before the next
            // admission. A pre-launch failure is retained as a completed result.
            if launched.load(Ordering::Acquire) || self.results.last().is_some_and(Option::is_some)
            {
                std::task::Poll::Ready(())
            } else {
                std::task::Poll::Pending
            }
        })
        .await
    }
    async fn join(mut self) -> Vec<T> {
        std::future::poll_fn(|cx| {
            if self.poll_all(cx) {
                std::task::Poll::Pending
            } else {
                std::task::Poll::Ready(())
            }
        })
        .await;
        self.results
            .into_iter()
            .map(|result| result.expect("all futures joined"))
            .collect()
    }
}
mod invocation;
