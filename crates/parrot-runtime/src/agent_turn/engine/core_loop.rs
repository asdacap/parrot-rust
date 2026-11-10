use super::*;
use parrot_core::{
    ContextCompaction as CoreContextCompaction, ContextSnapshot as CoreContextSnapshot,
    Error as CoreError, ErrorCode, ErrorKind, ProviderRequest, ProviderResponse,
    States as CoreStates, TokenCalibration, ToolCatalog, ToolDefinition, TurnOptions,
};

mod conversation;
mod provider;
mod states;
mod tools;

struct RuntimeStates<'a, S: AccessSnapshot, T: TurnTools<S>> {
    backend: &'a RuntimeBackend<S, T>,
    binding: &'a TurnBinding<S>,
    cancellation: &'a dyn Cancellation,
    work: &'a dyn DrainWork,
    persistence_key: Option<TurnKey>,
    budget: &'a mut parrot_core::TurnBudget,
}

struct RuntimeEventPublisher<'a, S: AccessSnapshot, T: TurnTools<S>> {
    backend: &'a RuntimeBackend<S, T>,
    cancellation: &'a dyn Cancellation,
}

impl<S, T> parrot_core::EventPublisher for RuntimeEventPublisher<'_, S, T>
where
    S: AccessSnapshot + 'static,
    T: TurnTools<S> + 'static,
    T::Snapshot: 'static,
{
    fn publish<'a>(
        &'a self,
        event: parrot_core::ConversationEvent,
        cancellation: parrot_core::CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<(), CoreError>> {
        adapt_operation(async move {
            self.backend
                .emit(
                    conversation::render_event(self.backend.event(), event),
                    &cancellation,
                )
                .await
        })
    }
    fn on_epoch_started(&self) -> BoxFuture<'_, Result<(), CoreError>> {
        adapt_operation(async move {
            self.backend
                .prompt
                .sample_epoch(self.cancellation)
                .await
                .map_err(Into::into)
        })
    }
    fn report_compaction_started(&self) -> BoxFuture<'_, Result<(), CoreError>> {
        adapt_operation(async move {
            self.backend
                .state
                .lock()
                .await
                .context
                .report_epoch_started(&ContextEmitter {
                    backend: self.backend,
                })
                .await
                .map_err(Into::into)
        })
    }
    fn report_compaction_failed(&self, message: String) -> BoxFuture<'_, Result<(), CoreError>> {
        adapt_operation(async move {
            self.backend
                .state
                .lock()
                .await
                .context
                .fail_epoch_compaction(
                    message,
                    &ContextEmitter {
                        backend: self.backend,
                    },
                )
                .await
                .map_err(Into::into)
        })
    }
}

struct RuntimeTools<'a, S: AccessSnapshot, T: TurnTools<S>> {
    backend: &'a RuntimeBackend<S, T>,
    binding: &'a TurnBinding<S>,
    captured: std::sync::Mutex<Option<CapturedTools<T::Snapshot>>>,
}

struct RuntimeProvider<'a, S: AccessSnapshot, T: TurnTools<S>> {
    backend: &'a RuntimeBackend<S, T>,
    binding: &'a TurnBinding<S>,
}

struct CapturedTools<T> {
    normal: T,
    final_phase: T,
}

struct TurnBinding<S: AccessSnapshot> {
    selection: std::sync::Mutex<Option<TurnSelection<S>>>,
    exit_blocker_sources:
        std::sync::Mutex<Vec<(Arc<dyn CompletionStateSource>, CompletionFeedbackKind)>>,
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
                let code = match error.diagnostic_code() {
                    "cancelled" => ErrorCode::Cancelled,
                    "access_denied" => ErrorCode::AccessDenied,
                    "io" => ErrorCode::Io,
                    "timeout" => ErrorCode::Timeout,
                    "http" => ErrorCode::Http,
                    "argument" => ErrorCode::Argument,
                    "invalid_operation" => ErrorCode::InvalidOperation,
                    "websocket_closed" => ErrorCode::WebSocketClosed,
                    "websocket" => ErrorCode::WebSocket,
                    "wire_protocol" => ErrorCode::WireProtocol,
                    "websocket_upgrade" => ErrorCode::WebSocketUpgrade,
                    "websocket_message_too_large" => ErrorCode::WebSocketMessageTooLarge,
                    "request_too_large" => ErrorCode::RequestTooLarge,
                    "websocket_transport" => ErrorCode::WebSocketTransport,
                    _ => ErrorCode::Unexpected,
                };
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

impl<'a, S, T> RuntimeStates<'a, S, T>
where
    S: AccessSnapshot + 'static,
    T: TurnTools<S> + 'static,
    T::Snapshot: 'static,
{
    fn new(
        backend: &'a RuntimeBackend<S, T>,
        binding: &'a TurnBinding<S>,
        cancellation: &'a dyn Cancellation,
        work: &'a dyn DrainWork,
        budget: &'a mut parrot_core::TurnBudget,
    ) -> Self {
        Self {
            backend,
            binding,
            cancellation,
            work,
            persistence_key: None,
            budget,
        }
    }
}

pub(super) async fn run<S, T>(
    backend: &RuntimeBackend<S, T>,
    cancellation: &dyn Cancellation,
    work: &dyn DrainWork,
) -> (Result<Execution, TurnError>, Option<TurnKey>)
where
    S: AccessSnapshot + 'static,
    T: TurnTools<S> + 'static,
    T::Snapshot: 'static,
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
    let mut budget = parrot_core::budget::TurnBudget::new(backend.agent_limits.disable_max_turns);
    let mut failed_key = None;
    let outcome: Result<parrot_core::Outcome, CoreError> = async {
        let mut answer = String::new();
        loop {
            if work.has_scheduled() {
                return Ok(parrot_core::Outcome::Completed(answer));
            }
            let binding = TurnBinding::new();
            let mut states = RuntimeStates::new(backend, &binding, cancellation, work, &mut budget);
            if cancellation.is_cancelled() {
                return Err(CoreError::cancelled());
            }
            let turn = states.get_last_turn().await?;
            if !turn.as_ref().is_some_and(|turn| turn.needs_response())
                && states.peek_pending_input_header().await?.is_none()
            {
                return Ok(parrot_core::Outcome::Completed(answer));
            }
            let tools = RuntimeTools {
                backend,
                binding: &binding,
                captured: std::sync::Mutex::new(None),
            };
            let provider = RuntimeProvider {
                backend,
                binding: &binding,
            };
            let result = async {
                adapt_operation(async {
                    let mut guard = backend.state.lock().await;
                    let state = &mut *guard;
                    let events = ContextEmitter { backend };
                    backend.skills.end_turn().await;
                    let mut selection = backend.selection.capture()?;
                    backend.sessions.begin_turn().await?;
                    let mut started = backend.event();
                    let mut payload = protocol::TurnStarted {
                        model: selection.model.canonical_model.selector(),
                        ..Default::default()
                    };
                    if let Some(icon) = selection
                        .model
                        .alias
                        .as_ref()
                        .and_then(|alias| alias.icon.as_ref())
                    {
                        payload.model_alias_icon =
                            protobuf::MessageField::some(protocol::TurnModelAliasIcon {
                                glyph: icon.glyph.clone(),
                                color: protobuf::EnumOrUnknown::from_i32(icon.color as i32),
                                ..Default::default()
                            });
                    }
                    started.set_turn_started(payload);
                    states.persistence_key = Some(TurnKey {
                        agent_session_id: backend.identity.session_id.clone(),
                        start_event_id: started.id.clone(),
                    });
                    backend.activity.turn_started(&started.id);
                    backend.emit(started, cancellation).await?;
                    state
                        .context
                        .ensure_epoch(&backend.prompt, cancellation)
                        .await?;
                    loop {
                        let snapshot = backend.snapshot(&selection)?;
                        let definitions = backend.tools.definitions(&snapshot).to_vec();
                        let status = backend.status.bind(&selection);
                        let outcome = state
                            .context
                            .inject_status(
                                EpochInput {
                                    selection: &selection.model,
                                    tools: &definitions,
                                    conversation: &mut state.conversation,
                                    prompt: &backend.prompt,
                                    sessions: &backend.sessions,
                                    status: status.as_ref(),
                                    events: &events,
                                },
                                backend.identity.depth,
                                &selection.profile.id,
                                cancellation,
                            )
                            .await?;
                        if matches!(outcome, StatusInjection::Complete) {
                            break;
                        }
                        selection = backend.selection.capture()?;
                        tokio::task::yield_now().await;
                    }
                    backend.skills.begin_turn(cancellation).await?;
                    binding.bind(selection);
                    Ok(())
                })
                .await?;
                parrot_core::run_turn(
                    &mut states,
                    &tools,
                    &provider,
                    &RuntimeEventPublisher {
                        backend,
                        cancellation,
                    },
                    parrot_core::CancellationToken::new(cancellation),
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
                    answer = completed;
                }
                Ok(parrot_core::Outcome::BudgetExhausted) => {
                    failed_key = states.persistence_key;
                    backend
                        .fail(backend.notices.runaway.clone(), String::new())
                        .await?;
                    return Ok(parrot_core::Outcome::BudgetExhausted);
                }
                Err(error) => {
                    failed_key = states.persistence_key;
                    return Err(error);
                }
            }
        }
    }
    .await;
    {
        let mut state = backend.state.lock().await;
        state.request_status = None;
        state.request_accounting = None;
        state.tool_conversation.reset_pending();
    }
    let execution = match outcome {
        Ok(parrot_core::Outcome::Completed(answer)) => Ok(Execution::succeeded(answer)),
        Ok(parrot_core::Outcome::BudgetExhausted) => {
            Ok(Execution::failed(&backend.notices.runaway))
        }
        Err(error) => Err(TurnError::Core(error)),
    };
    (execution, failed_key)
}

pub(super) async fn interrupt<S, T>(
    backend: &RuntimeBackend<S, T>,
    work: &dyn DrainWork,
    key: &TurnKey,
) -> Result<(), TurnError>
where
    S: AccessSnapshot + 'static,
    T: TurnTools<S> + 'static,
    T::Snapshot: 'static,
{
    let binding = TurnBinding::new();
    let mut budget = parrot_core::TurnBudget::new(backend.agent_limits.disable_max_turns);
    let mut states = RuntimeStates::new(backend, &binding, &NeverCancelled, work, &mut budget);
    states.persistence_key = Some(key.clone());
    if let Some(message) = parrot_core::interrupt_turn(&mut states)
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
            TurnError::Provider(sessions::Error::Cancellation("typed cancellation".into())),
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
