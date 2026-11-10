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
    ) -> Result<Vec<parrot_runtime::agent_turn::services::ToolExecutionResult>, TurnError> {
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
                flags: parrot_core::budget::TurnFlags { final_phase: false },
                starts_batch: request.starts_batch,
                cancel_remaining: request.cancel_remaining,
            },
            parrot_core::CancellationToken::new(cancellation),
        )
        .await
        .map_err(TurnError::Core)
        .map(|results| {
            results
                .into_iter()
                .map(
                    |execution| parrot_runtime::agent_turn::services::ToolExecutionResult {
                        result: execution.result,
                        yielded_process: execution.yielded_process.map(|value| {
                            protocol::YieldedShellProcess {
                                process_id: value.process_id,
                                name: value.name,
                                inventory_instance_id: value.inventory_instance_id,
                                visible_revision: value.visible_revision,
                                stdout_path: value.stdout_path,
                                stderr_path: value.stderr_path,
                                activity_kind: protobuf::EnumOrUnknown::from_i32(
                                    value.activity_kind,
                                ),
                                ..Default::default()
                            }
                        }),
                    },
                )
                .collect()
        })
    }
    pub async fn restore(&mut self, fixture: &Fixture) -> Result<(), TurnError> {
        fixture.fresh_batches.lock().unwrap().clear();
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
                self.state.begin_execution(sequence, calls.clone())?;
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
                    self.state
                        .record_results(&mut self.runtime_state.get_mut().conversation, &results)
                        .await?;
                    cancelled |= results
                        .iter()
                        .any(|result| result.result.status == parrot_core::ToolStatus::Cancelled);
                    completed.extend(results.into_iter().map(|result| result.result));
                }
                self.state
                    .complete_execution(&mut self.runtime_state.get_mut().conversation, &completed)
                    .await?;
            }
            self.state
                .restore_results(&mut self.runtime_state.get_mut().conversation)
                .await
        }
        .await;
        if result.is_err() {
            self.state.reset_pending();
        }
        result
    }
}

impl Fixture {
    pub async fn conversation(&self) -> AgentConversation {
        AgentConversation::new(self.history.clone()).await.unwrap()
    }
}
