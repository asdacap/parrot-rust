//! Actual durable turn loop behind the session-owned lifecycle.
use super::{
    budget::TurnBudget,
    lifecycle::{
        DrainBackend, DrainPass, DrainWork, Execution, ExecutionResultReference, ExecutionStatus,
        LifecycleError,
    },
    provider_call::{self, OutputWriter, ProviderActivity, ProviderCall, ProviderUsage},
    services::*,
    usage::SessionUsageRecorder,
};
use crate::{
    active_work::exit_reminders::{ExitReminders, ReminderError},
    compaction::{
        ContextController, ContextError, ContextEvents, ContextResult, EpochInput, RetryObserver,
        StatusInjection, provider_message,
    },
    event_broker::{BrokerError, EventBroker},
    prompt_context::{SessionPrompt, SystemPrompt},
};
use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationError},
        events::EventPublisher,
        security::AccessSnapshot,
        storage::Committed,
    },
    protocol::messages::parrot::{self as protocol, Event},
    values::{
        accounting::AgentSessionStatistics,
        agent_state::{ExecutionCheckpoint, ExecutionOutcome, ExecutionTerminal},
    },
};
use parrot_foundation::{
    config::runtime::AgentLimitsConfig, template_catalog::PromptTemplateCatalog,
};
use parrot_provider::sessions::{self, ProviderSessions};
use parrot_storage::{
    conversation::{
        AgentConversation, AgentHistoryView, ConversationReadView, ModelContent, ModelMessage,
    },
    events::{
        Admission, CloseTurnError, ConversationMessage, ConversationOrigin, ConversationPart,
        EventError, MessageRole, NeverCancelled, ProviderContent, TurnClosure, TurnKey,
    },
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Mutex;

pub type Publisher = dyn EventPublisher<Event = Event, Error = BrokerError>;

pub struct RuntimeConfig<S: AccessSnapshot, T: TurnTools<S>> {
    pub identity: SessionIdentity,
    pub agent_limits: AgentLimitsConfig,
    pub history: AgentHistoryView,
    pub broker: Arc<EventBroker>,
    pub publisher: Arc<Publisher>,
    pub selection: Arc<dyn SelectionSource<S>>,
    pub tools: Arc<T>,
    pub prompt: Box<dyn SystemPrompt>,
    pub skills: Arc<dyn TurnSkills>,
    pub context: ContextController,
    pub status: Arc<dyn TurnStatus<S>>,
    pub completions: Vec<Arc<dyn TurnCompletion<S>>>,
    pub templates: Arc<PromptTemplateCatalog>,
    pub reminders: Arc<ExitReminders<Publisher>>,
    pub output: Arc<dyn SessionOutput>,
    pub activity: Arc<dyn SessionActivity>,
    pub parent_completion: Arc<dyn ParentCompletion>,
    pub ancestors: Vec<(String, Arc<AgentSessionStatistics>)>,
    pub event_id: Arc<dyn Fn() -> String + Send + Sync>,
}

struct State {
    conversation: AgentConversation,
    context: ContextController,
    turn: Option<TurnKey>,
    diagnostic_turn: Option<String>,
}

/// A real storage/provider backend. No service has a success default, and no
/// concrete filesystem/security implementation is constructed by the drain.
pub struct RuntimeBackend<S: AccessSnapshot, T: TurnTools<S>> {
    identity: SessionIdentity,
    agent_limits: AgentLimitsConfig,
    history: AgentHistoryView,
    broker: Arc<EventBroker>,
    publisher: Arc<Publisher>,
    selection: Arc<dyn SelectionSource<S>>,
    tools: Arc<T>,
    skills: Arc<dyn TurnSkills>,
    status: Arc<dyn TurnStatus<S>>,
    completions: Vec<Arc<dyn TurnCompletion<S>>>,
    output: Arc<dyn SessionOutput>,
    activity: Arc<dyn SessionActivity>,
    parent_completion: Arc<dyn ParentCompletion>,
    event_id: Arc<dyn Fn() -> String + Send + Sync>,
    sessions: ProviderSessions,
    usage: SessionUsageRecorder,
    reminders: Arc<ExitReminders<Publisher>>,
    interruption: AtomicBool,
    suspending: AtomicBool,
    state: Mutex<State>,
    prompt: SessionPrompt,
    conversation: ConversationReadView,
    materialized_tools: std::sync::Mutex<Option<T::Snapshot>>,
    planner: Arc<crate::compaction::CompactionPlanner>,
    notices: Notices,
    execution_activity: std::sync::Mutex<Option<i64>>,
}
struct Notices {
    runaway: String,
    truncated: String,
    invalid: String,
    final_request: String,
    interrupted_final: String,
    restored: String,
    interrupted: String,
}

impl<S, T> RuntimeBackend<S, T>
where
    S: AccessSnapshot + 'static,
    T: TurnTools<S> + 'static,
    T::Snapshot: 'static,
{
    pub async fn new(config: RuntimeConfig<S, T>) -> Result<Arc<Self>, TurnError> {
        Self::with_sessions(config, ProviderSessions::new()).await
    }

    pub async fn with_sessions(
        config: RuntimeConfig<S, T>,
        sessions: ProviderSessions,
    ) -> Result<Arc<Self>, TurnError> {
        if config.identity.session_id != config.history.agent_session_id() {
            return Err(TurnError::Service(
                "session identity must match its bound history".into(),
            ));
        }
        let conversation = AgentConversation::new(config.history.clone()).await?;
        let usage = SessionUsageRecorder::new(
            config.history.clone(),
            config.identity.parent_session_id.clone(),
            config.broker.clone(),
            config.ancestors,
        )
        .await?;
        let reminders = config.reminders;
        let render = |key: &str| {
            config
                .templates
                .render(key, &[], &NeverCancelled)
                .map_err(|e| TurnError::Service(e.to_string()))
        };
        let notices = Notices {
            runaway: render("agent-session.runaway")?,
            truncated: render("agent-session.truncated-tool-call")?,
            invalid: render("agent-session.invalid-tool-call")?,
            final_request: render("agent-session.final-request")?,
            interrupted_final: render("agent-session.final-request-interrupted")?,
            restored: render("agent-session.tools-restored")?,
            interrupted: render("agent-session.interrupted-note")?,
        };
        Ok(Arc::new(Self {
            identity: config.identity,
            agent_limits: config.agent_limits,
            history: config.history,
            broker: config.broker,
            publisher: config.publisher,
            selection: config.selection,
            tools: config.tools,
            skills: config.skills,
            status: config.status,
            completions: config.completions,
            output: config.output,
            activity: config.activity,
            parent_completion: config.parent_completion,
            event_id: config.event_id,
            sessions,
            usage,
            reminders,
            interruption: AtomicBool::new(false),
            suspending: AtomicBool::new(false),
            prompt: SessionPrompt::new(config.prompt),
            conversation: conversation.read_view(),
            materialized_tools: std::sync::Mutex::new(None),
            planner: config.context.compactor().planner_handle(),
            state: Mutex::new(State {
                conversation,
                context: config.context,
                turn: None,
                diagnostic_turn: None,
            }),
            notices,
            execution_activity: std::sync::Mutex::new(None),
        }))
    }
    pub fn current_selection(&self) -> Result<TurnSelection<S>, TurnError> {
        self.selection.capture()
    }
    pub fn advertised_tool_definitions(
        &self,
        selection: &TurnSelection<S>,
    ) -> Result<Vec<sessions::Tool>, TurnError> {
        Ok(self.tools.definitions(&self.snapshot(selection)?).to_vec())
    }
    /// Capture current actual inputs without waiting for the drain, tools or summary.
    pub async fn observe_context(
        &self,
        cancellation: &dyn Cancellation,
    ) -> Result<super::observation::ObservedContext<S>, TurnError> {
        let selection = self.current_selection()?;
        let context = self
            .prompt
            .capture(&selection.model, cancellation, |instructions| {
                let tools = self.advertised_tool_definitions(&selection)?;
                let messages = self
                    .conversation
                    .messages()
                    .iter()
                    .map(provider_message)
                    .collect::<Vec<_>>();
                self.planner
                    .estimate_selected_context(&selection.model, instructions, &tools, &messages)
                    .map_err(TurnError::Planning)
            })
            .await??;
        Ok(super::observation::ObservedContext { selection, context })
    }
    pub fn request_final_phase(&self) {
        self.interruption.store(true, Ordering::Release);
    }
    pub fn history(&self) -> &AgentHistoryView {
        &self.history
    }
    pub fn reminders(&self) -> &Arc<ExitReminders<Publisher>> {
        &self.reminders
    }
    pub fn usage(&self) -> &SessionUsageRecorder {
        &self.usage
    }
    pub fn broker(&self) -> &Arc<EventBroker> {
        &self.broker
    }
    pub async fn listen<'a, I, IF>(
        &'a self,
        cursor: Option<String>,
        capture_scopes: I,
    ) -> Result<
        impl parrot_domain::contracts::events::EventSubscription<
            Event = Event,
            Error = crate::event_broker::ListenError<EventError>,
        > + 'a,
        EventError,
    >
    where
        I: FnOnce() -> IF + Send + 'a,
        IF: std::future::Future<Output = Result<Vec<Event>, EventError>> + Send + 'a,
    {
        self.broker
            .listen(
                cursor.as_deref(),
                || self.history.repository().events().replay(),
                move || async move {
                    let mut snapshots = capture_scopes().await?;
                    let mut usage = Event::new();
                    usage.set_session_usage_snapshot(self.usage.usage_snapshot());
                    snapshots.push(usage);
                    Ok(snapshots)
                },
            )
            .await
    }
    pub async fn compact_now(
        &self,
        selection: &TurnSelection<S>,
        target: Option<&crate::compaction::ContextSize>,
        cancellation: &dyn Cancellation,
    ) -> Result<crate::compaction::ContextCompactionResult, TurnError> {
        let mut guard = self.state.lock().await;
        let state = &mut *guard;
        let snapshot = self.snapshot(selection)?;
        let definitions = self.tools.definitions(&snapshot).to_vec();
        let status = self.status.bind(selection);
        let events = ContextEmitter { backend: self };
        state
            .context
            .compact(
                EpochInput {
                    selection: &selection.model,
                    tools: &definitions,
                    conversation: &mut state.conversation,
                    prompt: &self.prompt,
                    sessions: &self.sessions,
                    status: status.as_ref(),
                    events: &events,
                },
                target,
                cancellation,
            )
            .await
            .map_err(TurnError::Context)
    }
    fn event(&self) -> Event {
        Event {
            id: (self.event_id)(),
            agent_session_id: self.identity.session_id.clone(),
            ..Event::new()
        }
    }
    async fn emit(&self, event: Event, cancellation: &dyn Cancellation) -> Result<(), TurnError> {
        self.history.append(event.clone(), None, None).await?;
        self.publish(event, cancellation).await
    }
    async fn publish(
        &self,
        event: Event,
        cancellation: &dyn Cancellation,
    ) -> Result<(), TurnError> {
        self.publisher
            .publish_committed(&Committed::after_commit(event), cancellation)
            .await
            .map_err(TurnError::Publication)
    }
    fn snapshot(&self, selection: &TurnSelection<S>) -> Result<T::Snapshot, TurnError> {
        let mut materialized = self
            .materialized_tools
            .lock()
            .expect("tool catalog poisoned");
        if materialized.is_none() {
            *materialized = Some(self.tools.materialize()?);
        }
        self.tools.permitted(
            materialized.as_ref().expect("materialized above"),
            selection,
        )
    }
    async fn reconcile(
        &self,
        state: &mut State,
        cancellation: &dyn Cancellation,
    ) -> Result<TurnSelection<S>, TurnError> {
        let selection = self.selection.capture()?;
        let snapshot = self.snapshot(&selection)?;
        let status = self.status.bind(&selection);
        let events = ContextEmitter { backend: self };
        self.tools
            .reconcile(
                TurnResources {
                    history: &self.history,
                    conversation: &mut state.conversation,
                    context: &mut state.context,
                    prompt: &self.prompt,
                    sessions: &self.sessions,
                    status: status.as_ref(),
                    events: &events,
                    usage: &self.usage,
                },
                &selection,
                &snapshot,
                self.selection.as_ref(),
                cancellation,
            )
            .await?;
        Ok(selection)
    }
    async fn promote(
        &self,
        state: &mut State,
        cancellation: &dyn Cancellation,
    ) -> Result<(), TurnError> {
        let agent = self.identity.session_id.clone();
        let ids = self.event_id.clone();
        let mut promotions = self
            .history
            .promote_steers(move |input| {
                let mut event = Event {
                    id: ids(),
                    agent_session_id: agent.clone(),
                    ..Event::new()
                };
                event.set_input_promoted(protocol::InputPromoted {
                    input_id: input.id.clone(),
                    message_id: input.message_id.clone(),
                    ..Default::default()
                });
                Ok(event)
            })
            .await?;
        if promotions.is_empty() && !state.conversation.is_answerable() {
            let agent = self.identity.session_id.clone();
            let ids = self.event_id.clone();
            promotions = self
                .history
                .promote_next_queue(move |input| {
                    let mut event = Event {
                        id: ids(),
                        agent_session_id: agent.clone(),
                        ..Event::new()
                    };
                    event.set_input_promoted(protocol::InputPromoted {
                        input_id: input.id.clone(),
                        message_id: input.message_id.clone(),
                        ..Default::default()
                    });
                    Ok(event)
                })
                .await?;
        }
        for promotion in promotions {
            self.skills
                .select(&promotion.input.parts, cancellation)
                .await?;
            let contents = self.history.materialize(promotion.input.parts).await?;
            state.conversation.add(ModelMessage {
                role: MessageRole::User,
                contents,
                tool_calls: Vec::new(),
                tool_call_id: String::new(),
                provider_content: ProviderContent::default(),
            });
            self.publish(promotion.published, cancellation).await?;
        }
        let rendering = self.skills.render(cancellation).await?;
        if !rendering.content.is_empty() {
            let mut event = self.event();
            event.set_agent_notice(protocol::AgentNotice {
                kind: "skill_context".into(),
                ..Default::default()
            });
            self.history
                .append_message(
                    event.clone(),
                    Some(ConversationMessage::text(
                        MessageRole::User,
                        &rendering.content,
                    )),
                    ConversationOrigin::System,
                )
                .await?;
            state
                .conversation
                .add(text_message(MessageRole::User, rendering.content));
            self.publish(event, cancellation).await?;
            for path in rendering.loaded_paths {
                let mut event = self.event();
                event.set_skill_loaded(protocol::SkillLoadedEvent {
                    path,
                    ..Default::default()
                });
                self.emit(event, cancellation).await?;
            }
        }
        Ok(())
    }
    async fn retry_notice(
        &self,
        state: &mut State,
        reason: &str,
        attempt: i32,
        cancellation: &dyn Cancellation,
    ) -> Result<(), TurnError> {
        let mut event = self.event();
        event.set_retry_notice(protocol::RetryNotice {
            reason: reason.into(),
            attempt,
            ..Default::default()
        });
        self.history
            .append_message(
                event.clone(),
                Some(ConversationMessage::text(MessageRole::System, reason)),
                ConversationOrigin::System,
            )
            .await?;
        state
            .conversation
            .add(text_message(MessageRole::System, reason.into()));
        self.publish(event, cancellation).await
    }
    async fn fail(&self, message: String, body: String) -> Result<(), TurnError> {
        let mut event = self.event();
        event.set_turn_failed(protocol::TurnFailed {
            message,
            provider_response_body: body,
            ..Default::default()
        });
        self.emit(event, &NeverCancelled).await
    }
    async fn close_turn(
        &self,
        state: &mut State,
        completion: &sessions::Completion,
        cancellation: &dyn Cancellation,
    ) -> Result<(), TurnError> {
        let Some(key) = state.turn.clone() else {
            return Ok(());
        };
        let totals = self.usage.capture().own.totals;
        let mut event = self.event();
        event.set_turn_ended(protocol::TurnEnded {
            finish_reason: completion.finish_reason.clone(),
            input_tokens: totals.input_tokens,
            output_tokens: totals.output_tokens,
            ..Default::default()
        });
        let message = conversation_message(completion);
        let model = model_assistant(completion);
        let active = &mut state.turn;
        let conversation = &mut state.conversation;
        let publisher = PublisherRef(self.publisher.as_ref());
        let result = self
            .history
            .repository()
            .events()
            .close_turn_and_publish(
                key.clone(),
                event,
                Some(message),
                &publisher,
                cancellation,
                |receipt| {
                    // This callback runs after durable commit and BEFORE publication.
                    if active.as_ref() == Some(&receipt.key) {
                        *active = None;
                    }
                    if receipt.closure == TurnClosure::NewlyClosed {
                        conversation.add(model);
                    }
                },
            )
            .await;
        let committed = match &result {
            Ok(receipt) => Some(receipt),
            Err(CloseTurnError::Publication { committed, .. }) => Some(committed.as_ref()),
            _ => None,
        };
        if let Some(receipt) = committed {
            self.history.refresh().await?;
            if receipt.closure == TurnClosure::AlreadyClosed {
                state.conversation.reload().await?;
            }
        }
        match result {
            Ok(_) => Ok(()),
            Err(CloseTurnError::AdmissionCancelled) => Err(TurnError::Cancelled),
            Err(CloseTurnError::Repository(error)) => Err(error.into()),
            Err(CloseTurnError::Publication { error, .. }) => Err(TurnError::Publication(error)),
        }
    }
    async fn run_pass(
        &self,
        state: &mut State,
        cancellation: &dyn Cancellation,
        work: &dyn DrainWork,
    ) -> Result<Execution, TurnError> {
        let mut answer = String::new();
        let mut budget = TurnBudget::new(self.agent_limits.disable_max_turns);
        let mut retry_pending = false;
        let mut compacted_oversized = false;
        let mut selected: Option<TurnSelection<S>> = None;
        let mut active_tools: Option<T::Snapshot> = None;
        let events = ContextEmitter { backend: self };
        loop {
            if state.turn.is_none() && work.has_scheduled() {
                return Ok(Execution::succeeded(answer));
            }
            self.reconcile(state, cancellation).await?;
            cancellation.check().map_err(|_| TurnError::Cancelled)?;
            if !retry_pending
                && !state.conversation.is_answerable()
                && !self.history.has_pending_inputs().await?
            {
                return Ok(Execution::succeeded(answer));
            }
            if state.turn.is_none() {
                self.skills.end_turn().await;
                let mut selection = self.selection.capture()?;
                budget.begin_turn();
                self.sessions.begin_turn().await?;
                let mut started = self.event();
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
                    agent_session_id: self.identity.session_id.clone(),
                    start_event_id: started.id.clone(),
                });
                state.diagnostic_turn = Some(started.id.clone());
                self.activity.turn_started(&started.id);
                self.emit(started, cancellation).await?;
                state
                    .context
                    .ensure_epoch(&self.prompt, cancellation)
                    .await?;
                loop {
                    let snapshot = self.snapshot(&selection)?;
                    let definitions = self.tools.definitions(&snapshot).to_vec();
                    let status = self.status.bind(&selection);
                    let outcome = state
                        .context
                        .inject_status(
                            EpochInput {
                                selection: &selection.model,
                                tools: &definitions,
                                conversation: &mut state.conversation,
                                prompt: &self.prompt,
                                sessions: &self.sessions,
                                status: status.as_ref(),
                                events: &events,
                            },
                            self.identity.depth,
                            &selection.profile.id,
                            cancellation,
                        )
                        .await?;
                    if matches!(outcome, StatusInjection::Complete) {
                        break;
                    }
                    selection = self.selection.capture()?;
                    tokio::task::yield_now().await;
                }
                self.skills.begin_turn(cancellation).await?;
                active_tools = Some(self.snapshot(&selection)?);
                state
                    .context
                    .restore_tool_availability(
                        &mut state.conversation,
                        &self.notices.restored,
                        &events,
                        cancellation,
                    )
                    .await?;
                selected = Some(selection);
            }
            self.promote(state, cancellation).await?;
            let selection = selected
                .as_ref()
                .ok_or_else(|| TurnError::Service("turn selection is unavailable".into()))?;
            let active = active_tools
                .as_ref()
                .ok_or_else(|| TurnError::Service("turn tools are unavailable".into()))?;
            let Some(phase) = budget.admit(
                selection.profile.maximum_requests,
                self.interruption.swap(false, Ordering::AcqRel),
            ) else {
                self.fail(self.notices.runaway.clone(), String::new())
                    .await?;
                return Ok(Execution::failed(&self.notices.runaway));
            };
            let snapshot = if phase.final_phase {
                self.tools.enabled_after_interruption(active)
            } else {
                active.clone()
            };
            let definitions = self.tools.definitions(&snapshot).to_vec();
            if phase.enters_final_phase {
                let content = if phase.interrupted {
                    &self.notices.interrupted_final
                } else {
                    &self.notices.final_request
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
            let status = self.status.bind(selection);
            let epoch = state
                .context
                .prepare_epoch(
                    EpochInput {
                        selection: &selection.model,
                        tools: &definitions,
                        conversation: &mut state.conversation,
                        prompt: &self.prompt,
                        sessions: &self.sessions,
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
                &definitions,
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
                tools: definitions.clone(),
                max_output_tokens: maximum,
                reasoning: selection.model.canonical_model.reasoning(),
                ..Default::default()
            };
            let attempt = budget.count_request();
            let session = self
                .sessions
                .get(selection.model.canonical_model.provider.clone())
                .await?;
            let output = OutputBridge(self.output.as_ref());
            let activity = ActivityBridge(self.activity.as_ref());
            let mut usage = UsageBridge {
                recorder: &self.usage,
                selected: &selection.model.canonical_model,
                budget: state.context.budget_mut(),
                estimate,
            };
            let mut call = ProviderCall {
                history: &self.history,
                publisher: self.publisher.as_ref(),
                event_id: self.event_id.as_ref(),
                output: &output,
                activity: &activity,
                usage: &mut usage,
            };
            let completed = match call.stream(session.as_ref(), request, cancellation).await {
                Ok(event) => match event.base_event() {
                    sessions::Event::Completed(completed) => completed.clone(),
                    _ => unreachable!("provider call returns completion"),
                },
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
                ) if !compacted_oversized => {
                    compacted_oversized = true;
                    let compacted = state
                        .context
                        .compact_epoch(
                            EpochInput {
                                selection: &selection.model,
                                tools: &definitions,
                                conversation: &mut state.conversation,
                                prompt: &self.prompt,
                                sessions: &self.sessions,
                                status: status.as_ref(),
                                events: &events,
                            },
                            None,
                            cancellation,
                        )
                        .await?;
                    if !compacted.reduced {
                        return Err(error.into());
                    }
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            compacted_oversized = false;
            if completed.finish_reason == "length" && !completed.tool_calls.is_empty() {
                self.retry_notice(state, &self.notices.truncated, attempt, cancellation)
                    .await?;
                continue;
            }
            if completed.tool_calls.iter().any(|call| {
                serde_json::from_str::<serde_json::Value>(&call.arguments_json).is_err()
            }) {
                self.retry_notice(state, &self.notices.invalid, attempt, cancellation)
                    .await?;
                continue;
            }
            if !completed.tool_calls.is_empty() {
                let published = self.event();
                self.history
                    .append_conversation(published.clone(), conversation_message(&completed))
                    .await?;
                state.conversation.add(model_assistant(&completed));
                self.publish(published, cancellation).await?;
                self.activity.assistant_message(&completed.assistant_text);
                let mut received = self.event();
                received.set_tool_request_received(protocol::ToolRequestReceived {
                    tool_call_count: completed.tool_calls.len() as i32,
                    ..Default::default()
                });
                self.emit(received, cancellation).await?;
                self.tools
                    .reconcile(
                        TurnResources {
                            history: &self.history,
                            conversation: &mut state.conversation,
                            context: &mut state.context,
                            prompt: &self.prompt,
                            sessions: &self.sessions,
                            status: status.as_ref(),
                            events: &events,
                            usage: &self.usage,
                        },
                        selection,
                        &snapshot,
                        self.selection.as_ref(),
                        cancellation,
                    )
                    .await?;
                if phase.final_phase && !budget.try_extend_settlement() {
                    self.fail(self.notices.runaway.clone(), String::new())
                        .await?;
                    return Ok(Execution::failed(&self.notices.runaway));
                }
                continue;
            }
            work.join_mutations()
                .await
                .map_err(|e| TurnError::Service(e.to_string()))?;
            let candidate = CompletionCandidate {
                session_id: self.identity.session_id.clone(),
                message_id: parrot_domain::values::identifiers::Identifier::message_id()
                    .map_err(|error| TurnError::Service(error.to_string()))?,
                assistant: completed.clone().into_message(),
                selection: selection.clone(),
            };
            let mut retry = None;
            for callback in &self.completions {
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
                    state.conversation.add(model_assistant(&completed));
                    self.activity.assistant_message(&completed.assistant_text);
                }
                state
                    .conversation
                    .add(text_message(MessageRole::System, system_message));
                if let Some(pending) = completion_retry_pending {
                    retry_pending = pending;
                }
                budget.restart();
                continue;
            }
            work.join_mutations()
                .await
                .map_err(|e| TurnError::Service(e.to_string()))?;
            answer = completed.assistant_text.clone();
            retry_pending = false;
            self.close_turn(state, &completed, cancellation).await?;
            if let Some(turn) = state.diagnostic_turn.take() {
                self.activity.turn_finished(&turn, "completed", None);
            }
            self.activity.assistant_message(&completed.assistant_text);
            selected = None;
            active_tools = None;
        }
    }
}

impl<S, T> super::lifecycle::AgentSession<RuntimeBackend<S, T>>
where
    S: AccessSnapshot + 'static,
    T: TurnTools<S> + 'static,
    T::Snapshot: 'static,
{
    /// Accepts an owned mutation; dropping its observer cannot drop an admitted
    /// SQLite commit or leave the live reminder instance stale.
    pub fn set_exit_reminder(
        &self,
        title: String,
        description: String,
        caller: Arc<dyn Cancellation>,
    ) -> Result<super::lifecycle::OperationHandle<()>, LifecycleError> {
        let publication = caller.clone();
        self.owned_mutation(
            move |backend, _lifetime| {
                Box::pin(async move {
                    backend
                        .reminders
                        .set(&title, &description, publication.as_ref())
                        .await
                        .map_err(reminder_lifecycle_error)
                })
            },
            caller,
        )
    }
    pub fn clear_exit_reminder(
        &self,
        title: String,
        caller: Arc<dyn Cancellation>,
    ) -> Result<super::lifecycle::OperationHandle<bool>, LifecycleError> {
        let publication = caller.clone();
        self.owned_mutation(
            move |backend, _lifetime| {
                Box::pin(async move {
                    backend
                        .reminders
                        .clear(&title, publication.as_ref())
                        .await
                        .map_err(reminder_lifecycle_error)
                })
            },
            caller,
        )
    }
    /// External context control is serialized as scheduled work. An executing
    /// tool uses its borrowed TurnResources instead; self-queueing would wait
    /// on the very drain whose tool result is outstanding.
    pub async fn compact_context(
        &self,
        target: Option<crate::compaction::ContextSize>,
        caller: Arc<dyn Cancellation>,
    ) -> Result<crate::compaction::ContextCompactionResult, TurnError> {
        let backend = self.backend().clone();
        let (send, receive) = tokio::sync::oneshot::channel();
        let work = self
            .schedule_work(
                move |selection, cancellation| {
                    Box::pin(async move {
                        let result = backend
                            .compact_now(&selection, target.as_ref(), cancellation.as_ref())
                            .await;
                        let outcome = result
                            .as_ref()
                            .map(|_| ())
                            .map_err(TurnError::lifecycle_error);
                        let _ = send.send(result);
                        outcome
                    })
                },
                caller.clone(),
            )
            .map_err(|e| match e {
                LifecycleError::Cancelled => TurnError::Cancelled,
                error => TurnError::Service(error.to_string()),
            })?;
        let waited = work.wait(caller.as_ref()).await;
        if caller.is_cancelled() {
            return Err(TurnError::Cancelled);
        }
        match receive.await {
            Ok(result) => result,
            Err(_) => Err(match waited {
                Err(LifecycleError::Cancelled) => TurnError::Cancelled,
                Err(error) => TurnError::Service(error.to_string()),
                Ok(()) => TurnError::Service("scheduled compaction ended without a result".into()),
            }),
        }
    }
}

fn reminder_lifecycle_error(error: ReminderError<BrokerError>) -> LifecycleError {
    match error {
        ReminderError::Publication(OperationError::Cancelled)
        | ReminderError::Template(OperationError::Cancelled) => LifecycleError::Cancelled,
        error => LifecycleError::Failed(error.to_string()),
    }
}

fn text_message(role: MessageRole, text: String) -> ModelMessage {
    ModelMessage {
        role,
        contents: vec![ModelContent::Text(text)],
        tool_calls: Vec::new(),
        tool_call_id: String::new(),
        provider_content: ProviderContent::default(),
    }
}
fn conversation_message(completed: &sessions::Completion) -> ConversationMessage {
    ConversationMessage {
        origin: ConversationOrigin::Model,
        role: MessageRole::Assistant,
        parts: vec![ConversationPart::text(&completed.assistant_text)],
        tool_calls: completed
            .tool_calls
            .iter()
            .map(|call| parrot_storage::events::ToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments_json: call.arguments_json.clone(),
            })
            .collect(),
        tool_call_id: String::new(),
        provider_content: ProviderContent {
            provider_id: completed.provider_id.clone(),
            content: completed.provider_content.clone(),
        },
    }
}
fn model_assistant(completed: &sessions::Completion) -> ModelMessage {
    let message = conversation_message(completed);
    ModelMessage {
        role: message.role,
        contents: vec![ModelContent::Text(completed.assistant_text.clone())],
        tool_calls: message.tool_calls,
        tool_call_id: message.tool_call_id,
        provider_content: message.provider_content,
    }
}
struct PublisherRef<'a>(&'a Publisher);
impl EventPublisher for PublisherRef<'_> {
    type Event = Event;
    type Error = BrokerError;
    fn publish_committed<'a>(
        &'a self,
        event: &'a Committed<Event>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<BrokerError>>> {
        self.0.publish_committed(event, cancellation)
    }
    fn publish_transient<'a>(
        &'a self,
        event: Event,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<BrokerError>>> {
        self.0.publish_transient(event, cancellation)
    }
}
struct OutputBridge<'a>(&'a dyn SessionOutput);
impl OutputWriter for OutputBridge<'_> {
    fn append<'a>(&'a self, text: &'a str) -> BoxFuture<'a, std::io::Result<()>> {
        self.0.append(text)
    }
}
struct ActivityBridge<'a>(&'a dyn SessionActivity);
impl ProviderActivity for ActivityBridge<'_> {
    fn begin_provider_request(&self) {
        self.0.begin_provider_request();
    }
    fn observe_provider_event(&self, event: &sessions::Event) {
        self.0.observe_provider_event(event);
    }
    fn finish_provider_request(&self) {
        self.0.finish_provider_request();
    }
}
struct UsageBridge<'a> {
    recorder: &'a SessionUsageRecorder,
    selected: &'a parrot_provider::routing::registry::ProviderModel,
    budget: &'a mut crate::compaction::ProviderTokenBudget,
    estimate: i64,
}
impl ProviderUsage for UsageBridge<'_> {
    fn observe_completion<'a>(
        &'a mut self,
        request: &'a sessions::Request,
        completed: &'a sessions::Completion,
    ) -> BoxFuture<'a, Result<(), EventError>> {
        Box::pin(async move {
            self.budget
                .observe_usage(&self.selected.selector(), self.estimate, completed);
            self.recorder
                .record_request(self.selected, request.reasoning.as_ref(), completed)
                .await
        })
    }
}
struct ContextEmitter<'a, S: AccessSnapshot, T: TurnTools<S>> {
    backend: &'a RuntimeBackend<S, T>,
}
impl<S, T> RetryObserver for ContextEmitter<'_, S, T>
where
    S: AccessSnapshot + 'static,
    T: TurnTools<S> + 'static,
    T::Snapshot: 'static,
{
    fn observe<'a>(
        &'a self,
        event: &'a sessions::Event,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), sessions::Error>> {
        Box::pin(async move {
            provider_call::emit_retry(
                &self.backend.history,
                self.backend.publisher.as_ref(),
                self.backend.event_id.as_ref(),
                event,
                cancellation,
            )
            .await
            .map_err(|error| match error {
                provider_call::ProviderCallError::Provider(error) => error,
                provider_call::ProviderCallError::Publication(OperationError::Cancelled) => {
                    sessions::Error::Cancelled
                }
                error => sessions::Error::Provider(error.to_string()),
            })
        })
    }
}
impl<S, T> ContextEvents for ContextEmitter<'_, S, T>
where
    S: AccessSnapshot + 'static,
    T: TurnTools<S> + 'static,
    T::Snapshot: 'static,
{
    fn emit<'a>(
        &'a self,
        event: Event,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<()>> {
        Box::pin(async move {
            self.backend
                .emit(event, cancellation)
                .await
                .map_err(|error| {
                    if error.is_cancelled() {
                        ContextError::Cancelled
                    } else {
                        ContextError::Operation(error.to_string())
                    }
                })
        })
    }
    fn publish_committed<'a>(
        &'a self,
        event: Committed<Event>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<()>> {
        Box::pin(async move {
            self.backend
                .publisher
                .publish_committed(&event, cancellation)
                .await
                .map_err(|error| match error {
                    OperationError::Cancelled => ContextError::Cancelled,
                    OperationError::Failed(error) => ContextError::Operation(error.to_string()),
                })
        })
    }
    fn publish<'a>(
        &'a self,
        event: Event,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<()>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| ContextError::Cancelled)?;
            self.backend.broker.publish_retained(&event);
            Ok(())
        })
    }
}

impl<S, T> RuntimeBackend<S, T>
where
    S: AccessSnapshot + 'static,
    T: TurnTools<S> + 'static,
    T::Snapshot: 'static,
{
    async fn emit_execution(
        &self,
        mut event: Event,
        correlation: &ExecutionCorrelation,
        suffix: &str,
    ) -> Result<(), LifecycleError> {
        if let ExecutionCorrelation::Correlated(correlation) = correlation {
            event.id = format!("{correlation}.{suffix}");
            if let Some(event) = self
                .history
                .repository()
                .events()
                .append_execution_event(event)
                .await
                .map_err(|error| LifecycleError::Failed(error.to_string()))?
            {
                self.publish(event, &NeverCancelled)
                    .await
                    .map_err(|error| error.lifecycle_error())?;
            }
            Ok(())
        } else {
            self.emit(event, &NeverCancelled)
                .await
                .map_err(|error| error.lifecycle_error())
        }
    }
}

enum ExecutionCorrelation {
    Legacy,
    Correlated(String),
}
impl<S, T> RuntimeBackend<S, T>
where
    S: AccessSnapshot + 'static,
    T: TurnTools<S> + 'static,
    T::Snapshot: 'static,
{
    fn start_execution(
        &self,
        correlation: ExecutionCorrelation,
    ) -> BoxFuture<'_, Result<(), LifecycleError>> {
        Box::pin(async move {
            *self
                .execution_activity
                .lock()
                .expect("execution activity poisoned") = Some(self.activity.begin_execution());
            let mut event = self.event();
            event.set_agent_started(protocol::AgentStarted {
                parent_agent_session_id: self.identity.parent_session_id.clone(),
                name: self.identity.name.clone(),
                ..Default::default()
            });
            self.emit_execution(event, &correlation, "started").await
        })
    }
    fn finish_execution<'a>(
        &'a self,
        execution: &'a Execution,
        correlation: ExecutionCorrelation,
    ) -> BoxFuture<'a, Result<(), LifecycleError>> {
        Box::pin(async move {
            let mut event = self.event();
            let elapsed = self
                .activity
                .execution_duration()
                .as_millis()
                .min(i64::MAX as u128) as i64;
            if execution.status == ExecutionStatus::Succeeded {
                event.set_agent_finished(protocol::AgentFinished {
                    parent_agent_session_id: self.identity.parent_session_id.clone(),
                    name: self.identity.name.clone(),
                    elapsed_ms: Some(elapsed),
                    ..Default::default()
                });
            } else {
                event.set_agent_failed(protocol::AgentFailed {
                    parent_agent_session_id: self.identity.parent_session_id.clone(),
                    name: self.identity.name.clone(),
                    message: execution.error.clone(),
                    ..Default::default()
                });
            }
            let publication = self.emit_execution(event, &correlation, "finished").await;
            if matches!(correlation, ExecutionCorrelation::Correlated(_)) && publication.is_err() {
                return publication;
            }
            let completed = publication.as_ref().map_or_else(
                |error| Execution::failed(error.to_string()),
                |_| execution.clone(),
            );
            if let Some(activity) = self
                .execution_activity
                .lock()
                .expect("execution activity poisoned")
                .take()
            {
                self.activity.finish_execution(activity, &completed);
            }
            if let ExecutionCorrelation::Correlated(correlation) = correlation {
                self.parent_completion
                    .deliver_correlated(&completed, &format!("{correlation}.parent"))
                    .await
                    .map_err(|error| error.lifecycle_error())?;
            } else {
                self.parent_completion
                    .deliver(&completed)
                    .await
                    .map_err(|error| error.lifecycle_error())?;
            }
            publication
        })
    }
}

impl<S, T> DrainBackend for RuntimeBackend<S, T>
where
    S: AccessSnapshot + 'static,
    T: TurnTools<S> + 'static,
    T::Snapshot: 'static,
{
    type Selection = TurnSelection<S>;
    fn admit(
        &self,
        parts: Vec<ConversationPart>,
        message_id: String,
        delivery: protocol::Delivery,
        source: protocol::InputSource,
    ) -> BoxFuture<'_, Result<Admission, LifecycleError>> {
        let ids = self.event_id.clone();
        let agent = self.identity.session_id.clone();
        Box::pin(async move {
            self.history
                .admit_parts(message_id, parts, delivery, move |input| {
                    let mut event = Event {
                        id: ids(),
                        agent_session_id: agent,
                        ..Default::default()
                    };
                    let parts = input
                        .parts
                        .iter()
                        .map(|part| {
                            let mut output = protocol::MessageContentPart::new();
                            match part {
                                ConversationPart::Text(text) => output.set_text(text.clone()),
                                ConversationPart::ImageArtifact { artifact_id, .. } => {
                                    output.set_artifact_id(artifact_id.clone())
                                }
                            };
                            output
                        })
                        .collect();
                    event.set_input_admitted(protocol::InputAdmitted {
                        input_id: input.id.clone(),
                        message_id: input.message_id.clone(),
                        content: input.content(),
                        delivery: input.delivery.into(),
                        source: source.into(),
                        parts,
                        ..Default::default()
                    });
                    Ok(event)
                })
                .await
                .map_err(|error| LifecycleError::Failed(error.to_string()))
        })
    }
    fn find_execution_input(
        &self,
        message_id: String,
    ) -> BoxFuture<'_, Result<Option<parrot_storage::events::AdmittedInput>, LifecycleError>> {
        Box::pin(async move {
            self.history
                .repository()
                .events()
                .find_input(self.identity.session_id.clone(), message_id)
                .await
                .map_err(|error| LifecycleError::Failed(error.to_string()))
        })
    }
    fn publish_admission<'a>(
        &'a self,
        admission: &'a Admission,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), LifecycleError>> {
        Box::pin(async move {
            if let Some(event) = &admission.published {
                self.publish(event.clone(), cancellation)
                    .await
                    .map_err(|e| e.lifecycle_error())?;
            }
            Ok(())
        })
    }
    fn has_recovery_work(&self) -> BoxFuture<'_, Result<bool, LifecycleError>> {
        Box::pin(async move {
            if self.state.lock().await.conversation.is_answerable() {
                return Ok(true);
            }
            self.has_pending_inputs().await
        })
    }
    fn has_pending_inputs(&self) -> BoxFuture<'_, Result<bool, LifecycleError>> {
        Box::pin(async move {
            self.history
                .has_pending_inputs()
                .await
                .map_err(|e| LifecycleError::Failed(e.to_string()))
        })
    }
    fn cancel_pending_input(&self, input_id: String) -> BoxFuture<'_, Result<(), LifecycleError>> {
        let agent = self.identity.session_id.clone();
        let ids = self.event_id.clone();
        let canceled = input_id.clone();
        Box::pin(async move {
            let event = self
                .history
                .cancel_pending_input(input_id, move || {
                    let mut event = Event {
                        id: ids(),
                        agent_session_id: agent,
                        ..Default::default()
                    };
                    event.set_input_canceled(protocol::InputCanceled {
                        input_id: canceled,
                        ..Default::default()
                    });
                    Ok(event)
                })
                .await
                .map_err(|e| LifecycleError::Failed(e.to_string()))?;
            if let Some(event) = event {
                self.publish(event, &NeverCancelled)
                    .await
                    .map_err(|e| e.lifecycle_error())?;
            }
            Ok(())
        })
    }
    fn update_selection(&self, selection: Self::Selection) -> Result<(), LifecycleError> {
        self.selection
            .update(selection)
            .map_err(|e| e.lifecycle_error())
    }
    fn drain_state_changed(&self, state: super::lifecycle::DrainState) {
        self.activity.change_state(state);
    }
    fn prepare_scheduled<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Self::Selection, LifecycleError>> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            self.reconcile(&mut state, cancellation)
                .await
                .map_err(|e| e.lifecycle_error())
        })
    }
    fn pass<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
        work: &'a dyn DrainWork,
    ) -> BoxFuture<'a, Result<DrainPass, LifecycleError>> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            let result = self.run_pass(&mut state, cancellation, work).await;
            let outcome = async {
                match result {
                    Ok(execution) => {
                        if execution.status == ExecutionStatus::Failed {
                            if let Some(turn) = state.diagnostic_turn.take() {
                                self.activity
                                    .turn_finished(&turn, "failed", Some("runaway"));
                            }
                        }
                        Ok(execution)
                    }
                    Err(error) if error.is_cancelled() => {
                        work.join_mutations().await?;
                        if let Some(turn) = state.diagnostic_turn.take() {
                            self.activity.turn_finished(&turn, "cancelled", None);
                        }
                        if state.turn.is_some() && !self.suspending.load(Ordering::Acquire) {
                            let completed = sessions::Completion {
                                assistant_text: self.notices.interrupted.clone(),
                                finish_reason: "interrupted".into(),
                                ..Default::default()
                            };
                            self.close_turn(&mut state, &completed, &NeverCancelled)
                                .await
                                .map_err(|e| e.lifecycle_error())?;
                            self.activity.assistant_message(&self.notices.interrupted);
                        }
                        Ok(Execution::canceled())
                    }
                    Err(error) => {
                        work.join_mutations().await?;
                        state.turn = None;
                        if let Some(turn) = state.diagnostic_turn.take() {
                            self.activity.turn_finished(
                                &turn,
                                "failed",
                                Some(error.diagnostic_code()),
                            );
                        }
                        self.fail(error.to_string(), error.response_body().to_owned())
                            .await
                            .map_err(|e| e.lifecycle_error())?;
                        Ok(Execution::failed(error.to_string()))
                    }
                }
            }
            .await;
            self.skills.end_turn().await;
            state.turn = None;
            state.diagnostic_turn = None;
            let execution = outcome?;
            let result_reference = if execution.status == ExecutionStatus::Succeeded {
                self.history
                    .repository()
                    .latest_execution_result_sequence(self.identity.session_id.clone())
                    .await
                    .map_err(|error| LifecycleError::Failed(error.to_string()))?
                    .map_or(
                        ExecutionResultReference::Missing,
                        ExecutionResultReference::Captured,
                    )
            } else {
                ExecutionResultReference::Missing
            };
            Ok(DrainPass {
                execution,
                result_reference,
            })
        })
    }
    fn execution_started(&self) -> BoxFuture<'_, Result<(), LifecycleError>> {
        self.start_execution(ExecutionCorrelation::Legacy)
    }
    fn execution_started_correlated(
        &self,
        correlation: &str,
    ) -> BoxFuture<'_, Result<(), LifecycleError>> {
        self.start_execution(ExecutionCorrelation::Correlated(correlation.to_owned()))
    }
    fn execution_finished<'a>(
        &'a self,
        execution: &'a Execution,
    ) -> BoxFuture<'a, Result<(), LifecycleError>> {
        self.finish_execution(execution, ExecutionCorrelation::Legacy)
    }
    fn execution_finished_correlated<'a>(
        &'a self,
        execution: &'a Execution,
        correlation: &'a str,
    ) -> BoxFuture<'a, Result<(), LifecycleError>> {
        self.finish_execution(
            execution,
            ExecutionCorrelation::Correlated(correlation.to_owned()),
        )
    }
    fn cancel_execution_input(
        &self,
        message_id: &str,
    ) -> BoxFuture<'_, Result<(), LifecycleError>> {
        let message_id = message_id.to_owned();
        Box::pin(async move {
            if let Some(input) = self
                .history
                .repository()
                .events()
                .find_input(self.identity.session_id.clone(), message_id)
                .await
                .map_err(|error| LifecycleError::Failed(error.to_string()))?
            {
                self.cancel_pending_input(input.id).await?;
            }
            Ok(())
        })
    }
    fn execution_watermark(&self) -> BoxFuture<'_, Result<i64, LifecycleError>> {
        Box::pin(async move {
            self.history
                .repository()
                .execution_watermark(self.identity.session_id.clone())
                .await
                .map_err(|error| LifecycleError::Failed(error.to_string()))
        })
    }
    fn restore_execution<'a>(
        &'a self,
        terminal: &'a ExecutionTerminal,
    ) -> BoxFuture<'a, Result<Execution, LifecycleError>> {
        Box::pin(async move {
            match &terminal.outcome {
                ExecutionOutcome::Failed(error) => Ok(Execution::failed(error)),
                ExecutionOutcome::Canceled => Ok(Execution::canceled()),
                ExecutionOutcome::Succeeded => {
                    let item = self
                        .history
                        .repository()
                        .execution_message(
                            self.identity.session_id.clone(),
                            terminal.history_sequence,
                        )
                        .await
                        .map_err(|error| LifecycleError::Failed(error.to_string()))?;
                    let item = item.ok_or_else(|| {
                        LifecycleError::Failed(
                            "saved execution result is missing from conversation history".into(),
                        )
                    })?;
                    if item.message.role != MessageRole::Assistant
                        || !item.message.tool_calls.is_empty()
                    {
                        return Err(LifecycleError::Failed(
                            "saved execution result is not a terminal assistant message".into(),
                        ));
                    }
                    Ok(Execution::succeeded(parrot_storage::events::content(
                        &item.message.parts,
                    )))
                }
            }
        })
    }
    fn capture_execution<'a>(
        &'a self,
        checkpoint: &'a ExecutionCheckpoint,
        execution: &'a mut Execution,
        reference: ExecutionResultReference,
    ) -> BoxFuture<'a, Result<ExecutionTerminal, LifecycleError>> {
        Box::pin(async move {
            let candidate = match reference {
                ExecutionResultReference::Captured(sequence)
                    if sequence > checkpoint.history_watermark =>
                {
                    self.history
                        .repository()
                        .execution_message(self.identity.session_id.clone(), sequence)
                        .await
                        .map_err(|error| LifecycleError::Failed(error.to_string()))?
                }
                _ => None,
            }
            .filter(|item| {
                item.message.role == MessageRole::Assistant && item.message.tool_calls.is_empty()
            });
            if execution.status == ExecutionStatus::Succeeded && candidate.is_none() {
                return Err(LifecycleError::Failed(
                    "successful execution has no captured terminal assistant reference".into(),
                ));
            }
            if execution.status == ExecutionStatus::Succeeded && execution.output.is_empty() {
                if let Some(item) = &candidate {
                    execution.output = parrot_storage::events::content(&item.message.parts);
                }
            }
            Ok(ExecutionTerminal {
                event_id: format!("{}.finished", checkpoint.execution_id),
                history_sequence: candidate.as_ref().map_or(0, |item| item.sequence),
                outcome: match execution.status {
                    ExecutionStatus::Succeeded => ExecutionOutcome::Succeeded,
                    ExecutionStatus::Failed => ExecutionOutcome::Failed(execution.error.clone()),
                    ExecutionStatus::Canceled => ExecutionOutcome::Canceled,
                },
            })
        })
    }
    fn suspend_execution(&self) {
        self.suspending.store(true, Ordering::Release);
    }
    fn close(&self) -> BoxFuture<'_, Result<(), LifecycleError>> {
        Box::pin(async move {
            let providers = self.sessions.close().await;
            let output = self.output.close().await;
            providers.map_err(|e| TurnError::Provider(e).lifecycle_error())?;
            output.map_err(|e| LifecycleError::Failed(e.to_string()))
        })
    }
}
