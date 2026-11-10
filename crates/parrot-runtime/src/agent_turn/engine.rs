//! Durable runtime integration behind the session-owned lifecycle.
use super::{
    lifecycle::{
        DrainBackend, DrainPass, DrainWork, Execution, ExecutionResultReference, ExecutionStatus,
        LifecycleError,
    },
    provider_call::{self, OutputWriter, ProviderActivity, ProviderCall, ProviderUsage},
    services::*,
    tool_conversation::ToolConversation,
    usage::SessionUsageRecorder,
};
use crate::{
    active_work::exit_reminders::{ExitReminders, ReminderError},
    compaction::{
        CompactionObserver, ContextController, ContextError, ContextEvents, ContextResult,
        EpochInput, StatusInjection, provider_message,
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
use parrot_provider::{
    routing::registry::ProviderModel,
    sessions::{self, ProviderSessions},
};
use parrot_storage::{
    conversation::{AgentConversation, AgentHistoryView, ConversationReadView, ModelMessage},
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

mod core_loop;

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
    pub completion_sources: Vec<Arc<dyn CompletionStateSource>>,
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
    tool_conversation: ToolConversation,
    context: ContextController,
    request_status: Option<Arc<dyn crate::compaction::ContextStatus>>,
    request_accounting: Option<RequestAccounting>,
    usage: Arc<SessionUsageRecorder>,
}

struct RequestAccounting {
    selected: ProviderModel,
    estimated_input_tokens: i64,
}

impl ProviderUsage for State {
    fn observe_completion<'a>(
        &'a mut self,
        request: &'a sessions::Request,
        completed: &'a sessions::Completion,
    ) -> BoxFuture<'a, Result<(), EventError>> {
        Box::pin(async move {
            let accounting = self
                .request_accounting
                .as_ref()
                .ok_or_else(|| EventError::Invalid("request accounting is unavailable".into()))?;
            self.context.budget_mut().observe_usage(
                &accounting.selected.selector(),
                accounting.estimated_input_tokens,
                completed,
            );
            self.usage
                .record_request(&accounting.selected, request.reasoning.as_ref(), completed)
                .await
        })
    }
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
    templates: Arc<PromptTemplateCatalog>,
    status: Arc<dyn TurnStatus<S>>,
    completion_sources: Vec<Arc<dyn CompletionStateSource>>,
    output: Arc<dyn SessionOutput>,
    activity: Arc<dyn SessionActivity>,
    parent_completion: Arc<dyn ParentCompletion>,
    event_id: Arc<dyn Fn() -> String + Send + Sync>,
    sessions: ProviderSessions,
    usage: Arc<SessionUsageRecorder>,
    reminders: Arc<ExitReminders<Publisher>>,
    suspending: AtomicBool,
    state: Mutex<State>,
    prompt: SessionPrompt,
    conversation: ConversationReadView,
    materialized_tools: std::sync::Mutex<Option<T::Snapshot>>,
    compactor: crate::compaction::Compactor,
    notices: Notices,
    execution_activity: std::sync::Mutex<Option<i64>>,
}
struct Notices {
    runaway: String,
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
        let usage = Arc::new(
            SessionUsageRecorder::new(
                config.history.clone(),
                config.identity.parent_session_id.clone(),
                config.broker.clone(),
                config.ancestors,
            )
            .await?,
        );
        let tool_conversation = ToolConversation::new(
            config.history.clone(),
            config.publisher.clone(),
            config.event_id.clone(),
            &config.templates,
        )?;
        let reminders = config.reminders;
        let render = |key: &str| {
            config
                .templates
                .render(key, &[], &NeverCancelled)
                .map_err(|e| TurnError::Service(e.to_string()))
        };
        let notices = Notices {
            runaway: render("agent-session.runaway")?,
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
            templates: config.templates,
            status: config.status,
            completion_sources: config.completion_sources,
            output: config.output,
            activity: config.activity,
            parent_completion: config.parent_completion,
            event_id: config.event_id,
            sessions,
            usage: usage.clone(),
            reminders,
            suspending: AtomicBool::new(false),
            prompt: SessionPrompt::new(config.prompt),
            conversation: conversation.read_view(),
            materialized_tools: std::sync::Mutex::new(None),
            compactor: config.context.compactor().clone(),
            state: Mutex::new(State {
                conversation,
                tool_conversation,
                context: config.context,
                request_status: None,
                request_accounting: None,
                usage,
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
                self.compactor
                    .planner()
                    .estimate_selected_context(&selection.model, instructions, &tools, &messages)
                    .map_err(TurnError::Planning)
            })
            .await??;
        Ok(super::observation::ObservedContext { selection, context })
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
    async fn compact_now(
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
        let events = ContextEmitter::new(
            &self.history,
            self.publisher.as_ref(),
            &self.broker,
            self.event_id.as_ref(),
        );
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

    async fn fail(&self, message: String, body: String) -> Result<(), TurnError> {
        let mut event = self.event();
        event.set_turn_failed(protocol::TurnFailed {
            message,
            provider_response_body: body,
            ..Default::default()
        });
        self.emit(event, &NeverCancelled).await
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
#[derive(Clone, Copy)]
struct ContextEmitter<'a> {
    history: &'a AgentHistoryView,
    publisher: &'a Publisher,
    broker: &'a EventBroker,
    event_id: &'a (dyn Fn() -> String + Send + Sync),
}
impl<'a> ContextEmitter<'a> {
    fn new(
        history: &'a AgentHistoryView,
        publisher: &'a Publisher,
        broker: &'a EventBroker,
        event_id: &'a (dyn Fn() -> String + Send + Sync),
    ) -> Self {
        Self {
            history,
            publisher,
            broker,
            event_id,
        }
    }
    fn event(&self) -> Event {
        Event {
            id: (self.event_id)(),
            agent_session_id: self.history.agent_session_id().into(),
            ..Event::new()
        }
    }
    async fn emit(&self, event: Event, cancellation: &dyn Cancellation) -> Result<(), TurnError> {
        self.history.append(event.clone(), None, None).await?;
        self.publisher
            .publish_committed(&Committed::after_commit(event), cancellation)
            .await
            .map_err(TurnError::Publication)
    }
}

impl CompactionObserver for ContextEmitter<'_> {
    fn observe<'a>(
        &'a self,
        event: &'a sessions::Event,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), sessions::Error>> {
        Box::pin(async move {
            provider_call::emit_retry(
                self.history,
                self.publisher,
                self.event_id,
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
    fn observe_progress<'a>(
        &'a self,
        progress: parrot_domain::protocol::messages::parrot::CompactionProgressEvent,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), sessions::Error>> {
        Box::pin(async move {
            let mut event = self.event();
            event.set_compaction_progress(progress);
            self.emit(event, cancellation).await.map_err(|error| {
                if error.is_cancelled() {
                    sessions::Error::Cancelled
                } else {
                    sessions::Error::Provider(error.to_string())
                }
            })
        })
    }
}
impl ContextEvents for ContextEmitter<'_> {
    fn emit<'a>(
        &'a self,
        event: Event,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<()>> {
        Box::pin(async move {
            self.emit(event, cancellation).await.map_err(|error| {
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
            self.publisher
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
            self.broker.publish_retained(&event);
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
        elapsed_ms: Option<i64>,
        correlation: ExecutionCorrelation,
    ) -> BoxFuture<'a, Result<(), LifecycleError>> {
        Box::pin(async move {
            let mut event = self.event();
            if execution.status == ExecutionStatus::Succeeded {
                event.set_agent_finished(protocol::AgentFinished {
                    parent_agent_session_id: self.identity.parent_session_id.clone(),
                    name: self.identity.name.clone(),
                    elapsed_ms,
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
                    .deliver_correlated(&completed, elapsed_ms, &format!("{correlation}.parent"))
                    .await
                    .map_err(|error| error.lifecycle_error())?;
            } else {
                self.parent_completion
                    .deliver(&completed, elapsed_ms)
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
                                ConversationPart::ImageArtifact { metadata } => {
                                    output.set_artifact_id(metadata.artifact_id.clone());
                                    output.display_name = metadata.display_name.clone();
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
    fn has_recovery_work_after(
        &self,
        watermark: i64,
    ) -> BoxFuture<'_, Result<bool, LifecycleError>> {
        Box::pin(async move {
            self.history
                .repository()
                .has_recovery_work_after(self.identity.session_id.clone(), watermark)
                .await
                .map_err(|error| LifecycleError::Failed(error.to_string()))
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
        _cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Self::Selection, LifecycleError>> {
        Box::pin(async move {
            let mut guard = self.state.lock().await;
            let state = &mut *guard;
            state
                .tool_conversation
                .restore_results(&mut state.conversation)
                .await
                .map_err(|error| error.lifecycle_error())?;
            self.selection
                .capture()
                .map_err(|error| error.lifecycle_error())
        })
    }
    fn pass<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
        work: &'a dyn DrainWork,
    ) -> BoxFuture<'a, Result<DrainPass, LifecycleError>> {
        Box::pin(async move {
            let (result, failed_key) = core_loop::run(self, cancellation, work).await;
            let outcome = async {
                match result {
                    Ok(execution) => {
                        if execution.status == ExecutionStatus::Failed {
                            if let Some(key) = &failed_key {
                                self.activity.turn_finished(
                                    &key.start_event_id,
                                    "failed",
                                    Some("runaway"),
                                );
                            }
                        }
                        Ok(execution)
                    }
                    Err(error) if error.is_cancelled() => {
                        work.join_mutations().await?;
                        if let Some(key) = &failed_key {
                            self.activity
                                .turn_finished(&key.start_event_id, "cancelled", None);
                            if !self.suspending.load(Ordering::Acquire) {
                                core_loop::interrupt(self, work, key)
                                    .await
                                    .map_err(|error| error.lifecycle_error())?;
                            }
                        }
                        Ok(Execution::canceled())
                    }
                    Err(error) => {
                        work.join_mutations().await?;
                        if let Some(key) = &failed_key {
                            self.activity.turn_finished(
                                &key.start_event_id,
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
            let recovery_watermark = self.execution_watermark().await?;
            Ok(DrainPass {
                execution,
                result_reference,
                recovery_watermark,
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
        self.finish_execution(
            execution,
            Some(
                self.activity
                    .execution_duration()
                    .as_millis()
                    .min(i64::MAX as u128) as i64,
            ),
            ExecutionCorrelation::Legacy,
        )
    }
    fn execution_finished_correlated<'a>(
        &'a self,
        execution: &'a Execution,
        terminal: &'a ExecutionTerminal,
        correlation: &'a str,
    ) -> BoxFuture<'a, Result<(), LifecycleError>> {
        self.finish_execution(
            execution,
            terminal.elapsed_ms,
            ExecutionCorrelation::Correlated(correlation.to_owned()),
        )
    }
    fn cancel_execution_input(
        &self,
        message_id: &str,
    ) -> BoxFuture<'_, Result<i64, LifecycleError>> {
        let message_id = message_id.to_owned();
        Box::pin(async move {
            let mut guard = self.state.lock().await;
            let state = &mut *guard;
            state
                .tool_conversation
                .restore_results(&mut state.conversation)
                .await
                .map_err(|error| error.lifecycle_error())?;
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
            self.execution_watermark().await
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
        recovery_watermark: i64,
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
                elapsed_ms: Some(
                    self.activity
                        .execution_duration()
                        .as_millis()
                        .min(i64::MAX as u128) as i64,
                ),
                history_sequence: candidate.as_ref().map_or(0, |item| item.sequence),
                recovery_watermark,
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
