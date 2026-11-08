use super::*;
use parrot_core::{
    CompletionFeedback as CoreCompletionFeedback, ContextCadence as CoreContextCadence,
    ContextCompaction as CoreContextCompaction,
    ContextReminderCheckpoint as CoreContextReminderCheckpoint,
    ContextSnapshot as CoreContextSnapshot, ContextState as CoreContextState, Error as CoreError,
    ErrorCode, ErrorKind, ProviderRequest, ProviderResponse, RetryReason, States as CoreStates,
    ToolCall, ToolCatalog, ToolDefinition, ToolResult, ToolStatus, TurnOptions,
};

struct RuntimeStates<'a, S: AccessSnapshot, T: TurnTools<S>> {
    backend: &'a RuntimeBackend<S, T>,
    binding: &'a TurnBinding<S, T>,
    cancellation: &'a dyn Cancellation,
    work: &'a dyn DrainWork,
    pending_context_reminder: Option<Event>,
    exit_blocker_sources: Vec<(Arc<dyn CompletionStateSource>, CompletionFeedbackKind)>,
    persistence_key: Option<TurnKey>,
    pending_compaction: Option<PendingCompaction>,
}

struct PendingCompaction {
    checkpoint: crate::compaction::CompactionCheckpoint,
    definitions: Vec<sessions::Tool>,
}

struct RuntimeTools<'a, S: AccessSnapshot, T: TurnTools<S>> {
    backend: &'a RuntimeBackend<S, T>,
    binding: &'a TurnBinding<S, T>,
    cancellation: &'a dyn Cancellation,
}

struct RuntimeCancellation<'a>(parrot_core::CancellationToken<'a>);

impl Cancellation for RuntimeCancellation<'_> {
    fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        self.0.cancelled()
    }
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
    assistant_sequence: i64,
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
        binding: &'a TurnBinding<S, T>,
        cancellation: &'a dyn Cancellation,
        work: &'a dyn DrainWork,
    ) -> Self {
        Self {
            backend,
            binding,
            cancellation,
            work,
            pending_context_reminder: None,
            exit_blocker_sources: Vec::new(),
            persistence_key: None,
            pending_compaction: None,
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
            let mut states = RuntimeStates::new(backend, &binding, cancellation, work);
            if cancellation.is_cancelled() {
                return Err(CoreError::cancelled());
            }
            let turn = states.get_last_turn().await?;
            if !turn.as_ref().is_some_and(|turn| turn.needs_response())
                && states.get_pending_message().await?.is_none()
            {
                return Ok(parrot_core::Outcome::Completed(answer));
            }
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
            let is_cancelled = || cancellation.is_cancelled();
            let cancelled = || cancellation.cancelled();
            let token = parrot_core::CancellationToken::new(&is_cancelled, &cancelled);
            let result = async {
                let options = adapt_operation(async {
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
                    let options = TurnOptions {
                        maximum_requests: selection.profile.maximum_requests,
                        model: selection.model.canonical_model.model.id.clone(),
                        reasoning: selection.model.canonical_model.reasoning().map(Into::into),
                        provider_preferences: String::new(),
                        include_router_metadata: false,
                    };
                    binding.bind(selection);
                    Ok(options)
                })
                .await?;
                parrot_core::run_turn(&mut states, &tools, &provider, &options, &mut budget, token)
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

impl<S, T> CoreStates for RuntimeStates<'_, S, T>
where
    S: AccessSnapshot + 'static,
    T: TurnTools<S> + 'static,
    T::Snapshot: 'static,
{
    fn record_tool_results<'a>(
        &'a mut self,
        results: &'a [ToolResult],
    ) -> BoxFuture<'a, Result<(), CoreError>> {
        adapt_operation(async move {
            let mut guard = self.backend.state.lock().await;
            let state = &mut *guard;
            state
                .tool_conversation
                .complete_execution(&mut state.conversation, results)
                .await?;
            self.binding
                .execution
                .lock()
                .expect("execution binding poisoned")
                .take();
            Ok(())
        })
    }
    fn get_last_turn(
        &self,
    ) -> BoxFuture<'_, Result<Option<parrot_core::ConversationTurn>, CoreError>> {
        adapt_operation(async move {
            let state = self.backend.state.lock().await;
            let Some(turn) = state.conversation.read_last_turn_messages() else {
                return Ok(None);
            };
            let messages: Vec<parrot_core::Message> =
                turn.iter().map(provider_message).map(Into::into).collect();
            let result = messages
                .iter()
                .rev()
                .find(|message| message.role != parrot_core::Role::System)
                .filter(|message| {
                    message.role == parrot_core::Role::Assistant && message.tool_calls.is_empty()
                })
                .cloned();
            Ok(Some(parrot_core::ConversationTurn { messages, result }))
        })
    }
    fn get_pending_message(
        &self,
    ) -> BoxFuture<'_, Result<Option<parrot_core::PendingMessage>, CoreError>> {
        adapt_operation(async move {
            let Some(input) = self.backend.history.peek_pending_input().await? else {
                return Ok(None);
            };
            let delivery = match input.delivery {
                protocol::Delivery::DELIVERY_STEER => parrot_core::Delivery::Steer,
                protocol::Delivery::DELIVERY_QUEUE => parrot_core::Delivery::Queue,
                protocol::Delivery::DELIVERY_UNSPECIFIED => {
                    return Err(TurnError::Service(
                        "pending input delivery is unspecified".into(),
                    ));
                }
            };
            Ok(Some(parrot_core::PendingMessage {
                input_id: input.id,
                message_id: input.message_id,
                delivery,
            }))
        })
    }
    fn promote_input(
        &mut self,
        admission: parrot_core::InputAdmission,
    ) -> BoxFuture<'_, Result<(), CoreError>> {
        adapt_operation(async move {
            let mut state = self.backend.state.lock().await;
            self.backend
                .promote(&mut state, admission, self.cancellation)
                .await
        })
    }
    fn read_context_state(&self) -> BoxFuture<'_, Result<CoreContextState, CoreError>> {
        adapt_operation(async move {
            let state = self.backend.state.lock().await;
            let selection = self.binding.capture_selection()?;
            Ok(state.context.capture_state(&selection.model))
        })
    }
    fn render_instructions(&mut self) -> BoxFuture<'_, Result<String, CoreError>> {
        adapt_operation(async move {
            let backend = self.backend;
            let mut guard = backend.state.lock().await;
            let state = &mut *guard;
            let selection = self.binding.capture_selection()?;
            state.request_status = Some(backend.status.bind(&selection));
            let cancellation = self.cancellation;
            cancellation.check().map_err(|_| ContextError::Cancelled)?;
            state
                .context
                .ensure_epoch(&backend.prompt, cancellation)
                .await?;
            Ok(backend
                .prompt
                .build(&selection.model, cancellation)
                .await
                .map_err(ContextError::from)?)
        })
    }
    fn estimate_input_tokens<'a>(
        &'a self,
        instructions: &'a str,
        definitions: &'a [ToolDefinition],
        messages: &'a [parrot_core::Message],
    ) -> BoxFuture<'a, Result<i64, CoreError>> {
        adapt_operation(async move {
            let selection = self.binding.capture_selection()?;
            let definitions: Vec<sessions::Tool> =
                definitions.iter().cloned().map(Into::into).collect();
            let messages: Vec<sessions::Message> =
                messages.iter().cloned().map(Into::into).collect();
            crate::compaction::CompactionPlanner::estimate_input_tokens(
                &selection.model.canonical_model,
                instructions,
                &definitions,
                &messages,
            )
            .map_err(TurnError::Planning)
        })
    }
    fn record_final_request(&mut self) -> BoxFuture<'_, Result<(), CoreError>> {
        adapt_operation(async move {
            let backend = self.backend;
            let mut guard = backend.state.lock().await;
            let state = &mut *guard;
            let events = ContextEmitter { backend };
            state
                .context
                .inject_final_provider_request_prompt(
                    &mut state.conversation,
                    &backend.notices.final_request,
                    &events,
                    self.cancellation,
                )
                .await?;
            Ok(())
        })
    }
    fn read_context_checkpoint(
        &self,
    ) -> BoxFuture<'_, Result<Option<CoreContextReminderCheckpoint>, CoreError>> {
        adapt_operation(async move {
            let state = self.backend.state.lock().await;
            Ok(state.context.read_context_checkpoint().await?)
        })
    }
    fn write_context_cadence(
        &mut self,
        cadence: CoreContextCadence,
    ) -> BoxFuture<'_, Result<(), CoreError>> {
        adapt_operation(async move {
            self.backend
                .state
                .lock()
                .await
                .context
                .write_context_cadence(cadence);
            Ok(())
        })
    }
    fn render_context_reminder<'a>(
        &'a self,
        context: &'a CoreContextSnapshot,
        percentage: i32,
    ) -> BoxFuture<'a, Result<String, CoreError>> {
        adapt_operation(async move {
            let state = self.backend.state.lock().await;
            Ok(state.context.render_reminder(
                &crate::compaction::ContextSnapshot {
                    estimated_tokens: context.estimated_tokens,
                    context_limit: context.context_limit,
                    usage_percent: context.usage_percent,
                    trigger_percent: context.trigger_percent,
                    input_limit: context.input_limit,
                    trigger_tokens: context.trigger_tokens,
                    has_context_limit_override: context.has_context_limit_override,
                },
                percentage,
            )?)
        })
    }
    fn append_context_reminder(
        &mut self,
        checkpoint: CoreContextReminderCheckpoint,
        content: String,
        observed_percentage: i32,
    ) -> BoxFuture<'_, Result<bool, CoreError>> {
        adapt_operation(async move {
            let mut guard = self.backend.state.lock().await;
            let state = &mut *guard;
            self.pending_context_reminder = state
                .context
                .append_context_reminder(
                    &mut state.conversation,
                    checkpoint,
                    content,
                    observed_percentage,
                )
                .await?;
            Ok(self.pending_context_reminder.is_some())
        })
    }
    fn publish_context_reminder(&mut self) -> BoxFuture<'_, Result<(), CoreError>> {
        adapt_operation(async move {
            let published = self.pending_context_reminder.take().ok_or_else(|| {
                TurnError::Service("context reminder publication is unavailable".into())
            })?;
            ContextEmitter {
                backend: self.backend,
            }
            .publish(published, &NeverCancelled)
            .await?;
            Ok(())
        })
    }
    fn read_messages(&self) -> BoxFuture<'_, Result<Vec<parrot_core::Message>, CoreError>> {
        adapt_operation(async move {
            let state = self.backend.state.lock().await;
            Ok(state
                .conversation
                .messages()
                .iter()
                .map(provider_message)
                .map(Into::into)
                .collect())
        })
    }
    fn prepare_compaction<'a>(
        &'a mut self,
        definitions: &'a [ToolDefinition],
    ) -> BoxFuture<'a, Result<parrot_core::compaction::PreparedCompaction<'a>, CoreError>> {
        adapt_operation(async move {
            let backend = self.backend;
            let mut guard = backend.state.lock().await;
            let state = &mut *guard;
            let selection = self.binding.capture_selection()?;
            let definitions: Vec<sessions::Tool> =
                definitions.iter().cloned().map(Into::into).collect();
            let events = ContextEmitter { backend };
            let status = state
                .request_status
                .clone()
                .ok_or_else(|| TurnError::Service("request status is unavailable".into()))?;
            let prepared = state
                .context
                .prepare_epoch_compaction(
                    &mut EpochInput {
                        selection: &selection.model,
                        tools: &definitions,
                        conversation: &mut state.conversation,
                        prompt: &backend.prompt,
                        sessions: &backend.sessions,
                        status: status.as_ref(),
                        events: &events,
                    },
                    None,
                    self.cancellation,
                )
                .await?;
            self.pending_compaction = Some(PendingCompaction {
                checkpoint: prepared.checkpoint,
                definitions,
            });
            let blobs = tokio::sync::MutexGuard::map(guard, |state| state.context.borrow_blobs());
            Ok(parrot_core::compaction::PreparedCompaction {
                plan: prepared.plan,
                effects: Box::new(crate::compaction::CompactionEffectsAdapter::new(
                    prepared.estimates,
                    blobs,
                    ContextEmitter { backend },
                )),
                provider: Box::new(crate::compaction::SummaryProviderAdapter::new(
                    selection.model.canonical_model,
                    &backend.sessions,
                    ContextEmitter { backend },
                )),
            })
        })
    }
    fn record_compaction(
        &mut self,
        result: Result<Option<parrot_core::compaction::CompactionResult>, CoreError>,
    ) -> BoxFuture<'_, Result<CoreContextCompaction, CoreError>> {
        Box::pin(async move {
            let pending = self
                .pending_compaction
                .take()
                .ok_or_else(|| CoreError::failed("compaction preparation is unavailable"))?;
            let backend = self.backend;
            let mut guard = backend.state.lock().await;
            let state = &mut *guard;
            let events = ContextEmitter { backend };
            let compacted = match result {
                Ok(compacted) => compacted,
                Err(error) => {
                    state
                        .context
                        .fail_epoch_compaction(error.to_string(), &events)
                        .await
                        .map_err(TurnError::from)
                        .map_err(CoreError::from)?;
                    return Err(error);
                }
            };
            let selection = self.binding.capture_selection().map_err(CoreError::from)?;
            let status = state
                .request_status
                .clone()
                .ok_or_else(|| CoreError::failed("request status is unavailable"))?;
            let result = state
                .context
                .record_epoch_compaction(
                    &mut EpochInput {
                        selection: &selection.model,
                        tools: &pending.definitions,
                        conversation: &mut state.conversation,
                        prompt: &backend.prompt,
                        sessions: &backend.sessions,
                        status: status.as_ref(),
                        events: &events,
                    },
                    pending.checkpoint,
                    Ok(compacted.map(Into::into)),
                    self.cancellation,
                )
                .await
                .map_err(TurnError::from)
                .map_err(CoreError::from)?;
            Ok(CoreContextCompaction {
                instructions: result.instructions,
                reduced: result.reduced,
            })
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
        phase: parrot_core::budget::TurnRequestPhase,
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
            let tools = self.binding.capture_tools(phase.final_phase)?;
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
                assistant_sequence: assistant.sequence,
                selection,
                tools,
                status,
            });
            Ok(())
        })
    }
    fn get_exit_blockers(&mut self) -> BoxFuture<'_, Result<Vec<String>, CoreError>> {
        adapt_operation(async move {
            let _guard = self.backend.state.lock().await;
            self.exit_blocker_sources.clear();
            self.work
                .join_mutations()
                .await
                .map_err(|error| TurnError::Service(error.to_string()))?;
            let mut blockers = Vec::new();
            for source in &self.backend.completion_sources {
                if let Some(reminder) = source.get_pending_ask_reminder(self.cancellation).await? {
                    blockers.push(reminder);
                    self.exit_blocker_sources
                        .push((source.clone(), CompletionFeedbackKind::PendingAsks));
                }
            }
            for source in &self.backend.completion_sources {
                if let Some(reminder) = source.get_active_work_reminder(self.cancellation).await? {
                    blockers.push(reminder);
                    self.exit_blocker_sources
                        .push((source.clone(), CompletionFeedbackKind::ActiveWork));
                }
            }
            for source in &self.backend.completion_sources {
                if let Some(reminder) = source.get_exit_reminder(self.cancellation).await? {
                    blockers.push(reminder);
                    self.exit_blocker_sources
                        .push((source.clone(), CompletionFeedbackKind::ExitReminders));
                }
            }
            Ok(blockers)
        })
    }
    fn record_completion_feedback(
        &mut self,
        feedback: CoreCompletionFeedback,
    ) -> BoxFuture<'_, Result<(), CoreError>> {
        adapt_operation(async move {
            let backend = self.backend;
            let mut guard = backend.state.lock().await;
            let state = &mut *guard;
            let mut candidate = Some(feedback.candidate.clone());
            for ((source, kind), message) in
                self.exit_blocker_sources.drain(..).zip(&feedback.messages)
            {
                source
                    .record_completion_feedback(
                        &CompletionFeedback {
                            kind,
                            message: message.clone(),
                            candidate: candidate.take(),
                        },
                        self.cancellation,
                    )
                    .await?;
            }
            let completed: sessions::Completion = feedback.candidate.into();
            state.conversation.add(model_assistant(&completed));
            backend
                .activity
                .assistant_message(&completed.assistant_text);
            for message in feedback.messages {
                state
                    .conversation
                    .add(text_message(MessageRole::System, message));
            }
            Ok(())
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
            let key = self
                .persistence_key
                .as_ref()
                .ok_or_else(|| TurnError::Service("turn start is unavailable".into()))?;
            backend
                .close_turn(state, key, completed, cancellation)
                .await?;
            Ok(completed.assistant_text.clone())
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
            Ok(catalog)
        })
    }
    fn execute<'a>(
        &'a self,
        calls: Vec<ToolCall>,
        cancellation: parrot_core::CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<Vec<ToolResult>, CoreError>> {
        adapt_operation(async move {
            let cancellation = RuntimeCancellation(cancellation);
            let backend = self.backend;
            let (assistant_sequence, selection, snapshot, status) = {
                let guard = self
                    .binding
                    .execution
                    .lock()
                    .expect("execution binding poisoned");
                let binding = guard
                    .as_ref()
                    .ok_or_else(|| TurnError::Service("execution tools are unavailable".into()))?;
                (
                    binding.assistant_sequence,
                    binding.selection.clone(),
                    binding.tools.clone(),
                    binding.status.clone(),
                )
            };
            let mut guard = backend.state.lock().await;
            let state = &mut *guard;
            let events = ContextEmitter { backend };
            let mut results = Vec::with_capacity(calls.len());
            let mut cancel_remaining = false;
            while results.len() < calls.len() {
                let chunk = backend
                    .tools
                    .execute(
                        ToolExecutionChunk {
                            assistant_sequence,
                            starts_batch: results.is_empty(),
                            calls: calls[results.len()..].to_vec(),
                            cancel_remaining,
                        },
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
                        &cancellation,
                    )
                    .await?;
                state
                    .tool_conversation
                    .record_results(&mut state.conversation, &chunk)
                    .await?;
                cancel_remaining |= chunk
                    .iter()
                    .any(|result| result.result.status == ToolStatus::Cancelled);
                results.extend(chunk.into_iter().map(|result| result.result));
            }
            Ok(results)
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
            let request: sessions::Request = request.into();
            let estimate = crate::compaction::CompactionPlanner::estimate_input_tokens(
                selected,
                &request.instructions,
                &request.tools,
                &request.messages,
            )
            .map_err(|error| CoreError::from(TurnError::Planning(error)))?;
            state.request_accounting = Some(RequestAccounting {
                selected: selected.clone(),
                estimated_input_tokens: estimate,
            });
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
                .stream(session.as_ref(), request, self.cancellation)
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
