use super::*;
use parrot_core::{
    ConfigKey, ContextSnapshot as CoreContextSnapshot, ConversationAddition,
    ConversationPersistence, Error as CoreError, ErrorKind, FinishReason, Message, ProviderRequest,
    ProviderResponse, Role, States as CoreStates, ToolCatalog, ToolDefinition, TurnOptions,
};

mod conversation;
mod events;
use events::RuntimeEventPublisher;
mod provider;
use provider::RuntimeProvider;
mod hooks;
use hooks::RuntimeTurnHooks;
mod states;
use states::RuntimeStates;
mod skills;
use skills::RuntimeSkillProvider;
mod tools;
use tools::RuntimeTools;

struct CapturedTools<T> {
    normal: T,
    final_phase: T,
}

struct TurnBinding<S: AccessSnapshot> {
    selection: std::sync::Mutex<Option<TurnSelection<S>>>,
}

impl<S: AccessSnapshot> TurnBinding<S> {
    fn new() -> Self {
        Self {
            selection: std::sync::Mutex::new(None),
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

pub(super) fn adapt_operation<'a, T: Send + 'a>(
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

impl State {
    pub(super) async fn restore_agent_state(
        &mut self,
        store: &dyn AgentStateStore,
    ) -> Result<(), TurnError> {
        if self.persisted_agent_state.is_none() {
            let restored = store
                .load()
                .await
                .map_err(|error| TurnError::Service(error.to_string()))?
                .and_then(|snapshot| snapshot.core)
                .unwrap_or_default();
            self.agent_state = restored.clone();
            self.persisted_agent_state = Some(restored);
        }
        Ok(())
    }
    fn read_last_turn(&self) -> Option<parrot_core::ConversationTurn> {
        self.conversation
            .read_last_turn_messages()
            .map(|messages| parrot_core::ConversationTurn {
                messages: messages
                    .iter()
                    .map(provider_message)
                    .map(Into::into)
                    .collect(),
            })
    }
}

struct CoreLoopLifecycle<'a, S: AccessSnapshot + 'static> {
    backend: &'a RuntimeBackend<S>,
    cancellation: &'a dyn Cancellation,
    work: &'a dyn DrainWork,
    failed_turn: bool,
}

impl<S: AccessSnapshot + 'static> parrot_core::TurnLifecycle for CoreLoopLifecycle<'_, S> {
    fn has_scheduled_work(&self) -> bool {
        self.work.has_scheduled()
    }

    fn has_ready_turn(&mut self) -> BoxFuture<'_, Result<bool, CoreError>> {
        Box::pin(async move {
            let turn = self.backend.state.lock().await.read_last_turn();
            Ok(turn
                .as_ref()
                .is_some_and(parrot_core::ConversationTurn::needs_response)
                || self
                    .backend
                    .history
                    .has_pending_inputs()
                    .await
                    .map_err(TurnError::from)?)
        })
    }

    fn open_turn<'a>(
        &'a mut self,
        cancellation: parrot_core::CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<Box<dyn parrot_core::Turn + 'a>, CoreError>> {
        Box::pin(async move {
            let cancellation: &dyn Cancellation = &cancellation;
            let binding = Arc::new(TurnBinding::new());
            let backend = self.backend;
            let states =
                RuntimeStates::bind(backend, binding.clone(), self.cancellation, self.work);
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
                binding: binding.clone(),
                events: Box::new(tool_events),
                interrupted_result: &backend.notices.interrupted_result,
                captured: std::sync::Mutex::new(None),
            };
            let hooks = RuntimeTurnHooks {
                backend,
                binding: binding.clone(),
                cancellation: self.cancellation,
            };
            let event_publisher = RuntimeEventPublisher {
                events: ContextEmitter::new(
                    &backend.history,
                    backend.publisher.as_ref(),
                    &backend.broker,
                    backend.event_id.as_ref(),
                ),
                selection: backend.selection.clone(),
            };
            hooks.begin_conversation_turn().await?;
            event_publisher
                .report_turn_started(parrot_core::CancellationToken::new(cancellation))
                .await?;
            let provider = RuntimeProvider {
                binding: binding.clone(),
                state: &backend.state,
                prompt: &backend.prompt,
                status: backend.status.as_ref(),
                compactor: &backend.compactor,
                sessions: &backend.sessions,
                history: &backend.history,
                publisher: backend.publisher.as_ref(),
                event_id: backend.event_id.as_ref(),
                output: backend.output.as_ref(),
                activity: backend.activity.as_ref(),
                usage: &backend.usage,
                events: ContextEmitter::new(
                    &backend.history,
                    backend.publisher.as_ref(),
                    &backend.broker,
                    backend.event_id.as_ref(),
                ),
            };
            let skills = RuntimeSkillProvider {
                skills: backend.skills.as_ref(),
                cancellation: self.cancellation,
            };
            Ok(Box::new(RuntimeTurn {
                backend,
                failed_turn: &mut self.failed_turn,
                states,
                tools,
                hooks,
                event_publisher,
                provider,
                skills,
            }) as Box<dyn parrot_core::Turn>)
        })
    }
}

struct RuntimeTurn<'a, S: AccessSnapshot + 'static> {
    backend: &'a RuntimeBackend<S>,
    failed_turn: &'a mut bool,
    states: RuntimeStates<'a, S>,
    tools: RuntimeTools<'a, S>,
    hooks: RuntimeTurnHooks<'a, S>,
    event_publisher: RuntimeEventPublisher<'a, S>,
    provider: RuntimeProvider<'a, S>,
    skills: RuntimeSkillProvider<'a>,
}

impl<S: AccessSnapshot + 'static> parrot_core::Turn for RuntimeTurn<'_, S> {
    fn borrow_services(&mut self) -> parrot_core::TurnServices<'_> {
        parrot_core::TurnServices {
            states: &mut self.states,
            skills: &self.skills,
            tools: &self.tools,
            provider: &self.provider,
            event_publisher: &self.event_publisher,
            hooks: &self.hooks,
        }
    }

    fn dispose<'a>(
        &'a mut self,
        result: &'a Result<parrot_core::Outcome, CoreError>,
    ) -> BoxFuture<'a, Result<(), CoreError>> {
        Box::pin(async move {
            let backend = self.backend;
            backend.skills.end_turn().await;
            match result {
                Ok(parrot_core::Outcome::Completed(completed)) => {
                    backend.activity.turn_finished("completed", None);
                    backend.activity.assistant_message(completed);
                }
                Ok(parrot_core::Outcome::BudgetExhausted(notice)) => {
                    *self.failed_turn = true;
                    backend.fail(notice.clone(), String::new()).await?;
                }
                Err(error) => {
                    *self.failed_turn = true;
                    if error.kind == ErrorKind::Cancelled
                        && !backend.suspending.load(Ordering::Acquire)
                    {
                        let work = self.states.work;
                        let mut states = RuntimeStates::bind(
                            backend,
                            Arc::new(TurnBinding::new()),
                            &NeverCancelled,
                            work,
                        );
                        let content = states
                            .get_config(ConfigKey::InterruptedTemplate)
                            .await?
                            .into_prompt_template()?
                            .render(&[], parrot_core::CancellationToken::uncancelled())?;
                        let usage = states.read_usage_totals().await?;
                        let receipt = states
                            .add_conversation_messages(ConversationAddition {
                                messages: vec![Message::text(Role::Assistant, &content)],
                                persistence: ConversationPersistence::CloseTurn {
                                    finish_reason: FinishReason::Interrupted,
                                    usage,
                                },
                            })
                            .await?;
                        if receipt.inserted {
                            backend.activity.assistant_message(&content);
                        }
                    }
                }
            }
            Ok(())
        })
    }
}

pub(super) async fn run<S>(
    backend: &RuntimeBackend<S>,
    cancellation: &dyn Cancellation,
    work: &dyn DrainWork,
) -> (Result<Execution, TurnError>, bool)
where
    S: AccessSnapshot + 'static,
{
    let initialized = async {
        let mut guard = backend.state.lock().await;
        let state = &mut *guard;
        state
            .restore_agent_state(backend.agent_state.as_ref())
            .await?;
        state.agent_state.restart();
        state.tool_conversation.reset_pending();
        state
            .tool_conversation
            .restore_results(&mut state.conversation)
            .await
    }
    .await;
    if let Err(error) = initialized {
        return (Err(error), false);
    }
    let mut lifecycle = CoreLoopLifecycle {
        backend,
        cancellation,
        work,
        failed_turn: false,
    };
    let outcome = parrot_core::TurnEngine::run(
        &mut lifecycle,
        parrot_core::CancellationToken::new(cancellation),
    )
    .await;
    let failed_turn = lifecycle.failed_turn;
    {
        let mut state = backend.state.lock().await;
        state.tool_conversation.reset_pending();
    }
    let execution = match outcome {
        Ok(parrot_core::Outcome::Completed(answer)) => Ok(Execution::succeeded(answer)),
        Ok(parrot_core::Outcome::BudgetExhausted(notice)) => Ok(Execution::failed(notice)),
        Err(error) => Err(TurnError::Core(error)),
    };
    (execution, failed_turn)
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
            TurnError::Planning(parrot_core::PlanningError::GroupExceedsBudget),
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
                    retry_after: None,
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
