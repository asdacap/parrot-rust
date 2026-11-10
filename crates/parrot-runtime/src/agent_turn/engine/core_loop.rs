use super::*;
use parrot_core::{
    ContextCompaction as CoreContextCompaction, ContextSnapshot as CoreContextSnapshot,
    Error as CoreError, ErrorCode, ErrorKind, ProviderRequest, ProviderResponse,
    States as CoreStates, TokenCalibration, ToolCallReceipt, ToolCatalog, ToolDefinition,
    TurnOptions,
};

mod conversation;

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
    cancellation: &'a dyn Cancellation,
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
                && states.get_pending_message().await?.is_none()
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
                cancellation,
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
                    &RuntimeEventPublisher { backend },
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

impl<S, T> CoreStates for RuntimeStates<'_, S, T>
where
    S: AccessSnapshot + 'static,
    T: TurnTools<S> + 'static,
    T::Snapshot: 'static,
{
    fn add_conversation(
        &mut self,
        addition: parrot_core::ConversationAddition,
    ) -> BoxFuture<'_, Result<parrot_core::ConversationReceipt, CoreError>> {
        adapt_operation(conversation::add_conversation(self, addition))
    }
    fn read_final_phase(&self) -> BoxFuture<'_, Result<bool, CoreError>> {
        adapt_operation(async move {
            self.backend
                .history
                .read_final_phase()
                .await
                .map_err(Into::into)
        })
    }
    fn get_config(
        &self,
        key: parrot_core::ConfigKey,
    ) -> BoxFuture<'_, Result<parrot_core::ConfigValue, CoreError>> {
        adapt_operation(async move {
            use parrot_core::{ConfigKey, ConfigValue};
            match key {
                ConfigKey::Turn => {
                    let selection = self.binding.capture_selection()?;
                    Ok(ConfigValue::Turn(TurnOptions {
                        maximum_requests: selection.profile.maximum_requests,
                        model: selection.model.canonical_model.model.id.clone(),
                        reasoning: selection.model.canonical_model.reasoning().map(Into::into),
                        provider_preferences: String::new(),
                        include_router_metadata: false,
                    }))
                }
                ConfigKey::Context => {
                    let state = self.backend.state.lock().await;
                    let selection = self.binding.capture_selection()?;
                    Ok(ConfigValue::Context(
                        state
                            .context
                            .capture_context_configuration(&selection.model),
                    ))
                }
                ConfigKey::Compaction => {
                    let selection = self.binding.capture_selection()?;
                    Ok(ConfigValue::Compaction(
                        self.backend
                            .compactor
                            .planner()
                            .capture_compaction_configuration(&selection.model, None),
                    ))
                }
                ConfigKey::SystemPromptBindings => self
                    .backend
                    .prompt
                    .read_core_prompt_bindings(self.cancellation)
                    .await
                    .map(ConfigValue::SystemPromptBindings)
                    .map_err(ContextError::from)
                    .map_err(Into::into),
                key => self
                    .backend
                    .templates
                    .get_template(
                        crate::prompt_context::get_prompt_template_id(key).ok_or_else(|| {
                            TurnError::Service("unsupported template configuration key".into())
                        })?,
                    )
                    .map(ConfigValue::PromptTemplate)
                    .map_err(|error| {
                        TurnError::Prompt(crate::prompt_context::PromptError::Template(
                            error.to_string(),
                        ))
                    }),
            }
        })
    }
    fn read_turn_budget(&self) -> Result<parrot_core::TurnBudget, CoreError> {
        Ok(self.budget.clone())
    }
    fn write_turn_budget(&mut self, budget: parrot_core::TurnBudget) -> Result<(), CoreError> {
        *self.budget = budget;
        Ok(())
    }
    fn get_last_context_reminder(
        &self,
    ) -> BoxFuture<'_, Result<Option<parrot_core::ContextReminderCheckpoint>, CoreError>> {
        adapt_operation(async move {
            self.backend
                .state
                .lock()
                .await
                .context
                .get_last_context_reminder()
                .await
                .map_err(Into::into)
        })
    }
    fn sample_epoch(&mut self) -> BoxFuture<'_, Result<(), CoreError>> {
        adapt_operation(async move {
            self.backend
                .prompt
                .sample_epoch(self.cancellation)
                .await
                .map_err(Into::into)
        })
    }

    fn get_exit_blockers(&mut self) -> BoxFuture<'_, Result<Vec<String>, CoreError>> {
        adapt_operation(async move {
            let _guard = self.backend.state.lock().await;
            self.binding
                .exit_blocker_sources
                .lock()
                .expect("completion sources poisoned")
                .clear();
            let mut sources = Vec::new();
            self.work
                .join_mutations()
                .await
                .map_err(|error| TurnError::Service(error.to_string()))?;
            let mut blockers = Vec::new();
            for source in &self.backend.completion_sources {
                if let Some(reminder) = source.get_pending_ask_reminder(self.cancellation).await? {
                    blockers.push(reminder);
                    sources.push((source.clone(), CompletionFeedbackKind::PendingAsks));
                }
            }
            for source in &self.backend.completion_sources {
                if let Some(reminder) = source.get_active_work_reminder(self.cancellation).await? {
                    blockers.push(reminder);
                    sources.push((source.clone(), CompletionFeedbackKind::ActiveWork));
                }
            }
            for source in &self.backend.completion_sources {
                if let Some(reminder) = source.get_exit_reminder(self.cancellation).await? {
                    blockers.push(reminder);
                    sources.push((source.clone(), CompletionFeedbackKind::ExitReminders));
                }
            }
            *self
                .binding
                .exit_blocker_sources
                .lock()
                .expect("completion sources poisoned") = sources;
            Ok(blockers)
        })
    }

    fn render_status<'a, 'c>(
        &'a self,
        context: CoreContextSnapshot,
        cancellation: parrot_core::CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<String, CoreError>>
    where
        'c: 'a,
    {
        adapt_operation(async move {
            let status = self
                .backend
                .state
                .lock()
                .await
                .request_status
                .clone()
                .ok_or_else(|| TurnError::Service("request status is unavailable".into()))?;
            status
                .observe(
                    &crate::compaction::ContextSnapshot {
                        estimated_tokens: context.estimated_tokens,
                        context_limit: context.context_limit,
                        usage_percent: context.usage_percent,
                        trigger_percent: context.trigger_percent,
                        input_limit: context.input_limit,
                        trigger_tokens: context.trigger_tokens,
                        has_context_limit_override: context.has_context_limit_override,
                    },
                    &cancellation,
                )
                .await
                .map_err(Into::into)
        })
    }
    fn spill_group<'a, 'c>(
        &'a self,
        messages: &'a [parrot_core::Message],
        cancellation: parrot_core::CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<Vec<parrot_core::Message>, CoreError>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            let mut state = self.backend.state.lock().await;
            let selection = self.binding.capture_selection().map_err(CoreError::from)?;
            crate::compaction::CompactionEstimates::new(&self.backend.compactor, &selection.model)
                .spill_group(state.context.borrow_blobs(), messages, cancellation)
                .map_err(Into::into)
        })
    }

    fn report_compaction_progress<'a, 'c>(
        &'a self,
        progress: parrot_core::compaction::CompactionProgress,
        cancellation: parrot_core::CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<(), CoreError>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            crate::compaction::CompactionObserver::observe_progress(
                &ContextEmitter {
                    backend: self.backend,
                },
                protocol::CompactionProgressEvent {
                    phase: protocol::CompactionPhase::COMPACTION_PHASE_SUMMARIZING.into(),
                    completed_groups: progress.completed_groups as u64,
                    total_groups: progress.total_groups as u64,
                    ..protocol::CompactionProgressEvent::new()
                },
                &cancellation,
            )
            .await
            .map_err(crate::compaction::CompactionError::from)
            .map_err(Into::into)
        })
    }

    fn read_system_prompts(&mut self) -> BoxFuture<'_, Result<Vec<(String, String)>, CoreError>> {
        adapt_operation(async move {
            let backend = self.backend;
            let selection = self.binding.capture_selection()?;
            {
                let mut state = backend.state.lock().await;
                state.request_status = Some(backend.status.bind(&selection));
                self.cancellation
                    .check()
                    .map_err(|_| ContextError::Cancelled)?;
                state
                    .context
                    .ensure_epoch(&backend.prompt, self.cancellation)
                    .await?;
            }
            backend
                .prompt
                .read_system_prompts(&selection.model, self.cancellation)
                .await
                .map_err(ContextError::from)
                .map_err(Into::into)
        })
    }
    fn read_template_arguments<'a>(
        &'a self,
        source_key: &'a str,
    ) -> BoxFuture<'a, Result<Vec<(String, serde_json::Value)>, CoreError>> {
        adapt_operation(async move {
            self.backend
                .prompt
                .read_template_arguments(source_key, self.cancellation)
                .await
                .map_err(ContextError::from)
                .map_err(Into::into)
        })
    }
    fn read_agents_files(&self) -> BoxFuture<'_, Result<Vec<(String, String)>, CoreError>> {
        adapt_operation(async move {
            self.backend
                .prompt
                .read_agents_files(self.cancellation)
                .await
                .map_err(ContextError::from)
                .map_err(Into::into)
        })
    }
    fn read_agent_identity(
        &self,
    ) -> BoxFuture<'_, Result<Option<parrot_core::AgentIdentityDescription>, CoreError>> {
        adapt_operation(async move {
            self.backend
                .prompt
                .read_agent_identity(self.cancellation)
                .await
                .map_err(ContextError::from)
                .map_err(Into::into)
        })
    }
    fn read_standing_instruction(&self) -> BoxFuture<'_, Result<String, CoreError>> {
        adapt_operation(async move {
            self.backend
                .prompt
                .read_standing_instruction(self.cancellation)
                .await
                .map_err(ContextError::from)
                .map_err(Into::into)
        })
    }
    fn read_skill_catalog(
        &self,
    ) -> BoxFuture<'_, Result<Vec<parrot_core::SkillCatalogEntry>, CoreError>> {
        adapt_operation(async move {
            self.backend
                .prompt
                .read_skill_catalog(self.cancellation)
                .await
                .map_err(ContextError::from)
                .map_err(Into::into)
        })
    }
    fn read_tool_recovery(&self) -> BoxFuture<'_, Result<parrot_core::ToolRecovery, CoreError>> {
        adapt_operation(async move {
            self.backend
                .state
                .lock()
                .await
                .tool_conversation
                .read_recovery()
                .await
        })
    }
    fn write_tool_watermark(&mut self, through: i64) -> BoxFuture<'_, Result<(), CoreError>> {
        adapt_operation(async move {
            self.backend
                .state
                .lock()
                .await
                .tool_conversation
                .advance_watermark(through);
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
    ) -> BoxFuture<'_, Result<Vec<parrot_core::PromotedInput>, CoreError>> {
        adapt_operation(async move { self.backend.promote(admission).await })
    }
    fn get_skills<'a>(
        &'a self,
        references: &'a [parrot_core::SkillReference],
    ) -> BoxFuture<'a, Result<Vec<Option<parrot_core::Skill>>, CoreError>> {
        adapt_operation(async move {
            self.backend
                .skills
                .get_skills(references, self.cancellation)
                .await
                .map_err(TurnError::from)
        })
    }
    fn read_skill_content<'a>(
        &'a self,
        skill: &'a parrot_core::Skill,
    ) -> BoxFuture<'a, Result<parrot_core::SkillContent, CoreError>> {
        adapt_operation(async move {
            self.backend
                .skills
                .read_skill_content(skill, self.cancellation)
                .await
                .map_err(TurnError::from)
        })
    }
    fn read_token_calibration(&self) -> BoxFuture<'_, Result<TokenCalibration, CoreError>> {
        adapt_operation(async move {
            let state = self.backend.state.lock().await;
            Ok(state.context.budget().capture_state())
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
    fn read_conversation_state(
        &self,
    ) -> BoxFuture<'_, Result<parrot_core::compaction::ConversationState, CoreError>> {
        adapt_operation(async move {
            let state = self.backend.state.lock().await;
            state
                .context
                .read_conversation_state(&state.conversation)
                .await
                .map_err(Into::into)
        })
    }
    fn record_compaction<'a>(
        &'a mut self,
        instructions: &'a str,
        definitions: &'a [ToolDefinition],
        result: Result<Option<parrot_core::compaction::FittedCompaction>, CoreError>,
    ) -> BoxFuture<'a, Result<CoreContextCompaction, CoreError>> {
        Box::pin(async move {
            let definitions: Vec<sessions::Tool> =
                definitions.iter().cloned().map(Into::into).collect();
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
                        tools: &definitions,
                        conversation: &mut state.conversation,
                        prompt: &backend.prompt,
                        sessions: &backend.sessions,
                        status: status.as_ref(),
                        events: &events,
                    },
                    instructions.into(),
                    Ok(compacted),
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
    ) -> BoxFuture<'a, Result<Vec<parrot_core::ToolExecutionResult>, CoreError>> {
        adapt_operation(async move {
            let backend = self.backend;
            let selection = self.binding.capture_selection()?;
            let snapshot = {
                let guard = self.captured.lock().expect("captured tools poisoned");
                let tools = guard
                    .as_ref()
                    .ok_or_else(|| TurnError::Service("turn tools are unavailable".into()))?;
                if request.flags.final_phase {
                    tools.final_phase.clone()
                } else {
                    tools.normal.clone()
                }
            };
            let mut guard = backend.state.lock().await;
            let state = &mut *guard;
            let status = state
                .request_status
                .clone()
                .ok_or_else(|| TurnError::Service("request status is unavailable".into()))?;
            let events = ContextEmitter { backend };
            let chunk = backend
                .tools
                .execute(
                    ToolExecutionChunk {
                        assistant_sequence: request.receipt.assistant_sequence,
                        starts_batch: request.starts_batch,
                        calls: request.calls,
                        cancel_remaining: request.cancel_remaining,
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
            Ok(chunk
                .into_iter()
                .map(|execution| parrot_core::ToolExecutionResult {
                    result: execution.result,
                    yielded_process: execution.yielded_process.map(|value| {
                        parrot_core::YieldedProcess {
                            process_id: value.process_id,
                            name: value.name,
                            inventory_instance_id: value.inventory_instance_id,
                            visible_revision: value.visible_revision,
                            stdout_path: value.stdout_path,
                            stderr_path: value.stderr_path,
                            activity_kind: value.activity_kind.value(),
                        }
                    }),
                })
                .collect())
        })
    }
}

impl<S, T> parrot_core::Provider for RuntimeProvider<'_, S, T>
where
    S: AccessSnapshot + 'static,
    T: TurnTools<S> + 'static,
    T::Snapshot: 'static,
{
    fn read_model_aliases(
        &self,
    ) -> BoxFuture<'_, Result<Vec<parrot_core::ModelAliasDescription>, CoreError>> {
        adapt_operation(async move {
            let selection = self.binding.capture_selection()?;
            self.backend
                .prompt
                .read_model_aliases(&selection.model, self.cancellation)
                .await
                .map_err(ContextError::from)
                .map_err(Into::into)
        })
    }
    fn read_model_system_prompt(&self) -> BoxFuture<'_, Result<Option<String>, CoreError>> {
        adapt_operation(async move {
            let selection = self.binding.capture_selection()?;
            self.backend
                .prompt
                .read_model_system_prompt(&selection.model, self.cancellation)
                .await
                .map_err(ContextError::from)
                .map_err(Into::into)
        })
    }
    fn estimate_input_tokens(
        &self,
        instructions: &str,
        definitions: &[ToolDefinition],
        messages: &[parrot_core::Message],
    ) -> Result<i64, CoreError> {
        let selection = self.binding.capture_selection().map_err(CoreError::from)?;
        crate::compaction::CompactionEstimates::new(&self.backend.compactor, &selection.model)
            .estimate_context_tokens(instructions, definitions, messages)
            .map_err(Into::into)
    }
    fn estimate_tokens(&self, messages: &[parrot_core::Message]) -> Result<i64, CoreError> {
        let selection = self.binding.capture_selection().map_err(CoreError::from)?;
        crate::compaction::CompactionEstimates::new(&self.backend.compactor, &selection.model)
            .estimate_tokens(messages)
            .map_err(Into::into)
    }
    fn estimate_summary_tokens(
        &self,
        preceding_summary: &str,
        chunk_tokens: i64,
        next_group: &[parrot_core::Message],
    ) -> Result<i64, CoreError> {
        let selection = self.binding.capture_selection().map_err(CoreError::from)?;
        crate::compaction::CompactionEstimates::new(&self.backend.compactor, &selection.model)
            .estimate_summary_tokens(preceding_summary, chunk_tokens, next_group)
            .map_err(Into::into)
    }
    fn summarize<'a, 'c>(
        &'a self,
        request: ProviderRequest,
        cancellation: parrot_core::CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<parrot_core::compaction::SummaryResponse, CoreError>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            let selection = self.binding.capture_selection().map_err(CoreError::from)?;
            let provider = crate::compaction::SummaryProviderAdapter::new(
                selection.model.canonical_model,
                &self.backend.sessions,
                ContextEmitter {
                    backend: self.backend,
                },
            );
            provider
                .summarize_native(request, cancellation)
                .await
                .map_err(Into::into)
        })
    }

    fn invoke<'a, 'c>(
        &'a self,
        request: ProviderRequest,
        cancellation: parrot_core::CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<ProviderResponse, CoreError>>
    where
        'c: 'a,
    {
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
            let event = match call.stream(session.as_ref(), request, &cancellation).await {
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

pub(super) async fn interrupt<S, T>(
    backend: &RuntimeBackend<S, T>,
    work: &dyn DrainWork,
    key: &TurnKey,
) -> Result<parrot_core::ConversationReceipt, TurnError>
where
    S: AccessSnapshot + 'static,
    T: TurnTools<S> + 'static,
    T::Snapshot: 'static,
{
    let binding = TurnBinding::new();
    let mut budget = parrot_core::TurnBudget::new(backend.agent_limits.disable_max_turns);
    let mut states = RuntimeStates::new(backend, &binding, &NeverCancelled, work, &mut budget);
    states.persistence_key = Some(key.clone());
    parrot_core::interrupt_turn(&mut states)
        .await
        .map_err(TurnError::Core)
}
