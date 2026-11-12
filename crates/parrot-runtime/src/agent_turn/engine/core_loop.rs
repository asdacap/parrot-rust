use super::*;
use parrot_core::{
    ContextSnapshot as CoreContextSnapshot, Error as CoreError, ErrorKind, ProviderRequest,
    ProviderResponse, States as CoreStates, TokenCalibration, ToolCatalog, ToolDefinition,
    TurnOptions,
};

mod conversation;
mod events;
use events::RuntimeEventPublisher;
mod provider;
use provider::RuntimeProvider;
mod states;
use states::RuntimeStates;
mod tools;
use tools::RuntimeTools;

struct CapturedTools<T> {
    normal: T,
    final_phase: T,
}

struct TurnBinding<S: AccessSnapshot> {
    selection: std::sync::Mutex<Option<TurnSelection<S>>>,
    exit_blocker_sources: std::sync::Mutex<
        Vec<(
            Arc<dyn CompletionStateSource>,
            CompletionBlockerFeedbackKind,
        )>,
    >,
}

impl<S: AccessSnapshot> TurnBinding<S> {
    fn new() -> Self {
        Self {
            selection: std::sync::Mutex::new(None),
            exit_blocker_sources: std::sync::Mutex::new(Vec::new()),
        }
    }
    fn bind(&self, selection: TurnSelection<S>) {
        *self.selection.lock().expect("turn binding poisoned") = Some(selection);
    }
    fn capture_selection(&self) -> Result<TurnSelection<S>, TurnError> {
        self.selection
            .lock()
            .expect("turn binding poisoned")
            .clone()
            .ok_or_else(|| TurnError::Service("turn selection is unavailable".into()))
    }
}

fn adapt_operation<'a, T: Send + 'a>(
    operation: impl std::future::Future<Output = Result<T, TurnError>> + Send + 'a,
) -> BoxFuture<'a, Result<T, CoreError>> {
    Box::pin(async move { operation.await.map_err(Into::into) })
}

impl From<crate::compaction::CompactionError> for CoreError {
    fn from(error: crate::compaction::CompactionError) -> Self {
        TurnError::Context(ContextError::Compaction(error)).into()
    }
}

impl From<TurnError> for CoreError {
    fn from(error: TurnError) -> Self {
        match error {
            TurnError::Core(error) => error,
            error => {
                let kind = if error.is_cancelled() {
                    ErrorKind::Cancelled
                } else {
                    ErrorKind::Failed
                };
                let code = error.diagnostic_code();
                Self {
                    kind,
                    code,
                    message: error.to_string(),
                    response_body: error.response_body().to_owned(),
                    detail: None,
                }
            }
        }
    }
}

struct CoreLoopLifecycle<'a, S: AccessSnapshot + 'static> {
    backend: &'a RuntimeBackend<S>,
    cancellation: &'a dyn Cancellation,
    work: &'a dyn DrainWork,
    agent_state: parrot_core::AgentState,
    failed_key: Option<TurnKey>,
}

impl<S: AccessSnapshot + 'static> parrot_core::TurnLifecycle for CoreLoopLifecycle<'_, S> {
    fn has_scheduled_work(&self) -> bool {
        self.work.has_scheduled()
    }

    fn has_ready_turn(&mut self) -> BoxFuture<'_, Result<bool, CoreError>> {
        Box::pin(async move {
            let binding = TurnBinding::new();
            RuntimeStates::bind(
                self.backend,
                &binding,
                self.cancellation,
                self.work,
                &mut self.agent_state,
            )
            .has_ready_turn()
            .await
        })
    }

    fn run_turn<'a>(
        &'a mut self,
        cancellation: parrot_core::CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<parrot_core::Outcome, CoreError>> {
        Box::pin(async move {
            let cancellation: &dyn Cancellation = &cancellation;
            let binding = TurnBinding::new();
            let backend = self.backend;
            let mut states = RuntimeStates::bind(
                backend,
                &binding,
                self.cancellation,
                self.work,
                &mut self.agent_state,
            );
            let tool_events = ContextEmitter::new(
                &backend.history,
                backend.publisher.as_ref(),
                &backend.broker,
                backend.event_id.as_ref(),
            );
            let tools = RuntimeTools {
                config: &backend.tool_execution,
                factory: backend.tools.as_ref(),
                materialized: &backend.materialized_tools,
                execution: &backend.tool_execution_cycle,
                state: &backend.state,
                history: &backend.history,
                prompt: &backend.prompt,
                sessions: &backend.sessions,
                usage: &backend.usage,
                capture: backend.selection.as_ref(),
                binding: &binding,
                events: &tool_events,
                interrupted_result: &backend.notices.interrupted_result,
                captured: std::sync::Mutex::new(None),
            };
            let result = async {
                let publisher = RuntimeEventPublisher {
                    events: ContextEmitter::new(
                        &backend.history,
                        backend.publisher.as_ref(),
                        &backend.broker,
                        backend.event_id.as_ref(),
                    ),
                    prompt: &backend.prompt,
                    state: &backend.state,
                    cancellation,
                };
                let token = parrot_core::CancellationToken::new(cancellation);
                let start_event_id = parrot_core::open_turn(&mut states, &publisher, token).await?;
                let provider = RuntimeProvider {
                    model: binding.capture_selection().map_err(CoreError::from)?.model,
                    sessions: &backend.sessions,
                    history: &backend.history,
                    publisher: backend.publisher.as_ref(),
                    event_id: backend.event_id.as_ref(),
                    output: backend.output.as_ref(),
                    activity: backend.activity.as_ref(),
                    state: &backend.state,
                    events: ContextEmitter::new(
                        &backend.history,
                        backend.publisher.as_ref(),
                        &backend.broker,
                        backend.event_id.as_ref(),
                    ),
                };
                parrot_core::run_turn(
                    &mut states,
                    &tools,
                    &provider,
                    &publisher,
                    &start_event_id,
                    token,
                )
                .await
            }
            .await;
            match result {
                Ok(parrot_core::Outcome::Completed(completed)) => {
                    if let Some(key) = &states.persistence_key {
                        backend
                            .activity
                            .turn_finished(&key.start_event_id, "completed", None);
                    }
                    backend.activity.assistant_message(&completed);
                    Ok(parrot_core::Outcome::Completed(completed))
                }
                Ok(parrot_core::Outcome::BudgetExhausted(notice)) => {
                    self.failed_key = states.persistence_key.clone();
                    backend.fail(notice.clone(), String::new()).await?;
                    Ok(parrot_core::Outcome::BudgetExhausted(notice))
                }
                Err(error) => {
                    self.failed_key = states.persistence_key.clone();
                    Err(error)
                }
            }
        })
    }
}

pub(super) async fn run<S>(
    backend: &RuntimeBackend<S>,
    cancellation: &dyn Cancellation,
    work: &dyn DrainWork,
) -> (Result<Execution, TurnError>, Option<TurnKey>)
where
    S: AccessSnapshot + 'static,
{
    let initialized = {
        let mut guard = backend.state.lock().await;
        let state = &mut *guard;
        state.request_status = None;
        state.request_accounting = None;
        state.tool_conversation.reset_pending();
        state
            .tool_conversation
            .restore_results(&mut state.conversation)
            .await
    };
    if let Err(error) = initialized {
        return (Err(error), None);
    }
    let mut lifecycle = CoreLoopLifecycle {
        backend,
        cancellation,
        work,
        agent_state: parrot_core::AgentState::new(backend.agent_limits.disable_max_turns),
        failed_key: None,
    };
    let outcome = parrot_core::run(
        &mut lifecycle,
        parrot_core::CancellationToken::new(cancellation),
    )
    .await;
    let failed_key = lifecycle.failed_key.take();
    {
        let mut state = backend.state.lock().await;
        state.request_status = None;
        state.request_accounting = None;
        state.tool_conversation.reset_pending();
    }
    let execution = match outcome {
        Ok(parrot_core::Outcome::Completed(answer)) => Ok(Execution::succeeded(answer)),
        Ok(parrot_core::Outcome::BudgetExhausted(notice)) => Ok(Execution::failed(notice)),
        Err(error) => Err(TurnError::Core(error)),
    };
    (execution, failed_key)
}

pub(super) async fn interrupt<S>(
    backend: &RuntimeBackend<S>,
    work: &dyn DrainWork,
    key: &TurnKey,
) -> Result<(), TurnError>
where
    S: AccessSnapshot + 'static,
{
    let binding = TurnBinding::new();
    let mut agent_state = parrot_core::AgentState::new(backend.agent_limits.disable_max_turns);
    let mut states =
        RuntimeStates::bind(backend, &binding, &NeverCancelled, work, &mut agent_state);
    if let Some(message) = parrot_core::interrupt_turn(&mut states, &key.start_event_id)
        .await
        .map_err(TurnError::Core)?
    {
        backend.activity.assistant_message(&message);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_errors_preserve_typed_cancellation_and_diagnostic_metadata() {
        let errors = [
            TurnError::Cancelled,
            TurnError::Service("cancelled is only message text".into()),
            TurnError::Provider(sessions::Error::Cancelled),
            TurnError::Planning(crate::compaction::PlanningError::InputLimitExceeded),
            TurnError::ProviderCall(provider_call::ProviderCallError::Output(
                std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            )),
            TurnError::Publication(OperationError::Cancelled),
        ];
        for error in errors {
            let expected = (
                error.is_cancelled(),
                error.diagnostic_code(),
                error.to_string(),
                error.response_body().to_owned(),
            );
            let translated = TurnError::Core(error.into());
            assert_eq!(translated.is_cancelled(), expected.0);
            assert_eq!(translated.diagnostic_code(), expected.1);
            assert_eq!(translated.to_string(), expected.2);
            assert_eq!(translated.response_body(), expected.3);
        }
    }

    #[test]
    fn fixed_errors_preserve_only_the_top_level_provider_body() {
        let body = "opaque-provider-response-body";
        let provider_error = || {
            sessions::Error::Http(parrot_provider::http_stream::HttpError::Response(
                parrot_provider::http_stream::HttpFailure {
                    status: 503,
                    error_type: "busy".into(),
                    error_code: "retry".into(),
                    detail: "safe classification".into(),
                    response_body: body.into(),
                },
            ))
        };
        for (error, expected_body) in [
            (provider_error(), body),
            (
                sessions::Error::ProviderWithCause {
                    message: "outer provider wrapper".into(),
                    cause: Box::new(provider_error()),
                },
                "",
            ),
        ] {
            let error = TurnError::Provider(error);
            let expected_code = error.diagnostic_code();
            let translated = TurnError::Core(error.into());
            assert_eq!(translated.response_body(), expected_body);
            assert_eq!(translated.diagnostic_code(), expected_code);
            assert!(!translated.to_string().contains(body));
        }
    }
}
