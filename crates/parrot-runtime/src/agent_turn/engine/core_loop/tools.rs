use super::*;
use crate::tool_settlement::*;
use parrot_core::{ToolCall, ToolResult, ToolStatus};
use parrot_domain::{
    contracts::{storage::Committed, tools::ParallelSafety},
    protocol::messages::parrot::YieldedShellProcess,
    values::{
        activity::AgentSessionActivity, identifiers::Identifier, tools::ToolCycleImageBudget,
    },
};
use parrot_storage::diagnostics::DiagnosticEvent;

mod invocation;

pub(super) struct RuntimeTools<'a, S: AccessSnapshot + 'static> {
    pub(super) config: &'a ToolExecutionConfig,
    pub(super) factory: &'a dyn ToolFactory<S>,
    pub(super) materialized: &'a std::sync::Mutex<Option<ToolSnapshot<S>>>,
    pub(super) execution: &'a tokio::sync::Mutex<Option<Arc<ToolCycleImageBudget>>>,
    pub(super) state: &'a Mutex<State>,
    pub(super) history: &'a AgentHistoryView,
    pub(super) prompt: &'a SessionPrompt,
    pub(super) status: &'a dyn TurnStatus<S>,
    pub(super) sessions: &'a ProviderSessions,
    pub(super) usage: &'a SessionUsageRecorder,
    pub(super) capture: &'a dyn SelectionSource<S>,
    pub(super) binding: &'a TurnBinding<S>,
    pub(super) events: &'a dyn ContextEvents,
    pub(super) interrupted_result: &'a str,
    pub(super) captured: std::sync::Mutex<Option<CapturedTools<ToolSnapshot<S>>>>,
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

impl<S> parrot_core::Tools<YieldedShellProcess> for RuntimeTools<'_, S>
where
    S: AccessSnapshot + 'static,
{
    fn capture_catalog(&self) -> BoxFuture<'_, Result<ToolCatalog, CoreError>> {
        adapt_operation(async move {
            let selection = self.binding.capture_selection()?;
            let normal = {
                let mut materialized = self.materialized.lock().expect("tool catalog poisoned");
                if materialized.is_none() {
                    *materialized = Some(ToolSnapshot::document(self.factory.materialize()?)?);
                }
                materialized
                    .as_ref()
                    .expect("materialized above")
                    .permitted_by(&selection.profile)
            };
            let final_phase = normal.enabled_after_interruption();
            let catalog = ToolCatalog {
                normal: normal.definitions().to_vec(),
                final_phase: final_phase.definitions().to_vec(),
            };
            *self.captured.lock().expect("captured tools poisoned") = Some(CapturedTools {
                normal,
                final_phase,
            });
            Ok(catalog)
        })
    }
    fn execute<'a>(
        &'a self,
        request: parrot_core::ToolExecutionRequest,
        cancellation: parrot_core::CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<Vec<ToolResult<YieldedShellProcess>>, CoreError>> {
        adapt_operation(async move {
            let selection = self.binding.capture_selection()?;
            let snapshot = {
                let captured = self.captured.lock().expect("captured tools poisoned");
                let tools = captured
                    .as_ref()
                    .ok_or_else(|| TurnError::Service("turn tools are unavailable".into()))?;
                if request.flags.final_phase {
                    tools.final_phase.clone()
                } else {
                    tools.normal.clone()
                }
            };
            let status = self.status.bind(&selection);
            let mut guard = self.state.lock().await;
            let state = &mut *guard;
            let resources = TurnResources {
                history: self.history,
                conversation: &mut state.conversation,
                agent_state: &mut state.agent_state,
                context: &mut state.context,
                prompt: self.prompt,
                sessions: self.sessions,
                status: status.as_ref(),
                events: self.events,
                usage: self.usage,
            };
            let selection = &selection;
            let snapshot = &snapshot;
            let capture = self.capture;
            let cancellation: &dyn Cancellation = &cancellation;
            let assistant_sequence = request.receipt.assistant_sequence;
            let mut execution = self.execution.lock().await;
            let first = request.calls.first().ok_or_else(|| {
                TurnError::Service("tool execution requires explicit calls".into())
            })?;
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
            if !Self::parallel_safe(snapshot, assistant_sequence, first, &self.config.templates) {
                let invocation = InvocationResources {
                    history: resources.history,
                    usage: resources.usage,
                    selection,
                    snapshot,
                    capture,
                    assistant_sequence,
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
                    || (index > 0
                        && !Self::parallel_safe(
                            snapshot,
                            assistant_sequence,
                            call,
                            &self.config.templates,
                        ))
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
                        assistant_sequence,
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
        })
    }
}

impl<S> RuntimeTools<'_, S>
where
    S: AccessSnapshot + 'static,
{
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
    fn parallel_safe(
        snapshot: &ToolSnapshot<S>,
        sequence: i64,
        call: &ToolCall,
        templates: &Arc<PromptTemplateCatalog>,
    ) -> bool {
        match snapshot.find(&call.name) {
            Some(ToolHandle::Ordinary(tool)) => {
                tool.parallel_safety(&ToolInvocation::new(
                    &call.id,
                    &call.arguments_json,
                    sequence,
                    templates.clone(),
                )) == ParallelSafety::ParallelSafe
            }
            Some(ToolHandle::Borrowed(_)) | None => false,
        }
    }
}

fn reborrow<'a>(resources: &'a mut TurnResources<'_>) -> TurnResources<'a> {
    TurnResources {
        history: resources.history,
        conversation: resources.conversation,
        agent_state: resources.agent_state,
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

#[cfg(test)]
#[path = "../../../../tests/tool_settlement/unit.rs"]
mod tests;
