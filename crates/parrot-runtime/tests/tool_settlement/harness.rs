use super::*;
pub use parrot_domain::{
    contracts::{
        async_ops::{Cancelled, Clock},
        security::InvocationContext,
        tools::{ParallelSafety, Tool},
    },
    values::{activity::AgentSessionActivity, tools::ConfiguredToolDefinition},
};
pub use parrot_runtime::{
    agent_turn::{lifecycle::Signal, usage::SessionUsageRecorder},
    tool_settlement::*,
};
pub use parrot_storage::{
    artifacts::{ImageArtifactMetadata, output_blobs::ToolOutputBlobStore},
    diagnostics::{DiagnosticEvent, FailureKind},
    events::{ToolExecutionStatus, ToolExecutionTerminal},
};
use std::time::{Instant, SystemTime};

pub struct TestClock;
impl Clock for TestClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn utc_now(&self) -> SystemTime {
        SystemTime::now()
    }
    fn sleep_until<'a>(
        &'a self,
        _: Instant,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Cancelled>> {
        panic!("activity does not sleep")
    }
}
#[derive(Default)]
pub struct Diagnostics(pub Mutex<Vec<DiagnosticEvent>>);
impl ToolDiagnostics for Diagnostics {
    fn write(&self, event: &DiagnosticEvent) {
        self.0.lock().unwrap().push(event.clone());
    }
}
pub struct Publisher {
    pub history: AgentHistoryView,
    pub events: Mutex<Vec<Event>>,
    pub fail_terminal: AtomicUsize,
    pub fail_started: AtomicUsize,
}
impl Publisher {
    pub fn new(history: AgentHistoryView) -> Self {
        Self {
            history,
            events: Mutex::new(vec![]),
            fail_terminal: AtomicUsize::new(0),
            fail_started: AtomicUsize::new(0),
        }
    }
}
impl EventPublisher for Publisher {
    type Event = Event;
    type Error = BrokerError;
    fn publish_committed<'a>(
        &'a self,
        event: &'a Committed<Event>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<BrokerError>>> {
        Box::pin(async move {
            assert!(
                !cancellation.is_cancelled(),
                "tool publication must be uncancelled"
            );
            let event = event.value();
            let persisted = self.history.repository().events().replay().await.unwrap();
            assert!(
                persisted.iter().any(|stored| stored.id == event.id),
                "publication preceded durable event"
            );
            self.events.lock().unwrap().push(event.clone());
            let terminal =
                event.has_tool_finished() || event.has_tool_error() || event.has_tool_cancelled();
            let fault = if terminal {
                &self.fail_terminal
            } else {
                &self.fail_started
            };
            if (terminal || event.has_tool_started())
                && fault
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                        value.checked_sub(1)
                    })
                    .is_ok()
            {
                return Err(OperationError::Failed(BrokerError::InvalidCapacity));
            }
            Ok(())
        })
    }
    fn publish_transient<'a>(
        &'a self,
        event: Event,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<BrokerError>>> {
        Box::pin(async move {
            self.events.lock().unwrap().push(event);
            Ok(())
        })
    }
}
pub struct ContextPublisher(pub AgentHistoryView);
impl RetryObserver for ContextPublisher {
    fn observe<'a>(
        &'a self,
        _: &'a sessions::Event,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move { cancellation.check().map_err(|_| Error::Cancelled) })
    }
}
impl ContextEvents for ContextPublisher {
    fn emit<'a>(
        &'a self,
        event: Event,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<()>> {
        Box::pin(async move {
            self.0.append(event, None, None).await?;
            Ok(())
        })
    }
    fn publish_committed<'a>(
        &'a self,
        _: Committed<Event>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn publish<'a>(
        &'a self,
        _: Event,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<()>> {
        Box::pin(async { Ok(()) })
    }
}
pub struct Factory {
    pub tools: Vec<ToolHandle<Security>>,
    pub creates: AtomicUsize,
}
impl ToolFactory<Security> for Factory {
    fn materialize(&self) -> Result<ToolInventory<Security>, TurnError> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        Ok(inventory(self.tools.clone()))
    }
}
pub fn inventory(tools: Vec<ToolHandle<Security>>) -> ToolInventory<Security> {
    let count = tools.len();
    ToolInventory {
        tools,
        supported: vec![true; count],
        definitions: (0..count)
            .map(|index| ConfiguredToolDefinition {
                description: format!("configured-{index}"),
                parameters_json: format!("{{\"index\":{index}}}"),
            })
            .collect(),
    }
}
pub struct Harness {
    pub tools: Arc<ToolSettlement<Security>>,
    pub snapshot: ToolSnapshot<Security>,
    pub selection: TurnSelection<Security>,
    pub capture: Selection,
    pub publisher: Arc<Publisher>,
    pub activity: Arc<AgentSessionActivity>,
    pub diagnostics: Arc<Diagnostics>,
    pub factory: Arc<Factory>,
    pub usage: SessionUsageRecorder,
    pub conversation: AgentConversation,
    pub context: ContextController,
    pub prompt: parrot_runtime::prompt_context::SessionPrompt,
    pub sessions: sessions::ProviderSessions,
    pub events: ContextPublisher,
}
impl Harness {
    pub async fn new(fixture: &Fixture, tools: Vec<ToolHandle<Security>>) -> Self {
        Self::with_limit(fixture, tools, 1024 * 1024).await
    }
    pub async fn with_limit(
        fixture: &Fixture,
        tools: Vec<ToolHandle<Security>>,
        limit: i32,
    ) -> Self {
        Self::configured(fixture, tools, limit, templates()).await
    }
    pub async fn configured(
        fixture: &Fixture,
        tools: Vec<ToolHandle<Security>>,
        limit: i32,
        templates: Arc<PromptTemplateCatalog>,
    ) -> Self {
        let factory = Arc::new(Factory {
            tools,
            creates: AtomicUsize::new(0),
        });
        let publisher = Arc::new(Publisher::new(fixture.history.clone()));
        let activity = Arc::new(AgentSessionActivity::new(Arc::new(TestClock)));
        let diagnostics = Arc::new(Diagnostics::default());
        let blobs = Arc::new(
            ToolOutputBlobStore::for_session(
                fixture.repository.events().database().selected_root(),
                "scratch",
                Arc::new(Names),
            )
            .unwrap(),
        );
        let tools = Arc::new(
            ToolSettlement::new(ToolSettlementConfig {
                factory: factory.clone(),
                templates,
                output_blobs: blobs,
                publisher: publisher.clone(),
                activity: activity.clone(),
                diagnostics: diagnostics.clone(),
                image_bytes_per_cycle: limit,
            })
            .unwrap(),
        );
        let snapshot = tools.materialize().unwrap();
        let selection = selection(&Scripted::default());
        let capture = Selection(Mutex::new(selection.clone()));
        Self {
            tools,
            snapshot,
            selection,
            capture,
            publisher,
            activity,
            diagnostics,
            factory,
            usage: SessionUsageRecorder::new(
                fixture.history.clone(),
                String::new(),
                Arc::new(EventBroker::new()),
                vec![],
            )
            .await
            .unwrap(),
            conversation: fixture.conversation().await,
            context: fixture.controller(),
            prompt: parrot_runtime::prompt_context::SessionPrompt::new(Box::new(Prompt)),
            sessions: sessions::ProviderSessions::new(),
            events: ContextPublisher(fixture.history.clone()),
        }
    }
    pub async fn run(
        &mut self,
        fixture: &Fixture,
        cancellation: &dyn Cancellation,
    ) -> Result<(), TurnError> {
        self.tools
            .reconcile(
                TurnResources {
                    history: &fixture.history,
                    conversation: &mut self.conversation,
                    context: &mut self.context,
                    prompt: &self.prompt,
                    sessions: &self.sessions,
                    status: &Status,
                    events: &self.events,
                    usage: &self.usage,
                },
                &self.selection,
                &self.snapshot,
                &self.capture,
                cancellation,
            )
            .await
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Observation {
    pub id: String,
    pub security: usize,
    pub profile: String,
    pub requested: String,
    pub active: Option<String>,
    pub starts: usize,
    pub usages: usize,
}
pub struct RecordingTool {
    pub metadata: ToolMetadata,
    pub safety: ParallelSafety,
    pub after_interruption: bool,
    pub calls: Arc<Mutex<Vec<Observation>>>,
    pub completed: Arc<Mutex<Vec<String>>>,
    pub gate: Option<Arc<Gate>>,
    pub output: ToolOutput,
    pub fail: bool,
    pub cancel: bool,
    pub ignore_cancel: bool,
    pub history: AgentHistoryView,
    pub activity: Mutex<Option<Arc<AgentSessionActivity>>>,
}
impl RecordingTool {
    pub fn new(fixture: &Fixture, name: &str, safety: ParallelSafety) -> Self {
        Self {
            metadata: ToolMetadata {
                name: name.into(),
                aliases: vec![],
            },
            safety,
            after_interruption: true,
            calls: Arc::new(Mutex::new(vec![])),
            completed: Arc::new(Mutex::new(vec![])),
            gate: None,
            output: ToolOutput::text(format!("result:{name}")),
            fail: false,
            cancel: false,
            ignore_cancel: false,
            history: fixture.history.clone(),
            activity: Mutex::new(None),
        }
    }
    pub fn handle(self: Arc<Self>) -> ToolHandle<Security> {
        ToolHandle::Ordinary(self)
    }
}
impl Tool for RecordingTool {
    type Definition = ToolMetadata;
    type Invocation = ToolInvocation;
    type Selection = TurnSelection<Security>;
    type Security = SharedSecurity<Security>;
    type Output = ToolOutput;
    type Error = ToolFailure;
    fn definition(&self) -> &ToolMetadata {
        &self.metadata
    }
    fn parallel_safety(&self, _: &ToolInvocation) -> ParallelSafety {
        self.safety
    }
    fn enabled_after_interruption(&self) -> bool {
        self.after_interruption
    }
    fn invoke<'a>(
        &'a self,
        invocation: ToolInvocation,
        context: InvocationContext<'a, SharedSecurity<Security>, TurnSelection<Security>>,
    ) -> BoxFuture<'a, Result<ToolOutput, OperationError<ToolFailure>>> {
        Box::pin(async move {
            let events = self.history.repository().events().replay().await.unwrap();
            let active = self
                .activity
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|activity| activity.capture().current_tool);
            let captured = context.security().snapshot().version;
            self.calls.lock().unwrap().push(Observation {
                id: invocation.call_id.clone(),
                security: captured,
                profile: context.selection().profile.id.clone(),
                requested: context.selection().model.requested_selector.value.clone(),
                active,
                starts: events
                    .iter()
                    .filter(|event| {
                        event.has_tool_started()
                            && event.tool_started().tool_call_id == invocation.call_id
                    })
                    .count(),
                usages: events
                    .iter()
                    .filter(|event| {
                        event.has_tool_execution_started()
                            && event.tool_execution_started().tool_call_id == invocation.call_id
                    })
                    .count(),
            });
            if let Some(gate) = &self.gate {
                gate.wait(if self.ignore_cancel {
                    &NeverCancelled
                } else {
                    context.cancellation()
                })
                .await
                .map_err(|_| OperationError::Cancelled)?;
            }
            assert_eq!(
                context.security().snapshot().version,
                captured,
                "running immutable capture changed"
            );
            self.completed.lock().unwrap().push(invocation.call_id);
            if self.cancel {
                return Err(OperationError::Cancelled);
            }
            if self.fail {
                return Err(OperationError::Failed(ToolFailure::new(
                    "RAW_SECRET_TOOL_FAILURE",
                    FailureKind::Io,
                )));
            }
            let mut output = self.output.clone();
            for image in &output.image_artifacts {
                if !invocation
                    .image_budget
                    .as_ref()
                    .unwrap()
                    .try_accept(image.byte_length)
                    .unwrap()
                {
                    output = ToolOutput::text("image budget exceeded");
                    output.outcome = ToolOutcome::ImageBudgetExceeded;
                    break;
                }
            }
            Ok(output)
        })
    }
}
