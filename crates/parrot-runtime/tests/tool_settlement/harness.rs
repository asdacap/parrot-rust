use parrot_domain::protocol::messages::parrot::YieldedShellProcess;
use parrot_storage::conversation::ConversationOrigin;

use super::*;

pub struct ContextPublisher(pub AgentHistoryView);
impl CompactionObserver for ContextPublisher {
    fn observe<'a>(
        &'a self,
        _: &'a sessions::Event,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move { cancellation.check().map_err(|_| Error::Cancelled) })
    }
    fn observe_progress<'a>(
        &'a self,
        progress: parrot_domain::protocol::messages::parrot::CompactionProgressEvent,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let mut event = Event {
                id: parrot_domain::values::identifiers::Identifier::event_id().unwrap(),
                agent_session_id: self.0.agent_session_id().into(),
                ..Event::new()
            };
            event.set_compaction_progress(progress);
            self.emit(event, cancellation)
                .await
                .map_err(|error| Error::Provider(error.to_string()))
        })
    }
}
impl ContextEvents for ContextPublisher {
    fn emit<'a>(
        &'a self,
        event: Event,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<()>> {
        Box::pin(async move {
            self.0
                .append_message(event, None, ConversationOrigin::Model)
                .await?;
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
pub struct Harness {
    pub config: ToolExecutionConfig,
    pub materialized: std::sync::Mutex<Option<ToolSnapshot<Security>>>,
    pub execution:
        tokio::sync::Mutex<Option<Arc<parrot_domain::values::tools::ToolCycleImageBudget>>>,
    pub runtime_state: tokio::sync::Mutex<State>,
    pub interrupted_result: String,
    pub selection: TurnSelection<Security>,
    pub capture: Arc<Selection>,
    pub state: parrot_runtime::agent_turn::tool_conversation::ToolConversation,
    pub settlement: parrot_core::ToolSettlement<YieldedShellProcess>,
    pub publisher: Arc<Publisher>,
    pub activity: Arc<AgentSessionActivity>,
    pub diagnostics: Arc<Diagnostics>,
    pub factory: Arc<Factory>,
    pub usage: Arc<SessionUsageRecorder>,
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
            .unwrap()
            .with_path_variables(BTreeMap::from([(
                "AGENT_DIR".into(),
                fixture.path.display().to_string(),
            )])),
        );
        let config = ToolExecutionConfig {
            path_variables: [("AGENT_DIR".into(), fixture.path.display().to_string())]
                .into_iter()
                .collect(),
            templates: templates.clone(),
            output_blobs: blobs,
            publisher: publisher.clone(),
            activity: activity.clone(),
            diagnostics: diagnostics.clone(),
            image_bytes_per_cycle: limit,
        };
        let snapshot = ToolSnapshot::document(factory.materialize().unwrap()).unwrap();
        let selection = selection(&Scripted::default());
        let capture = Arc::new(Selection(Mutex::new(selection.clone())));
        let state = parrot_runtime::agent_turn::tool_conversation::ToolConversation::new(
            fixture.history.clone(),
            publisher.clone(),
            Arc::new(id),
            templates.as_ref(),
        )
        .unwrap();
        let interrupted_result = templates
            .render("agent-session.interrupted-result", &[], &NeverCancelled)
            .unwrap();
        let usage = Arc::new(
            SessionUsageRecorder::new(
                fixture.history.clone(),
                String::new(),
                Arc::new(EventBroker::new()),
                vec![],
            )
            .await
            .unwrap(),
        );
        let runtime_state = tokio::sync::Mutex::new(State {
            agent_state: parrot_core::AgentState::new(false),
            conversation: fixture.conversation().await,
            tool_conversation:
                parrot_runtime::agent_turn::tool_conversation::ToolConversation::new(
                    fixture.history.clone(),
                    publisher.clone(),
                    Arc::new(id),
                    templates.as_ref(),
                )
                .unwrap(),
            context: fixture.controller(),
            request_status: Some(Arc::new(Status)),
            request_accounting: None,
            usage: usage.clone(),
        });
        Self {
            config,
            materialized: std::sync::Mutex::new(Some(snapshot.clone())),
            execution: tokio::sync::Mutex::new(None),
            runtime_state,
            interrupted_result,
            selection,
            capture,
            state,
            settlement: parrot_core::ToolSettlement::default(),
            publisher,
            activity,
            diagnostics,
            factory,
            usage,
            prompt: parrot_runtime::prompt_context::SessionPrompt::new(Box::new(Prompt)),
            sessions: sessions::ProviderSessions::new(),
            events: ContextPublisher(fixture.history.clone()),
        }
    }
    pub async fn execute(
        &mut self,
        request: ToolExecutionChunk,
        capture: &dyn SelectionSource<Security>,
        cancellation: &dyn Cancellation,
    ) -> Result<Vec<parrot_core::ToolResult<YieldedShellProcess>>, TurnError> {
        let binding = TurnBinding::new();
        binding.bind(self.selection.clone());
        let tools = RuntimeTools {
            config: &self.config,
            factory: self.factory.as_ref(),
            materialized: &self.materialized,
            execution: &self.execution,
            state: &self.runtime_state,
            history: &self.events.0,
            prompt: &self.prompt,
            sessions: &self.sessions,
            usage: &self.usage,
            capture,
            binding: &binding,
            events: &self.events,
            interrupted_result: &self.interrupted_result,
            captured: std::sync::Mutex::new(None),
        };
        parrot_core::Tools::capture_catalog(&tools)
            .await
            .map_err(TurnError::Core)?;
        parrot_core::Tools::execute(
            &tools,
            parrot_core::ToolExecutionRequest {
                calls: request.calls,
                receipt: parrot_core::ToolCallReceipt {
                    assistant_sequence: request.assistant_sequence,
                },
                flags: parrot_core::TurnFlags { final_phase: false },
                starts_batch: request.starts_batch,
                cancel_remaining: request.cancel_remaining,
            },
            parrot_core::CancellationToken::new(cancellation),
        )
        .await
        .map_err(TurnError::Core)
    }

    pub fn begin_execution(
        &mut self,
        sequence: i64,
        calls: Vec<parrot_core::ToolCall>,
    ) -> Result<(), TurnError> {
        self.settlement
            .begin_execution(sequence, calls)
            .map_err(TurnError::Core)
    }
    pub async fn record_results(
        &mut self,
        results: &[parrot_core::ToolResult<YieldedShellProcess>],
    ) -> Result<(), TurnError> {
        self.settlement
            .record_results(
                &mut SettlementStates {
                    tools: &mut self.state,
                    conversation: &mut self.runtime_state.get_mut().conversation,
                },
                results,
            )
            .await
            .map_err(TurnError::Core)
    }
    pub async fn complete_execution(
        &mut self,
        results: &[parrot_core::ToolResult<YieldedShellProcess>],
    ) -> Result<(), TurnError> {
        self.settlement
            .complete_execution(results)
            .map_err(TurnError::Core)
    }
    pub async fn restore(&mut self, fixture: &Fixture) -> Result<(), TurnError> {
        fixture.fresh_batches.lock().unwrap().clear();
        self.settlement.reset_pending();
        self.state.reset_pending();
        self.state
            .restore_results(&mut self.runtime_state.get_mut().conversation)
            .await
    }
    pub async fn run(
        &mut self,
        fixture: &Fixture,
        cancellation: &dyn Cancellation,
    ) -> Result<(), TurnError> {
        let capture = self.capture.clone();
        self.run_with_capture(fixture, capture.as_ref(), cancellation)
            .await
    }
    pub async fn run_with_capture(
        &mut self,
        fixture: &Fixture,
        capture: &dyn SelectionSource<Security>,
        cancellation: &dyn Cancellation,
    ) -> Result<(), TurnError> {
        let result = async {
            loop {
                let Some((sequence, calls)) = fixture.fresh_batches.lock().unwrap().pop_front()
                else {
                    break;
                };
                self.begin_execution(sequence, calls.clone())?;
                let mut completed = Vec::new();
                let mut cancelled = false;
                while completed.len() < calls.len() {
                    let request = ToolExecutionChunk {
                        assistant_sequence: sequence,
                        starts_batch: completed.is_empty(),
                        calls: calls[completed.len()..].to_vec(),
                        cancel_remaining: cancelled,
                    };
                    let results = self.execute(request, capture, cancellation).await?;
                    self.record_results(&results).await?;
                    cancelled |= results
                        .iter()
                        .any(|result| result.status == parrot_core::ToolStatus::Cancelled);
                    completed.extend(results);
                }
                self.complete_execution(&completed).await?;
            }
            self.state
                .restore_results(&mut self.runtime_state.get_mut().conversation)
                .await
        }
        .await;
        if result.is_err() {
            self.settlement.reset_pending();
            self.state.reset_pending();
        }
        result
    }
}

struct SettlementStates<'a> {
    tools: &'a mut parrot_runtime::agent_turn::tool_conversation::ToolConversation,
    conversation: &'a mut AgentConversation,
}
impl parrot_core::States for SettlementStates<'_> {
    fn get_config(
        &self,
        _: parrot_core::ConfigKey,
    ) -> BoxFuture<'_, Result<parrot_core::ConfigValue, parrot_core::Error>> {
        Box::pin(async {
            Err(parrot_core::Error::unsupported(
                "SettlementStates.get_config",
            ))
        })
    }
    fn add_conversation_messages(
        &mut self,
        addition: parrot_core::ConversationAddition,
    ) -> BoxFuture<'_, Result<parrot_core::ConversationReceipt, parrot_core::Error>> {
        Box::pin(async move {
            self.tools
                .add_conversation_messages(self.conversation, addition)
                .await
                .map_err(Into::into)
        })
    }
}

impl Fixture {
    pub async fn conversation(&self) -> AgentConversation {
        AgentConversation::new(self.history.clone()).await.unwrap()
    }
}
