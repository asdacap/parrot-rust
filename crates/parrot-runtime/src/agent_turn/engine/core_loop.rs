use super::*;
use parrot_core::{
    CompletionDecision as CoreCompletionDecision, Error as CoreError, ErrorCode, ErrorKind,
    ProviderRequest, ProviderResponse, RetryReason, States as CoreStates, ToolCatalog,
    ToolDefinition, ToolExecutionRequest, ToolResult, TurnOptions,
};

struct RuntimeStates<'a, S: AccessSnapshot, T: TurnTools<S>> {
    backend: &'a RuntimeBackend<S, T>,
    binding: &'a TurnBinding<S, T>,
    cancellation: &'a dyn Cancellation,
    work: &'a dyn DrainWork,
    request_tools: Option<T::Snapshot>,
}

struct RuntimeTools<'a, S: AccessSnapshot, T: TurnTools<S>> {
    backend: &'a RuntimeBackend<S, T>,
    binding: &'a TurnBinding<S, T>,
    cancellation: &'a dyn Cancellation,
}

struct RuntimeProvider<'a, S: AccessSnapshot, T: TurnTools<S>> {
    backend: &'a RuntimeBackend<S, T>,
    binding: &'a TurnBinding<S, T>,
    cancellation: &'a dyn Cancellation,
}

struct CapturedTools<T> {
    normal: T,
    final_phase: T,
}

struct BoundTurn<S: AccessSnapshot, T: TurnTools<S>> {
    selection: TurnSelection<S>,
    tools: Option<CapturedTools<T::Snapshot>>,
}

struct ExecutionBinding<S: AccessSnapshot, T: TurnTools<S>> {
    selection: TurnSelection<S>,
    tools: T::Snapshot,
    status: Arc<dyn crate::compaction::ContextStatus>,
}

struct TurnBinding<S: AccessSnapshot, T: TurnTools<S>> {
    turn: std::sync::Mutex<Option<BoundTurn<S, T>>>,
    execution: std::sync::Mutex<Option<ExecutionBinding<S, T>>>,
}

impl<S: AccessSnapshot, T: TurnTools<S>> TurnBinding<S, T> {
    fn new() -> Self {
        Self {
            turn: std::sync::Mutex::new(None),
            execution: std::sync::Mutex::new(None),
        }
    }
    fn bind(&self, selection: TurnSelection<S>) {
        *self.turn.lock().expect("turn binding poisoned") = Some(BoundTurn {
            selection,
            tools: None,
        });
    }
    fn capture_selection(&self) -> Result<TurnSelection<S>, TurnError> {
        self.turn
            .lock()
            .expect("turn binding poisoned")
            .as_ref()
            .map(|turn| turn.selection.clone())
            .ok_or_else(|| TurnError::Service("turn selection is unavailable".into()))
    }
    fn retain_tools(&self, normal: T::Snapshot, final_phase: T::Snapshot) -> Result<(), TurnError> {
        let mut guard = self.turn.lock().expect("turn binding poisoned");
        let turn = guard
            .as_mut()
            .ok_or_else(|| TurnError::Service("turn selection is unavailable".into()))?;
        turn.tools = Some(CapturedTools {
            normal,
            final_phase,
        });
        Ok(())
    }
    fn capture_tools(&self, final_phase: bool) -> Result<T::Snapshot, TurnError> {
        let guard = self.turn.lock().expect("turn binding poisoned");
        let tools = guard
            .as_ref()
            .and_then(|turn| turn.tools.as_ref())
            .ok_or_else(|| TurnError::Service("turn tools are unavailable".into()))?;
        Ok(if final_phase {
            tools.final_phase.clone()
        } else {
            tools.normal.clone()
        })
    }
}

fn adapt_operation<'a, T: Send + 'a>(
    operation: impl std::future::Future<Output = Result<T, TurnError>> + Send + 'a,
) -> BoxFuture<'a, Result<T, CoreError>> {
    Box::pin(async move { operation.await.map_err(Into::into) })
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
                }
            }
        }
    }
}

pub(super) async fn run<S, T>(
    backend: &RuntimeBackend<S, T>,
    cancellation: &dyn Cancellation,
    work: &dyn DrainWork,
) -> Result<Execution, TurnError>
where
    S: AccessSnapshot + 'static,
    T: TurnTools<S> + 'static,
    T::Snapshot: 'static,
{
    {
        let mut state = backend.state.lock().await;
        state.request_status = None;
        state.request_accounting = None;
        state.tool_conversation.reset_pending();
    }
    let binding = TurnBinding::new();
    let mut states = RuntimeStates {
        backend,
        binding: &binding,
        cancellation,
        work,
        request_tools: None,
    };
    let tools = RuntimeTools {
        backend,
        binding: &binding,
        cancellation,
    };
    let provider = RuntimeProvider {
        backend,
        binding: &binding,
        cancellation,
    };
    let outcome = parrot_core::run(&mut states, &tools, &provider).await;
    {
        let mut state = backend.state.lock().await;
        state.request_status = None;
        state.request_accounting = None;
        state.tool_conversation.reset_pending();
    }
    match outcome {
        Ok(parrot_core::Outcome::Completed(answer)) => Ok(Execution::succeeded(answer)),
        Ok(parrot_core::Outcome::Failed(message)) => Ok(Execution::failed(&message)),
        Err(error) => Err(TurnError::Core(error)),
    }
}

impl<S, T> CoreStates for RuntimeStates<'_, S, T>
where
    S: AccessSnapshot + 'static,
    T: TurnTools<S> + 'static,
    T::Snapshot: 'static,
{
    fn restore_tool_results(&mut self) -> BoxFuture<'_, Result<(), CoreError>> {
        adapt_operation(async move {
            let mut guard = self.backend.state.lock().await;
            let state = &mut *guard;
            state
                .tool_conversation
                .restore_results(&mut state.conversation)
                .await
        })
    }
    fn prepare_tool_execution(
        &mut self,
    ) -> BoxFuture<'_, Result<Option<ToolExecutionRequest>, CoreError>> {
        adapt_operation(async move {
            let mut guard = self.backend.state.lock().await;
            let state = &mut *guard;
            let request = state
                .tool_conversation
                .prepare_execution(&mut state.conversation)
                .await?;
            if request.is_none() {
                self.binding
                    .execution
                    .lock()
                    .expect("execution binding poisoned")
                    .take();
            }
            Ok(request)
        })
    }
    fn record_tool_results<'a>(
        &'a mut self,
        results: &'a [ToolResult],
    ) -> BoxFuture<'a, Result<(), CoreError>> {
        adapt_operation(async move {
            let mut guard = self.backend.state.lock().await;
            let state = &mut *guard;
            state
                .tool_conversation
                .record_results(&mut state.conversation, results)
                .await
        })
    }
    fn disables_request_limit(&self) -> bool {
        self.backend.agent_limits.disable_max_turns
    }
    fn has_scheduled_work(&self) -> bool {
        self.work.has_scheduled()
    }
    fn check_cancellation(&self) -> Result<(), CoreError> {
        self.cancellation
            .check()
            .map_err(|_| CoreError::cancelled())
    }
    fn is_turn_open(&self) -> BoxFuture<'_, bool> {
        Box::pin(async move { self.backend.state.lock().await.turn.is_some() })
    }
    fn has_work(&self) -> BoxFuture<'_, Result<bool, CoreError>> {
        adapt_operation(async move {
            let state = self.backend.state.lock().await;
            Ok(state.conversation.is_answerable()
                || self.backend.history.has_pending_inputs().await?)
        })
    }
    fn begin_turn(&mut self) -> BoxFuture<'_, Result<TurnOptions, CoreError>> {
        adapt_operation(async move {
            let backend = self.backend;
            let mut guard = backend.state.lock().await;
            let state = &mut *guard;
            let cancellation = self.cancellation;
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
            state.turn = Some(TurnKey {
                agent_session_id: backend.identity.session_id.clone(),
                start_event_id: started.id.clone(),
            });
            state.diagnostic_turn = Some(started.id.clone());
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
            let maximum_requests = selection.profile.maximum_requests;
            self.binding.bind(selection);
            Ok(TurnOptions { maximum_requests })
        })
    }
    fn restore_tool_availability(&mut self) -> BoxFuture<'_, Result<(), CoreError>> {
        adapt_operation(async move {
            let backend = self.backend;
            let mut guard = backend.state.lock().await;
            let state = &mut *guard;
            let events = ContextEmitter { backend };
            state
                .context
                .restore_tool_availability(
                    &mut state.conversation,
                    &backend.notices.restored,
                    &events,
                    self.cancellation,
                )
                .await?;
            Ok(())
        })
    }
    fn promote_input(&mut self) -> BoxFuture<'_, Result<(), CoreError>> {
        adapt_operation(async move {
            let mut state = self.backend.state.lock().await;
            self.backend.promote(&mut state, self.cancellation).await
        })
    }
    fn take_interruption(&mut self) -> bool {
        self.backend.interruption.swap(false, Ordering::AcqRel)
    }
    fn prepare_request<'a>(
        &'a mut self,
        definitions: &'a [ToolDefinition],
        phase: parrot_core::budget::TurnRequestPhase,
    ) -> BoxFuture<'a, Result<ProviderRequest, CoreError>> {
        adapt_operation(async move {
            let backend = self.backend;
            let mut guard = backend.state.lock().await;
            let state = &mut *guard;
            let selection = self.binding.capture_selection()?;
            let selection = &selection;
            let cancellation = self.cancellation;
            let definitions: Vec<sessions::Tool> =
                definitions.iter().cloned().map(Into::into).collect();
            let definitions = definitions.as_slice();
            let events = ContextEmitter { backend };
            if phase.enters_final_phase {
                let content = if phase.interrupted {
                    &backend.notices.interrupted_final
                } else {
                    &backend.notices.final_request
                };
                state
                    .context
                    .inject_final_provider_request_prompt(
                        &mut state.conversation,
                        content,
                        &events,
                        cancellation,
                    )
                    .await?;
            }
            let status = backend.status.bind(selection);
            state.request_status = Some(status.clone());
            let epoch = state
                .context
                .prepare_epoch(
                    EpochInput {
                        selection: &selection.model,
                        tools: definitions,
                        conversation: &mut state.conversation,
                        prompt: &backend.prompt,
                        sessions: &backend.sessions,
                        status: status.as_ref(),
                        events: &events,
                    },
                    cancellation,
                )
                .await?;
            let messages: Vec<_> = state
                .conversation
                .messages()
                .iter()
                .map(provider_message)
                .collect();
            let estimate = crate::compaction::CompactionPlanner::estimate_input_tokens(
                &selection.model.canonical_model,
                &epoch.instructions,
                definitions,
                &messages,
            )
            .map_err(TurnError::Planning)?;
            let maximum = state
                .context
                .budget()
                .calculate_maximum_output_tokens(
                    &selection.model.canonical_model.selector(),
                    estimate,
                    &selection.model.canonical_model.model,
                )
                .map_err(TurnError::Planning)?;
            let request = sessions::Request {
                model: selection.model.canonical_model.model.id.clone(),
                instructions: epoch.instructions,
                messages,
                tools: definitions.to_vec(),
                max_output_tokens: maximum,
                reasoning: selection.model.canonical_model.reasoning(),
                ..Default::default()
            };
            state.request_accounting = Some(RequestAccounting {
                selected: selection.model.canonical_model.clone(),
                estimated_input_tokens: estimate,
            });
            self.request_tools = Some(self.binding.capture_tools(phase.final_phase)?);
            Ok(request.into())
        })
    }
    fn compact_request<'a>(
        &'a mut self,
        definitions: &'a [ToolDefinition],
    ) -> BoxFuture<'a, Result<bool, CoreError>> {
        adapt_operation(async move {
            let backend = self.backend;
            let mut guard = backend.state.lock().await;
            let state = &mut *guard;
            let selection = self.binding.capture_selection()?;
            let selection = &selection;
            let cancellation = self.cancellation;
            let definitions: Vec<sessions::Tool> =
                definitions.iter().cloned().map(Into::into).collect();
            let definitions = definitions.as_slice();
            let events = ContextEmitter { backend };
            let status = state
                .request_status
                .as_ref()
                .ok_or_else(|| TurnError::Service("request status is unavailable".into()))?;
            let compacted = state
                .context
                .compact_epoch(
                    EpochInput {
                        selection: &selection.model,
                        tools: definitions,
                        conversation: &mut state.conversation,
                        prompt: &backend.prompt,
                        sessions: &backend.sessions,
                        status: status.as_ref(),
                        events: &events,
                    },
                    None,
                    cancellation,
                )
                .await?;
            Ok(compacted.reduced)
        })
    }
    fn record_retry(
        &mut self,
        reason: RetryReason,
        attempt: i32,
    ) -> BoxFuture<'_, Result<(), CoreError>> {
        adapt_operation(async move {
            let message = match reason {
                RetryReason::TruncatedToolCalls => &self.backend.notices.truncated,
                RetryReason::InvalidToolCalls => &self.backend.notices.invalid,
            };
            let mut state = self.backend.state.lock().await;
            self.backend
                .retry_notice(&mut state, message, attempt, self.cancellation)
                .await
        })
    }
    fn record_tool_calls<'a>(
        &'a mut self,
        completed: &'a ProviderResponse,
    ) -> BoxFuture<'a, Result<(), CoreError>> {
        adapt_operation(async move {
            let completed: sessions::Completion = completed.clone().into();
            let completed = &completed;
            let backend = self.backend;
            let mut guard = backend.state.lock().await;
            let state = &mut *guard;
            let cancellation = self.cancellation;
            let published = backend.event();
            let assistant = backend
                .history
                .append_conversation(published.clone(), conversation_message(completed))
                .await?;
            state.conversation.add(model_assistant(completed));
            backend.publish(published, cancellation).await?;
            backend
                .activity
                .assistant_message(&completed.assistant_text);
            let mut received = backend.event();
            received.set_tool_request_received(protocol::ToolRequestReceived {
                tool_call_count: completed.tool_calls.len() as i32,
                ..Default::default()
            });
            backend.emit(received, cancellation).await?;
            state.tool_conversation.begin_execution(
                assistant.sequence,
                completed
                    .tool_calls
                    .iter()
                    .cloned()
                    .map(Into::into)
                    .collect(),
            )?;
            let tools = self
                .request_tools
                .take()
                .ok_or_else(|| TurnError::Service("request tools are unavailable".into()))?;
            let selection = self.binding.capture_selection()?;
            let status = state
                .request_status
                .clone()
                .ok_or_else(|| TurnError::Service("request status is unavailable".into()))?;
            *self
                .binding
                .execution
                .lock()
                .expect("execution binding poisoned") = Some(ExecutionBinding {
                selection,
                tools,
                status,
            });
            Ok(())
        })
    }
    fn evaluate_completion<'a>(
        &'a mut self,
        completed: &'a ProviderResponse,
    ) -> BoxFuture<'a, Result<CoreCompletionDecision, CoreError>> {
        adapt_operation(async move {
            let completed: sessions::Completion = completed.clone().into();
            let completed = &completed;
            let backend = self.backend;
            let mut guard = backend.state.lock().await;
            let state = &mut *guard;
            let selection = self.binding.capture_selection()?;
            let cancellation = self.cancellation;
            let work = self.work;
            work.join_mutations()
                .await
                .map_err(|e| TurnError::Service(e.to_string()))?;
            let candidate = CompletionCandidate {
                session_id: backend.identity.session_id.clone(),
                message_id: parrot_domain::values::identifiers::Identifier::message_id()
                    .map_err(|error| TurnError::Service(error.to_string()))?,
                assistant: completed.clone().into_message(),
                selection: selection.clone(),
            };
            let mut retry = None;
            for callback in &backend.completions {
                match callback.complete(&candidate, cancellation).await? {
                    CompletionDecision::Complete => {}
                    decision @ CompletionDecision::Retry { .. } => {
                        retry = Some(decision);
                        break;
                    }
                }
            }
            if let Some(CompletionDecision::Retry {
                system_message,
                retain_candidate_assistant,
                completion_retry_pending,
            }) = retry
            {
                if retain_candidate_assistant {
                    state.conversation.add(model_assistant(completed));
                    backend
                        .activity
                        .assistant_message(&completed.assistant_text);
                }
                state
                    .conversation
                    .add(text_message(MessageRole::System, system_message));
                return Ok(CoreCompletionDecision::Retry {
                    pending: completion_retry_pending,
                });
            }
            Ok(CoreCompletionDecision::Complete)
        })
    }
    fn complete_turn<'a>(
        &'a mut self,
        completed: &'a ProviderResponse,
    ) -> BoxFuture<'a, Result<String, CoreError>> {
        adapt_operation(async move {
            let completed: sessions::Completion = completed.clone().into();
            let completed = &completed;
            let backend = self.backend;
            let mut guard = backend.state.lock().await;
            let state = &mut *guard;
            let cancellation = self.cancellation;
            let work = self.work;
            work.join_mutations()
                .await
                .map_err(|e| TurnError::Service(e.to_string()))?;
            backend.close_turn(state, completed, cancellation).await?;
            if let Some(turn) = state.diagnostic_turn.take() {
                backend.activity.turn_finished(&turn, "completed", None);
            }
            backend
                .activity
                .assistant_message(&completed.assistant_text);
            Ok(completed.assistant_text.clone())
        })
    }
    fn fail_runaway(&mut self) -> BoxFuture<'_, Result<String, CoreError>> {
        adapt_operation(async move {
            self.backend
                .fail(self.backend.notices.runaway.clone(), String::new())
                .await?;
            Ok(self.backend.notices.runaway.clone())
        })
    }
}

impl<S, T> parrot_core::Tools for RuntimeTools<'_, S, T>
where
    S: AccessSnapshot + 'static,
    T: TurnTools<S> + 'static,
    T::Snapshot: 'static,
{
    fn capture_catalog(&self) -> BoxFuture<'_, Result<ToolCatalog, CoreError>> {
        adapt_operation(async move {
            let selection = self.binding.capture_selection()?;
            let normal = self.backend.snapshot(&selection)?;
            let final_phase = self.backend.tools.enabled_after_interruption(&normal);
            let catalog = ToolCatalog {
                normal: self
                    .backend
                    .tools
                    .definitions(&normal)
                    .iter()
                    .cloned()
                    .map(Into::into)
                    .collect(),
                final_phase: self
                    .backend
                    .tools
                    .definitions(&final_phase)
                    .iter()
                    .cloned()
                    .map(Into::into)
                    .collect(),
            };
            self.binding.retain_tools(normal, final_phase)?;
            Ok(catalog)
        })
    }
    fn execute(
        &self,
        request: ToolExecutionRequest,
    ) -> BoxFuture<'_, Result<Vec<ToolResult>, CoreError>> {
        adapt_operation(async move {
            let backend = self.backend;
            let (selection, snapshot, status) = {
                let guard = self
                    .binding
                    .execution
                    .lock()
                    .expect("execution binding poisoned");
                let binding = guard
                    .as_ref()
                    .ok_or_else(|| TurnError::Service("execution tools are unavailable".into()))?;
                (
                    binding.selection.clone(),
                    binding.tools.clone(),
                    binding.status.clone(),
                )
            };
            let mut guard = backend.state.lock().await;
            let state = &mut *guard;
            let events = ContextEmitter { backend };
            backend
                .tools
                .execute(
                    request,
                    TurnResources {
                        history: &backend.history,
                        conversation: &mut state.conversation,
                        context: &mut state.context,
                        prompt: &backend.prompt,
                        sessions: &backend.sessions,
                        status: status.as_ref(),
                        events: &events,
                        usage: &backend.usage,
                    },
                    &selection,
                    &snapshot,
                    backend.selection.as_ref(),
                    self.cancellation,
                )
                .await
        })
    }
}

impl<S, T> parrot_core::Provider for RuntimeProvider<'_, S, T>
where
    S: AccessSnapshot + 'static,
    T: TurnTools<S> + 'static,
    T::Snapshot: 'static,
{
    fn invoke(
        &self,
        request: ProviderRequest,
    ) -> BoxFuture<'_, Result<ProviderResponse, CoreError>> {
        Box::pin(async move {
            let backend = self.backend;
            let selection = self.binding.capture_selection().map_err(CoreError::from)?;
            let selected = &selection.model.canonical_model;
            let mut state = backend.state.lock().await;
            let session = backend
                .sessions
                .get(selected.provider.clone())
                .await
                .map_err(|error| CoreError::from(TurnError::Provider(error)))?;
            let output = OutputBridge(backend.output.as_ref());
            let activity = ActivityBridge(backend.activity.as_ref());
            let mut call = ProviderCall {
                history: &backend.history,
                publisher: backend.publisher.as_ref(),
                event_id: backend.event_id.as_ref(),
                output: &output,
                activity: &activity,
                usage: &mut *state,
            };
            let event = match call
                .stream(session.as_ref(), request.into(), self.cancellation)
                .await
            {
                Ok(event) => event,
                Err(
                    error @ provider_call::ProviderCallError::Provider(
                        sessions::Error::Http(
                            parrot_provider::http_stream::HttpError::RequestTooLarge(_),
                        )
                        | sessions::Error::Socket(
                            parrot_provider::responses_websocket::WebSocketError::RequestTooLarge {
                                ..
                            },
                        ),
                    ),
                ) => {
                    let mut error = CoreError::from(TurnError::ProviderCall(error));
                    error.kind = ErrorKind::RequestTooLarge;
                    return Err(error);
                }
                Err(error) => return Err(CoreError::from(TurnError::ProviderCall(error))),
            };
            match event.base_event() {
                sessions::Event::Completed(completed) => Ok(completed.clone().into()),
                _ => unreachable!("provider call returns completion"),
            }
        })
    }
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
