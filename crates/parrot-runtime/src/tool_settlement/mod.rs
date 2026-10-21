//! Retained tool materialization and call-ordered durable batch settlement.
//!
//! A persisted terminal is never invoked again. An invocation interrupted by
//! process death before its terminal commit may run again: this is not an
//! exactly-once guarantee for external effects or an unpersisted policy capture.
mod model;
mod snapshot;
pub use model::*;
pub use snapshot::ToolSnapshot;

use crate::agent_turn::{
    services::{SelectionSource, TurnError, TurnResources, TurnSelection, TurnTools},
    usage::SessionUsageRecorder,
};
use crate::event_broker::BrokerError;
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
    events::{
        ConversationPart, MessageRole, NeverCancelled, ToolCall, ToolExecutionStatus,
        ToolExecutionTerminal,
    },
};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
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
    watermark: AtomicI64,
    reconciliation: tokio::sync::Mutex<()>,
}
struct Settlement {
    published: Event,
    terminal: ToolExecutionTerminal,
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
            watermark: AtomicI64::new(0),
            reconciliation: tokio::sync::Mutex::new(()),
        })
    }
    pub fn settled_conversation_sequence(&self) -> i64 {
        self.watermark.load(Ordering::Acquire)
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
    async fn restore(
        &self,
        resources: &mut TurnResources<'_>,
        sequence: i64,
        terminal: ToolExecutionTerminal,
    ) -> Result<(), TurnError> {
        if let Some(item) = resources
            .history
            .append_tool_settlement(self.event(resources.history)?, sequence, terminal)
            .await
            .map_err(TurnError::Storage)?
        {
            let message = resources
                .conversation
                .materialize(item)
                .await
                .map_err(TurnError::Storage)?;
            resources.conversation.add(message);
        }
        Ok(())
    }
    async fn settle(
        &self,
        resources: &mut TurnResources<'_>,
        sequence: i64,
        settlement: Settlement,
        terminals: &mut HashMap<String, ToolExecutionTerminal>,
    ) -> Result<(), TurnError> {
        if let Some(item) = resources
            .history
            .append_tool_settlement(
                settlement.published.clone(),
                sequence,
                settlement.terminal.clone(),
            )
            .await
            .map_err(TurnError::Storage)?
        {
            let message = resources
                .conversation
                .materialize(item)
                .await
                .map_err(TurnError::Storage)?;
            resources.conversation.add(message);
        }
        self.publish(settlement.published).await?;
        terminals.insert(
            settlement.terminal.tool_call_id.clone(),
            settlement.terminal,
        );
        Ok(())
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
    async fn reconcile_inner(
        &self,
        mut resources: TurnResources<'_>,
        selection: &TurnSelection<S>,
        snapshot: &ToolSnapshot<S>,
        capture: &dyn SelectionSource<S>,
        cancellation: &dyn Cancellation,
    ) -> Result<(), TurnError> {
        let _guard = self.reconciliation.lock().await;
        let conversation = resources
            .history
            .conversation_after(self.watermark.load(Ordering::Acquire))
            .await
            .map_err(TurnError::Storage)?;
        let batches: Vec<_> = conversation
            .iter()
            .filter(|item| {
                item.message.role == MessageRole::Assistant && !item.message.tool_calls.is_empty()
            })
            .collect();
        let mut terminals: HashMap<_, _> = resources
            .history
            .tool_terminals_of_calls(
                batches
                    .iter()
                    .flat_map(|batch| batch.message.tool_calls.iter().map(|call| call.id.clone()))
                    .collect(),
            )
            .await
            .map_err(TurnError::Storage)?
            .into_iter()
            .map(|terminal| (terminal.tool_call_id.clone(), terminal))
            .collect();
        for batch in batches {
            let image_budget =
                Arc::new(ToolCycleImageBudget::new(self.config.image_bytes_per_cycle));
            let calls = &batch.message.tool_calls;
            if calls.iter().any(|call| !terminals.contains_key(&call.id)) {
                for call in calls {
                    if let Some(terminal) = terminals.get(&call.id) {
                        if terminal.status == ToolExecutionStatus::ImageBudgetExceeded {
                            image_budget.restore_exceeded();
                        }
                        for part in &terminal.result_parts {
                            if let ConversationPart::ImageArtifact { artifact_id, .. } = part {
                                let artifact = resources
                                    .history
                                    .resolve_image_artifact(artifact_id.clone())
                                    .await
                                    .map_err(TurnError::Storage)?
                                    .ok_or_else(|| {
                                        TurnError::Service(
                                            "A settled tool image artifact is missing.".into(),
                                        )
                                    })?;
                                image_budget
                                    .restore_accepted(artifact.byte_length)
                                    .map_err(|error| TurnError::Service(error.to_string()))?;
                            }
                        }
                    }
                }
            }
            let mut stopped = false;
            let mut index = 0;
            while index < calls.len() {
                let call = &calls[index];
                if let Some(terminal) = terminals.get(&call.id).cloned() {
                    stopped |= terminal.status == ToolExecutionStatus::Cancelled;
                    self.restore(&mut resources, batch.sequence, terminal)
                        .await?;
                    index += 1;
                    continue;
                }
                if stopped || cancellation.is_cancelled() {
                    let settlement = self.cancel(resources.history, call)?;
                    self.settle(&mut resources, batch.sequence, settlement, &mut terminals)
                        .await?;
                    stopped = true;
                    index += 1;
                    continue;
                }
                if !Self::parallel_safe(snapshot, batch.sequence, call) {
                    let history = resources.history;
                    let usage = resources.usage;
                    let settlement = self
                        .invoke(
                            history,
                            usage,
                            Some(reborrow(&mut resources)),
                            selection,
                            snapshot,
                            capture,
                            batch.sequence,
                            call,
                            image_budget.clone(),
                            cancellation,
                            None,
                        )
                        .await?;
                    stopped |= settlement.terminal.status == ToolExecutionStatus::Cancelled;
                    self.settle(&mut resources, batch.sequence, settlement, &mut terminals)
                        .await?;
                    index += 1;
                    continue;
                }
                let mut end = index;
                let mut executions = InvocationBatch::new();
                while end < calls.len() {
                    let candidate = &calls[end];
                    if !Self::parallel_safe(snapshot, batch.sequence, candidate) {
                        break;
                    }
                    if let Some(terminal) = terminals.get(&candidate.id) {
                        if terminal.status == ToolExecutionStatus::Cancelled {
                            break;
                        }
                        end += 1;
                        continue;
                    }
                    if cancellation.is_cancelled() {
                        break;
                    }
                    let launched = Arc::new(AtomicBool::new(false));
                    executions.push(self.invoke(
                        resources.history,
                        resources.usage,
                        None,
                        selection,
                        snapshot,
                        capture,
                        batch.sequence,
                        candidate,
                        image_budget.clone(),
                        cancellation,
                        Some(launched.clone()),
                    ));
                    // Match hot source invocation: observe Started publication,
                    // capture, usage and actual tool launch before admitting a
                    // later call. Already admitted peers keep being polled while
                    // this invocation's asynchronous prelude is pending.
                    executions.wait_for_launch(&launched).await;
                    end += 1;
                }
                // Admission can stop between the outer check and the run scan.
                if end == index {
                    continue;
                }
                // Join every admitted invocation, even when a peer fails outside
                // its tool catch boundary. No detached invocation tasks exist.
                let results = executions.join().await;
                let mut completed = HashMap::new();
                for result in results {
                    let settlement = result?;
                    completed.insert(settlement.terminal.tool_call_id.clone(), settlement);
                }
                while index < end {
                    let call = &calls[index];
                    if let Some(terminal) = terminals.get(&call.id).cloned() {
                        self.restore(&mut resources, batch.sequence, terminal)
                            .await?;
                    } else {
                        let settlement = completed
                            .remove(&call.id)
                            .expect("joined invocation has a settlement");
                        stopped |= settlement.terminal.status == ToolExecutionStatus::Cancelled;
                        self.settle(&mut resources, batch.sequence, settlement, &mut terminals)
                            .await?;
                    }
                    index += 1;
                }
            }
            let images: Vec<_> = calls
                .iter()
                .filter_map(|call| terminals.get(&call.id))
                .filter(|terminal| terminal.status == ToolExecutionStatus::Finished)
                .flat_map(|terminal| terminal.result_parts.iter())
                .filter(|part| matches!(part, ConversationPart::ImageArtifact { .. }))
                .cloned()
                .collect();
            if !images.is_empty()
                && !resources
                    .history
                    .has_tool_synthetic(batch.sequence)
                    .await
                    .map_err(TurnError::Storage)?
            {
                let published = self.event(resources.history)?;
                if let Some(item) = resources
                    .history
                    .append_tool_synthetic(published.clone(), batch.sequence, images)
                    .await
                    .map_err(TurnError::Storage)?
                {
                    let message = resources
                        .conversation
                        .materialize(item)
                        .await
                        .map_err(TurnError::Storage)?;
                    resources.conversation.add(message);
                }
                self.publish(published).await?;
            }
        }
        if let Some(last) = conversation.last() {
            self.watermark.store(last.sequence, Ordering::Release);
        }
        Ok(())
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
    fn reconcile<'a>(
        &'a self,
        resources: TurnResources<'a>,
        selection: &'a TurnSelection<S>,
        snapshot: &'a Self::Snapshot,
        capture: &'a dyn SelectionSource<S>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), TurnError>> {
        Box::pin(self.reconcile_inner(resources, selection, snapshot, capture, cancellation))
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
