//! Bounded host adapter composition using real scopes, storage, drain and retained tools.

#[allow(dead_code, unused_imports, clippy::duplicate_mod)]
#[path = "../../../parrot-runtime/tests/children/admission_publisher.rs"]
mod admission_publisher;
#[allow(dead_code, unused_imports, clippy::duplicate_mod)]
#[path = "../../../parrot-runtime/tests/children/support.rs"]
mod support;

#[allow(dead_code, unused_imports)]
mod fixture {
    use std::{
        collections::{BTreeMap, VecDeque},
        future::Future,
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
        time::{Duration, Instant, SystemTime},
    };

    use parrot_domain::{
        contracts::{
            async_ops::{BoxFuture, Cancellation, Cancelled, Clock, OperationError},
            filenames::FilenameGenerator,
            paths::{RootOrigin, SelectedSessionRoot},
            runtime::AgentRuntime,
            security::InvocationContext,
            tools::{ParallelSafety, Tool},
        },
        protocol::messages::parrot::{self as protocol, Event},
        values::{
            activity::AgentSessionActivity, config::ModelProfiles, tools::ConfiguredToolDefinition,
        },
    };
    use parrot_foundation::template_catalog::PromptTemplateCatalog;
    use parrot_provider::{
        catalogs::Model,
        routing::{
            aliases::{ModelAliasCatalog, ModelRouter, ModelRouting, ResolvedModelSelection},
            registry::ProviderRegistry,
        },
        sessions::{self, Completion, Error, EventStream, Provider, Request, Session},
    };
    use parrot_runtime::{
        active_work::{
            blockers::{ActiveWorkBlocker, ActiveWorkReminder},
            exit_reminders::ExitReminders,
            wait::{WaitDuration, WaitService},
        },
        agent_turn::{
            engine::{RuntimeBackend, RuntimeConfig},
            lifecycle::{AgentSession, DrainState, Execution, IncomingActivity, Signal},
            provider_call::{OutputWriter, ProviderActivity},
            services::*,
        },
        compaction::{
            CompactionSettings, Compactor, ContextController, ContextResult, ContextSnapshot,
            ContextStatus,
        },
        event_broker::EventBroker,
        prompt_context::{PromptResult, SkillRendering, SystemPrompt},
        tool_settlement::{
            self, SharedSecurity, ToolDiagnostics, ToolFactory, ToolFailure, ToolHandle,
            ToolInventory, ToolMetadata, ToolOutput, ToolSettlement, ToolSettlementConfig,
        },
    };
    use parrot_storage::{
        artifacts::{compaction::CompactionGroupBlobStore, output_blobs::ToolOutputBlobStore},
        conversation::{AgentHistoryView, ConversationRepository, HistoryForkBoundary},
        database::SessionDatabase,
        diagnostics::{DiagnosticEvent, FailureKind},
        events::{ConversationPart, EventRepository, NeverCancelled},
        history_files::AgentHistoryFile,
    };
    use parrot_tools::agents::{
        self, AgentSelection, AgentTool, AgentToolError, AgentToolKind, ModelSelector,
        NamedRequest, SendRequest, SendResult, SpawnRequest, SpawnResult, input::WaitInput,
        status::StatusSnapshot,
    };
    use tokio::sync::Semaphore;

    use super::support::Security;
    use parrot_domain::values::accounting::RetainedAgentBudget;
    use parrot_host::composition::agents::{AgentSessionServices, ScopedAgentRuntime};
    use parrot_runtime::{active_work::completion::SessionCompletion, children::*};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    fn id() -> String {
        format!("t58-wait-{}", NEXT.fetch_add(1, Ordering::SeqCst))
    }
    pub(super) async fn bounded<F: Future>(future: F) -> F::Output {
        tokio::time::timeout(Duration::from_secs(10), future)
            .await
            .expect("retained wait test did not settle")
    }

    pub(super) struct Gate {
        entered: Semaphore,
        release: Semaphore,
    }
    impl Gate {
        pub(super) fn new() -> Arc<Self> {
            Arc::new(Self {
                entered: Semaphore::new(0),
                release: Semaphore::new(0),
            })
        }
        async fn wait(&self, cancellation: &dyn Cancellation) -> Result<(), Error> {
            self.entered.add_permits(1);
            tokio::select! {
                permit = self.release.acquire() => { permit.unwrap().forget(); Ok(()) },
                _ = cancellation.cancelled() => Err(Error::Cancelled),
            }
        }
        pub(super) async fn entered(&self) {
            bounded(self.entered.acquire()).await.unwrap().forget();
        }
        pub(super) fn open(&self) {
            self.release.add_permits(1);
        }
    }

    type ScriptStep = (Completion, Arc<Gate>);

    /// The only scripted edge is the provider protocol. The runtime owns requests,
    /// promotion, tool execution/settlement, state transitions and admission dedup.
    #[derive(Clone, Default)]
    pub(super) struct Scripted {
        pub(super) requests: Arc<Mutex<Vec<Request>>>,
        steps: Arc<Mutex<VecDeque<ScriptStep>>>,
    }
    impl Scripted {
        pub(super) fn step(&self, completion: Completion) -> Arc<Gate> {
            let gate = Gate::new();
            self.steps
                .lock()
                .unwrap()
                .push_back((completion, gate.clone()));
            gate
        }
        fn router(&self) -> Arc<ModelRouter> {
            let mut model = Model::new("model", "scripted");
            model.context_window = 100000;
            let registry = Arc::new(
                ProviderRegistry::new(
                    vec![Arc::new(self.clone())],
                    BTreeMap::from([("scripted".into(), vec![model])]),
                )
                .unwrap(),
            );
            let aliases = Arc::new(ModelAliasCatalog::new(registry.clone(), vec![]).unwrap());
            Arc::new(ModelRouter::new(
                registry,
                Arc::new(ModelRouting::new(aliases, "scripted/model")),
            ))
        }
        fn selection(&self) -> ResolvedModelSelection {
            self.router().resolve("scripted/model").unwrap()
        }
        fn call_impl(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
            self.requests.lock().unwrap().push(request);
            let (completion, gate) = self
                .steps
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected provider request");
            Ok(Box::new(Stream {
                completion: Some(completion),
                gate,
            }))
        }
    }
    impl Provider for Scripted {
        fn id(&self) -> &str {
            "scripted"
        }
        fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
            self.call_impl(request)
        }
        fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
            Ok(Arc::new(self.clone()))
        }
        fn has_credential<'a>(
            &'a self,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<bool, Error>> {
            panic!("offline fixture never requests credentials")
        }
        fn seed_models(&self) -> Vec<Model> {
            vec![]
        }
        fn list_models<'a>(
            &'a self,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
            panic!("offline fixture never lists remote models")
        }
    }
    impl Session for Scripted {
        fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
            Box::pin(async { Ok(()) })
        }
        fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
            self.call_impl(request)
        }
        fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
            Box::pin(async { Ok(false) })
        }
        fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
            Box::pin(async { Ok(()) })
        }
    }
    struct Stream {
        completion: Option<Completion>,
        gate: Arc<Gate>,
    }
    impl EventStream for Stream {
        fn next<'a>(
            &'a mut self,
            cancellation: &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<Option<sessions::Event>, Error>> {
            Box::pin(async move {
                let Some(completion) = self.completion.take() else {
                    return Ok(None);
                };
                self.gate.wait(cancellation).await?;
                Ok(Some(sessions::Event::Completed(completion)))
            })
        }
        fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
            Box::pin(async { Ok(()) })
        }
    }

    fn templates() -> Arc<PromptTemplateCatalog> {
        let env = [
            ("HOME", "/tmp/t58/home"),
            ("XDG_CONFIG_HOME", "/tmp/t58/config"),
            ("XDG_STATE_HOME", "/tmp/t58/state"),
            ("XDG_DATA_HOME", "/tmp/t58/data"),
            ("WORKDIR", "/tmp/t58/work"),
        ]
        .into_iter()
        .map(|(key, value)| (key.into(), value.into()))
        .collect();
        Arc::new(PromptTemplateCatalog::from_content("", &env).unwrap().1)
    }
    struct Names;
    impl FilenameGenerator for Names {
        fn next_filename(&self) -> String {
            format!("{}.dat", id())
        }
    }
    struct TestClock;
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
            panic!("activity clock never sleeps")
        }
    }
    struct Diagnostics;
    impl ToolDiagnostics for Diagnostics {
        fn write(&self, _: &DiagnosticEvent) {}
    }
    struct Prompt;
    impl SystemPrompt for Prompt {
        fn renew_epoch(&mut self) {}
        fn sample_epoch<'a>(
            &'a mut self,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, PromptResult<()>> {
            Box::pin(async { Ok(()) })
        }
        fn build(&mut self, _: &ResolvedModelSelection) -> PromptResult<String> {
            Ok("offline retained-wait fixture".into())
        }
    }
    struct Skills;
    impl TurnSkills for Skills {
        fn begin_turn<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, PromptResult<()>> {
            Box::pin(async { Ok(()) })
        }
        fn select<'a>(
            &'a self,
            _: &'a [ConversationPart],
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, PromptResult<()>> {
            Box::pin(async { Ok(()) })
        }
        fn render<'a>(
            &'a self,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, PromptResult<SkillRendering>> {
            Box::pin(async {
                Ok(SkillRendering {
                    content: String::new(),
                    loaded_paths: vec![],
                })
            })
        }
        fn end_turn(&self) -> BoxFuture<'_, ()> {
            Box::pin(async {})
        }
    }
    struct Status;
    impl ContextStatus for Status {
        fn observe<'a>(
            &'a self,
            _: &'a ContextSnapshot,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, ContextResult<String>> {
            Box::pin(async { Ok(String::new()) })
        }
        fn observe_fitted<'a>(
            &'a self,
            estimate: &'a (dyn Fn(&str) -> ContextResult<ContextSnapshot> + Send + Sync),
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, ContextResult<String>> {
            Box::pin(async move {
                estimate("")?;
                Ok(String::new())
            })
        }
    }
    impl TurnStatus<Security> for Status {
        fn bind(&self, _: &TurnSelection<Security>) -> Arc<dyn ContextStatus> {
            Arc::new(Self)
        }
    }
    struct Output;
    impl OutputWriter for Output {
        fn append<'a>(&'a self, _: &'a str) -> BoxFuture<'a, std::io::Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }
    impl SessionOutput for Output {
        fn close(&self) -> BoxFuture<'_, std::io::Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }
    struct Activity(Arc<AgentSessionActivity>);
    impl ProviderActivity for Activity {
        fn begin_provider_request(&self) {}
        fn observe_provider_event(&self, _: &sessions::Event) {}
        fn finish_provider_request(&self) {}
    }
    impl SessionActivity for Activity {
        fn change_state(&self, state: DrainState) {
            self.0.change_state(state);
        }
        fn begin_execution(&self) -> i64 {
            self.0.begin_execution()
        }
        fn execution_duration(&self) -> Duration {
            Duration::ZERO
        }
        fn finish_execution(&self, execution: i64, outcome: &Execution) {
            self.0.finish_execution(execution, outcome.clone());
        }
        fn turn_started(&self, _: &str) {}
        fn turn_finished(&self, _: &str, _: &str, _: Option<&str>) {}
        fn assistant_message(&self, _: &str) {}
    }
    struct NoParent;
    impl ParentCompletion for NoParent {
        fn deliver<'a>(&'a self, _: &'a Execution) -> BoxFuture<'a, Result<(), TurnError>> {
            Box::pin(async { Ok(()) })
        }
    }
    struct Selection(Mutex<TurnSelection<Security>>);
    impl SelectionSource<Security> for Selection {
        fn capture(&self) -> Result<TurnSelection<Security>, TurnError> {
            Ok(self.0.lock().unwrap().clone())
        }
        fn update(&self, value: TurnSelection<Security>) -> Result<(), TurnError> {
            *self.0.lock().unwrap() = value;
            Ok(())
        }
    }

    type Backend = RuntimeBackend<Security, ToolSettlement<Security>>;
    type WaitTool = AgentTool<ScopedAgentRuntime<Security>, ModelRouter>;
    fn definition() -> ConfiguredToolDefinition {
        use parrot_foundation::config::{
            configuration::source_predefined_content, runtime::read_tool_definitions, yaml,
        };
        let root = yaml::parse_root(&source_predefined_content().unwrap()).unwrap();
        let definitions = read_tool_definitions(&root).unwrap();
        let definition = definitions.get("wait").unwrap();
        ConfiguredToolDefinition::new(&definition.description, &definition.parameters_json)
    }
    fn wait_tool(bridge: Arc<ScopedAgentRuntime<Security>>, models: Arc<ModelRouter>) -> WaitTool {
        AgentTool::new(
            AgentToolKind::Wait,
            definition(),
            bridge,
            models,
            None,
            false,
        )
    }
    struct RetainedTool {
        metadata: ToolMetadata,
        tool: WaitTool,
        waiting: Arc<Semaphore>,
    }
    impl Tool for RetainedTool {
        type Definition = ToolMetadata;
        type Invocation = tool_settlement::ToolInvocation;
        type Selection = TurnSelection<Security>;
        type Security = SharedSecurity<Security>;
        type Output = ToolOutput;
        type Error = ToolFailure;
        fn definition(&self) -> &ToolMetadata {
            &self.metadata
        }
        fn parallel_safety(&self, _: &Self::Invocation) -> ParallelSafety {
            ParallelSafety::Sequential
        }
        fn enabled_after_interruption(&self) -> bool {
            true
        }
        fn invoke<'a>(
            &'a self,
            invocation: Self::Invocation,
            context: InvocationContext<'a, Self::Security, Self::Selection>,
        ) -> BoxFuture<'a, Result<ToolOutput, OperationError<ToolFailure>>> {
            Box::pin(async move {
                let mut invocation = self.tool.invoke(
                    agents::ToolInvocation {
                        arguments_json: invocation.arguments_json,
                        assistant_sequence: invocation.assistant_sequence,
                        call_id: invocation.call_id,
                    },
                    InvocationContext::new(
                        context.security().clone(),
                        AgentSelection {
                            requested_model: context
                                .selection()
                                .model
                                .requested_selector
                                .value
                                .clone(),
                        },
                        context.cancellation(),
                    ),
                );
                let mut observed = false;
                std::future::poll_fn(|cx| {
                    let poll = invocation.as_mut().poll(cx);
                    // The retained tool has no asynchronous edge before the real
                    // WaitService registers its incoming-input wait. Observe that
                    // first Pending, not the earlier ToolExecutionStarted event.
                    if poll.is_pending() && !observed {
                        observed = true;
                        self.waiting.add_permits(1);
                    }
                    poll
                })
                .await
                .map(ToolOutput::text)
                .map_err(|error| match error {
                    OperationError::Cancelled => OperationError::Cancelled,
                    OperationError::Failed(error) => OperationError::Failed(ToolFailure::new(
                        error.to_string(),
                        FailureKind::Unexpected,
                    )),
                })
            })
        }
    }
    struct RetainedFactory(Arc<RetainedTool>);
    impl ToolFactory<Security> for RetainedFactory {
        fn materialize(&self) -> Result<ToolInventory<Security>, TurnError> {
            Ok(ToolInventory {
                tools: vec![ToolHandle::Ordinary(self.0.clone())],
                supported: vec![true],
                definitions: vec![definition()],
            })
        }
    }
    pub(super) struct Built {
        pub(super) session: Arc<AgentSession<Backend>>,
        pub(super) bridge: Arc<ScopedAgentRuntime<Security>>,
        pub(super) provider: Scripted,
        pub(super) history: AgentHistoryView,
        pub(super) activity: Arc<AgentSessionActivity>,
        pub(super) questions: Arc<parrot_runtime::interactions::ChildQuestionCoordinator>,
        waiting: Arc<Semaphore>,
    }
    struct InteractionDiagnostics;
    impl parrot_runtime::interactions::InteractionDiagnostics for InteractionDiagnostics {
        fn write(&self, _: parrot_runtime::interactions::BrokerDiagnostic) {}
    }
    struct EmptyProcesses;
    impl parrot_domain::contracts::status::ProcessStatusSource for EmptyProcesses {
        fn active(&self) -> Vec<parrot_domain::contracts::status::ActiveWorkObservation> {
            vec![]
        }
        fn snapshot(&self) -> Vec<parrot_domain::contracts::status::ProcessStatusSnapshot> {
            vec![]
        }
    }
    struct Factory {
        registry: Mutex<std::sync::Weak<AgentRegistry<Security>>>,
        histories: Arc<super::support::Histories>,
        built: Mutex<BTreeMap<String, Arc<Built>>>,
        providers: Mutex<BTreeMap<String, Scripted>>,
        user_questions: Arc<parrot_runtime::interactions::QuestionBroker>,
    }
    impl Factory {
        async fn build(
            &self,
            prepared: PreparedScope<Security>,
        ) -> ChildResult<ScopeParts<Security>> {
            let provider = self.providers.lock().unwrap().get("main").unwrap().clone();
            let templates = prepared.identity.templates.clone();
            let broker = Arc::new(EventBroker::new());
            let publisher: Arc<parrot_runtime::agent_turn::engine::Publisher> = broker.clone();
            let reminders = Arc::new(
                ExitReminders::new(
                    prepared.history.clone(),
                    publisher.clone(),
                    templates.clone(),
                )
                .await
                .unwrap(),
            );
            let activity = Arc::new(AgentSessionActivity::new(Arc::new(TestClock)));
            let active_work = Arc::new(ActiveWorkReminder {
                blockers: vec![],
                templates: templates.clone(),
            });
            let bridge = Arc::new(ScopedAgentRuntime::new(
                self.registry.lock().unwrap().clone(),
                provider.router(),
                self.user_questions.clone(),
                templates.clone(),
            ));
            let question = ScopedAgentRuntime::<Security>::child_question(&prepared.identity);
            let questions = Arc::new(parrot_runtime::interactions::ChildQuestionCoordinator::new(
                prepared.identity.session.session_id.clone(),
                bridge.clone(),
                bridge.clone(),
                bridge.clone(),
            ));
            let waiting = Arc::new(Semaphore::new(0));
            let tools = Arc::new(
                ToolSettlement::new(ToolSettlementConfig {
                    factory: Arc::new(RetainedFactory(Arc::new(RetainedTool {
                        metadata: ToolMetadata {
                            name: "wait".into(),
                            aliases: vec![],
                        },
                        tool: wait_tool(bridge.clone(), provider.router()),
                        waiting: waiting.clone(),
                    }))),
                    templates: templates.clone(),
                    output_blobs: Arc::new(
                        ToolOutputBlobStore::for_session(
                            self.histories
                                .repository
                                .events()
                                .database()
                                .selected_root(),
                            "scratch",
                            Arc::new(Names),
                        )
                        .unwrap(),
                    ),
                    publisher: broker.clone(),
                    activity: activity.clone(),
                    diagnostics: Arc::new(Diagnostics),
                    image_bytes_per_cycle: 0,
                })
                .unwrap(),
            );
            let statistics = prepared
                .history
                .repository()
                .events()
                .get_runtime_statistics()
                .await
                .unwrap();
            let ancestors = (!prepared.identity.session.parent_session_id.is_empty())
                .then(|| {
                    let parent = prepared.identity.session.parent_session_id.clone();
                    (parent.clone(), statistics.get_agent_statistics(&parent))
                })
                .into_iter()
                .collect();
            let backend = RuntimeBackend::new(RuntimeConfig {
                agent_limits: parrot_foundation::config::runtime::AgentLimitsConfig {
                    disable_max_turns: false,
                    ..Default::default()
                },
                identity: prepared.identity.session.clone(),
                history: prepared.history.clone(),
                broker: broker.clone(),
                publisher: publisher.clone(),
                selection: prepared.selection.clone(),
                tools,
                prompt: Box::new(Prompt),
                skills: Arc::new(Skills),
                context: self.histories.controller(prepared.history.clone()),
                status: Arc::new(Status),
                completions: vec![Arc::new(SessionCompletion {
                    questions: Some(questions.clone()),
                    active_work: active_work.clone(),
                    reminders: reminders.clone(),
                    history: prepared.history.clone(),
                    broker,
                    publisher,
                })],
                templates,
                reminders,
                output: Arc::new(Output),
                activity: Arc::new(Activity(activity.clone())),
                parent_completion: prepared.parent_completion,
                ancestors,
                event_id: Arc::new(id),
            })
            .await
            .unwrap();
            let session = Arc::new(AgentSession::new(backend, prepared.lifetime));
            let bound = Arc::new(AgentSessionServices {
                session: session.clone(),
                activity: activity.clone(),
                output_path: self
                    .histories
                    .path
                    .join(format!("{}.out", prepared.identity.session.name))
                    .to_string_lossy()
                    .into_owned(),
                active_work,
            });
            let adapter = Arc::new(SessionAdapter::new(
                prepared.identity.clone(),
                prepared.selection,
                session.as_ref().clone(),
            ));
            let process_port: Arc<dyn parrot_domain::contracts::status::ProcessStatusSource> =
                Arc::new(EmptyProcesses);
            self.built.lock().unwrap().insert(
                prepared.identity.session.name.clone(),
                Arc::new(Built {
                    session,
                    bridge: bridge.clone(),
                    provider,
                    history: prepared.history,
                    activity,
                    questions: questions.clone(),
                    waiting,
                }),
            );
            Ok(ScopeParts {
                session: adapter,
                services: vec![
                    ServiceRegistration::new(bound),
                    ServiceRegistration::new(bridge),
                    ServiceRegistration::new(question),
                    ServiceRegistration::new(questions),
                    ServiceRegistration::new(process_port),
                ],
                publishers: vec![],
                work: vec![],
                resources: vec![],
                final_resources: vec![],
            })
        }
    }
    impl ScopeFactory<Security> for Factory {
        fn create(
            &self,
            prepared: PreparedScope<Security>,
        ) -> BoxFuture<'_, ChildResult<ScopeParts<Security>>> {
            Box::pin(self.build(prepared))
        }
    }
    pub(super) struct Harness {
        pub(super) registry: Arc<AgentRegistry<Security>>,
        pub(super) root: Arc<Scope<Security>>,
        pub(super) built: Arc<Built>,
        factory: Arc<Factory>,
        histories: Arc<super::support::Histories>,
    }
    impl Harness {
        pub(super) async fn new() -> Self {
            // This module and the queue cases reuse the runtime helpers, whose
            // module-local counters would otherwise select the same directory.
            let path = std::env::current_dir().unwrap().join(".tmp").join(format!(
                "host-agents-{}-{}",
                std::process::id(),
                id()
            ));
            std::fs::create_dir_all(path.join("scratch/blobs")).unwrap();
            let database = SessionDatabase::open(
                SelectedSessionRoot::new(path.clone(), RootOrigin::Current).unwrap(),
            )
            .unwrap();
            let histories = Arc::new(super::support::Histories {
                path,
                repository: ConversationRepository::new(EventRepository::new(database)),
            });
            let factory = Arc::new(Factory {
                registry: Mutex::new(std::sync::Weak::new()),
                histories: histories.clone(),
                built: Mutex::new(BTreeMap::new()),
                providers: Mutex::new(BTreeMap::new()),
                user_questions: Arc::new(
                    parrot_runtime::interactions::QuestionBroker::new(
                        None,
                        Arc::new(InteractionDiagnostics),
                    )
                    .unwrap(),
                ),
            });
            let registry = AgentRegistry::new(RegistryConfig {
                agent_limits: parrot_foundation::config::runtime::AgentLimitsConfig {
                    disable_recursion_limit: false,
                    ..Default::default()
                },
                profiles: Arc::new(super::support::Profiles),
                factory: factory.clone(),
                histories: histories.clone(),
                ids: Arc::new(super::support::Ids),
                diagnostics: Arc::new(super::support::Diagnostics::default()),
                retained: RetainedAgentBudget::new(32),
                lifetime: Arc::new(Signal::new()),
                validate_child: Arc::new(|scope| {
                    scope
                        .get_service::<ScopedAgentRuntime<Security>>()?
                        .bind(scope)
                }),
            });
            *factory.registry.lock().unwrap() = Arc::downgrade(&registry);
            let identity = AgentIdentity::root(id(), "main".into(), templates());
            let history = histories.history(&identity);
            let lineage = PolicyLineage::root();
            let model = Scripted::default();
            factory
                .providers
                .lock()
                .unwrap()
                .insert("main".into(), model.clone());
            let selection = Arc::new(SelectionCell::new(
                TurnSelection {
                    model: model.selection(),
                    profile: super::support::profile("build"),
                    security: Security::new(255),
                },
                lineage.clone(),
            ));
            let parts = factory
                .build(PreparedScope {
                    identity: identity.clone(),
                    history: history.clone(),
                    selection: selection.clone(),
                    lineage,
                    parent_completion: Arc::new(NoParent),
                    lifetime: registry.lifetime(),
                })
                .await
                .unwrap();
            let root = Scope::create(ScopeConfig {
                authority: registry.clone(),
                identity,
                history,
                selection,
                parent: None,
                delivery: CompletionDelivery::RetainedOnly,
                retention: None,
                parts,
            })
            .join()
            .await
            .unwrap();
            root.get_service::<ScopedAgentRuntime<Security>>()
                .unwrap()
                .bind(&root)
                .unwrap();
            registry.register_root(root.clone()).unwrap();
            let built = factory.built.lock().unwrap().get("main").unwrap().clone();
            Self {
                registry,
                root,
                built,
                factory,
                histories,
            }
        }
        pub(super) fn built(&self, name: &str) -> Arc<Built> {
            self.factory
                .built
                .lock()
                .unwrap()
                .get(name)
                .unwrap()
                .clone()
        }
        pub(super) fn user_questions(&self) -> &parrot_runtime::interactions::QuestionBroker {
            &self.factory.user_questions
        }
        pub(super) async fn child(&self, name: &str) -> Arc<Scope<Security>> {
            self.root
                .spawner()
                .spawn_or_resume(LaunchRequest {
                    parent: self.root.session().clone(),
                    requested_name: name.into(),
                    requested_profile: "worker".into(),
                    model: self.built.provider.selection(),
                    requested_scope: String::new(),
                    delivery: CompletionDelivery::RetainedOnly,
                    boundary: HistoryForkBoundary::AfterCompletedHistory,
                    fork: parrot_storage::conversation::HistoryForkSelection::parse("empty"),
                    history_source: HistorySource::Parent,
                })
                .unwrap()
                .join()
                .await
                .unwrap()
        }
        pub(super) async fn send(
            &self,
            text: &str,
            id: &str,
        ) -> parrot_runtime::agent_turn::lifecycle::SendResult {
            self.built
                .session
                .send(
                    vec![ConversationPart::text(text)],
                    id.into(),
                    protocol::Delivery::DELIVERY_STEER,
                    IncomingActivity::default(),
                    Arc::new(NeverCancelled),
                )
                .await
                .unwrap()
        }
        pub(super) async fn invoke_wait(
            &self,
            arguments: &str,
            cancellation: &dyn Cancellation,
        ) -> Result<String, OperationError<AgentToolError>> {
            wait_tool(self.built.bridge.clone(), self.built.provider.router())
                .invoke(
                    agents::ToolInvocation {
                        arguments_json: arguments.into(),
                        assistant_sequence: 0,
                        call_id: "direct-wait".into(),
                    },
                    InvocationContext::new(
                        SharedSecurity(Security::new(255)),
                        AgentSelection {
                            requested_model: "scripted/model".into(),
                        },
                        cancellation,
                    ),
                )
                .await
        }
        pub(super) async fn waiting(&self) {
            bounded(self.built.waiting.acquire())
                .await
                .unwrap()
                .forget();
        }
        pub(super) async fn events(&self) -> Vec<Event> {
            self.histories.repository.events().replay().await.unwrap()
        }
        pub(super) async fn close(&self) {
            bounded(self.registry.shutdown().join()).await.unwrap();
        }
    }
    pub(super) fn wait_completion() -> Completion {
        Completion {
            finish_reason: "tool_calls".into(),
            tool_calls: vec![sessions::ToolCall {
                id: "wait-call".into(),
                name: "wait".into(),
                arguments_json: "{}".into(),
            }],
            ..Completion::default()
        }
    }
    pub(super) fn done_completion() -> Completion {
        Completion {
            finish_reason: "stop".into(),
            assistant_text: "done".into(),
            ..Completion::default()
        }
    }
}

use fixture::{Harness, bounded, done_completion, wait_completion};
use parrot_domain::contracts::{
    async_ops::OperationError,
    interactions::{ChildQuestionAnswers, QuestionRequester},
    runtime::AgentRuntime,
};
use parrot_provider::sessions;
use parrot_runtime::{
    agent_turn::lifecycle::{DrainState, Signal},
    interactions::QuestionDefinition,
};
use parrot_storage::events::{ConversationPart, NeverCancelled};
use parrot_tools::agents::{NamedRequest, SendRequest, input::SendInput};
use std::{sync::Arc, time::Duration};

fn assert_waking_request(h: &Harness, text: &str) {
    let requests = h.built.provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["wait"]
    );
    assert_eq!(
        requests[1]
            .messages
            .iter()
            .filter(|m| m.role == sessions::Role::User && m.content() == text)
            .count(),
        1
    );
    let results = requests[1]
        .messages
        .iter()
        .filter(|m| m.role == sessions::Role::Tool)
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].tool_call_id, "wait-call");
    assert_eq!(results[0].content(), "wait interrupted");
    assert_eq!(h.built.session.state(), DrainState::Running);
    assert_eq!(h.built.activity.capture().state, DrainState::Running);
}
async fn assert_lifecycle(h: &Harness) {
    let events = h.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_tool_started())
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_tool_finished())
            .count(),
        1
    );
    assert!(
        !events
            .iter()
            .any(|event| event.has_tool_error() || event.has_tool_cancelled())
    );
    assert_eq!(h.built.session.state(), DrainState::Idle);
    assert_eq!(h.built.activity.capture().state, DrainState::Idle);
}
#[tokio::test]
async fn retained_host_wait_promotes_new_message_once_and_ignores_duplicate() {
    let h = Harness::new().await;
    let first = h.built.provider.step(wait_completion());
    let second = h.built.provider.step(done_completion());
    h.send("first", "message-1").await;
    first.entered().await;
    first.open();
    h.waiting().await;
    h.send("first", "message-1").await;
    assert!(
        tokio::time::timeout(Duration::from_millis(20), second.entered())
            .await
            .is_err()
    );
    assert_eq!(h.built.provider.requests.lock().unwrap().len(), 1);
    let sent = h.send("wakeup", "message-2").await;
    second.entered().await;
    assert_waking_request(&h, "wakeup");
    second.open();
    assert_eq!(bounded(sent.execution.join()).await.unwrap().output, "done");
    h.close().await;
    assert_lifecycle(&h).await;
}
#[tokio::test]
async fn retained_host_wait_is_steered_by_queued_send_and_wait() {
    let h = Harness::new().await;
    let first = h.built.provider.step(wait_completion());
    let second = h.built.provider.step(done_completion());
    h.send("first", "message-1").await;
    first.entered().await;
    first.open();
    h.waiting().await;
    let session = h.built.session.clone();
    let queued = tokio::spawn(async move {
        session
            .send_and_wait(
                vec![ConversationPart::text("queued")],
                "message-2".into(),
                None,
                None,
                Arc::new(NeverCancelled),
            )
            .await
    });
    second.entered().await;
    assert_waking_request(&h, "queued");
    second.open();
    assert_eq!(bounded(queued).await.unwrap().unwrap(), "done");
    h.close().await;
    assert_lifecycle(&h).await;
}
#[tokio::test]
async fn host_retained_wait_rejects_invalid_arguments_before_registration() {
    let h = Harness::new().await;
    for arguments in [
        "{\"extra\":true}",
        "{\"duration_ms\":\"10000\"}",
        "{\"duration_ms\":9999}",
        "{\"duration_ms\":4294967295}",
        "{\"duration_ms\":10000.5}",
        "{\"duration_ms\":null}",
    ] {
        let result = h.invoke_wait(arguments, &NeverCancelled).await.unwrap();
        assert!(
            result.starts_with("error:"),
            "accepted invalid wait arguments: {arguments}: {result}"
        );
    }
    assert!(h.built.provider.requests.lock().unwrap().is_empty());
    h.close().await;
}

#[tokio::test]
async fn host_retained_wait_cancellation_releases_registration_for_next_call() {
    use std::{future::Future, task::Poll};
    let h = Harness::new().await;
    // Each call reaches the actual session registration; cancellation of the
    // previous tool must not leave its single incoming-input slot occupied.
    for arguments in [
        "{}",
        "{\"duration_ms\":10000}",
        "{\"duration_ms\":4294967294}",
    ] {
        let cancellation = Signal::new();
        let mut wait = Box::pin(h.invoke_wait(arguments, &cancellation));
        std::future::poll_fn(|cx| match wait.as_mut().poll(cx) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(result) => panic!("wait did not register: {result:?}"),
        })
        .await;
        cancellation.cancel();
        assert!(matches!(
            bounded(wait).await,
            Err(OperationError::Cancelled)
        ));
    }
    assert!(h.built.provider.requests.lock().unwrap().is_empty());
    h.close().await;
}

#[tokio::test]
async fn host_send_status_and_direct_child_interrupt_use_actual_scope_services() {
    let h = Harness::new().await;
    let child = h.child("worker").await;
    let built = h.built("worker");
    let gate = built.provider.step(done_completion());
    let sent = h
        .built
        .bridge
        .send(
            SendRequest {
                input: SendInput {
                    name: "worker".into(),
                    message: "actual send".into(),
                },
                allow_parent: true,
            },
            &NeverCancelled,
        )
        .await
        .unwrap();
    assert_eq!(sent.name, "worker");
    assert!(!sent.message_id.is_empty());
    gate.entered().await;
    let snapshot = h
        .built
        .bridge
        .status(NamedRequest {
            name: "worker".into(),
        })
        .unwrap();
    assert_eq!(snapshot.session_id, child.identity().session.session_id);
    assert_eq!(snapshot.output_path, child.get_service::<parrot_host::composition::agents::AgentSessionServices<support::Security>>().unwrap().output_path);
    assert_eq!(snapshot.activity.state, DrainState::Running);
    let forbidden = built
        .bridge
        .send(
            SendRequest {
                input: SendInput {
                    name: "parent".into(),
                    message: "forbidden".into(),
                },
                allow_parent: false,
            },
            &NeverCancelled,
        )
        .await
        .unwrap_err();
    assert!(
        forbidden
            .to_string()
            .contains("sending to the parent agent is disabled")
    );
    h.built
        .bridge
        .interrupt(
            NamedRequest {
                name: "worker".into(),
            },
            &NeverCancelled,
        )
        .await
        .unwrap();
    bounded(async {
        while built.session.is_active() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(
        h.built
            .bridge
            .interrupt(
                NamedRequest {
                    name: "worker/foreign".into()
                },
                &NeverCancelled
            )
            .await
            .is_err()
    );
    h.close().await;
}

#[tokio::test]
async fn host_spawn_uses_actual_spawner_and_admits_prompt_before_returning() {
    use parrot_runtime::tool_settlement::SharedSecurity;
    use parrot_tools::agents::{
        SpawnRequest,
        input::{ForkSelection, SpawnInput},
    };
    let h = Harness::new().await;
    let child_gate = h.built.provider.step(done_completion());
    let parent_gate = h.built.provider.step(done_completion());
    let spawned = bounded(h.built.bridge.spawn(
        SpawnRequest {
            input: SpawnInput {
                prompt: "do concrete work".into(),
                agent: "worker".into(),
                model: "scripted/model".into(),
                name: "Work A".into(),
                scope: "src".into(),
                fork: ForkSelection::Empty,
            },
            assistant_sequence: 0,
            call_id: "spawn-call".into(),
        },
        SharedSecurity(support::Security::new(255)),
        &NeverCancelled,
    ))
    .await
    .unwrap();
    assert_eq!(spawned.name, "work-a");
    assert_eq!(spawned.depth, 1);
    let child = h.root.children().resolve("work-a").unwrap();
    assert_eq!(spawned.output_path, child.get_service::<parrot_host::composition::agents::AgentSessionServices<support::Security>>().unwrap().output_path);
    child_gate.entered().await;
    assert!(
        h.built.provider.requests.lock().unwrap()[0]
            .messages
            .iter()
            .any(|message| message.content() == "do concrete work")
    );
    child_gate.open();
    parent_gate.entered().await;
    parent_gate.open();
    bounded(async {
        while h.built.session.is_active() || h.built("work-a").session.is_active() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    h.close().await;
}

fn questions(header: &str) -> Vec<QuestionDefinition> {
    vec![QuestionDefinition {
        header: header.into(),
        prompt: "Continue?".into(),
        options: vec![parrot_domain::contracts::interactions::QuestionOption {
            label: "Yes".into(),
            description: String::new(),
        }],
        multiple: false,
        custom: false,
    }]
}

async fn two_child_questions_guard_completion(promoted_before_completion: bool) {
    use parrot_domain::contracts::{security::InvocationContext, tools::Tool};
    use parrot_domain::values::tools::ConfiguredToolDefinition;
    use parrot_runtime::tool_settlement::SharedSecurity;
    use parrot_tools::agents::{
        AgentSelection, ToolInvocation,
        questions::{AnswerTool, QuestionTool},
    };
    let h = Harness::new().await;
    h.child("first").await;
    h.child("second").await;
    let first_child = h.built("first");
    let second_child = h.built("second");
    let optional = promoted_before_completion.then(|| {
        h.built.provider.step(sessions::Completion {
            finish_reason: "tool_calls".into(),
            tool_calls: vec![sessions::ToolCall {
                id: "missing-call".into(),
                name: "missing_tool".into(),
                arguments_json: "{}".into(),
            }],
            ..sessions::Completion::default()
        })
    });
    let premature = h.built.provider.step(sessions::Completion {
        finish_reason: "stop".into(),
        assistant_text: "premature".into(),
        ..sessions::Completion::default()
    });
    let finished = h.built.provider.step(sessions::Completion {
        finish_reason: "stop".into(),
        assistant_text: "finished".into(),
        ..sessions::Completion::default()
    });
    let sent = h.send("finish", "finish-message").await;
    if let Some(initial) = &optional {
        initial.entered().await;
    } else {
        premature.entered().await;
    }
    let first_tool = QuestionTool::<_, SharedSecurity<support::Security>>::new(
        ConfiguredToolDefinition::new("question", "{}"),
        first_child.bridge.clone(),
        None,
    );
    let second_tool = QuestionTool::<_, SharedSecurity<support::Security>>::new(
        ConfiguredToolDefinition::new("question", "{}"),
        second_child.bridge.clone(),
        None,
    );
    let invocation = |header: &str| ToolInvocation {
        arguments_json: format!(
            "{{\"questions\":[{{\"header\":\"{header}\",\"prompt\":\"Continue?\",\"options\":[\"Yes\"],\"custom\":false}}]}}"
        ),
        assistant_sequence: 0,
        call_id: format!("question-{header}"),
    };
    let context = || {
        InvocationContext::new(
            SharedSecurity(support::Security::new(255)),
            AgentSelection {
                requested_model: "scripted/model".into(),
            },
            &NeverCancelled,
        )
    };
    let first = first_tool.invoke(invocation("first"), context());
    let second = second_tool.invoke(invocation("second"), context());
    tokio::pin!(first, second);
    let observe = async {
        bounded(async {
            while h.built.questions.pending().len() != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        if let Some(initial) = &optional {
            initial.open();
            premature.entered().await;
        }
        premature.open();
        finished.entered().await;
        let before_answers = h.events().await;
        assert_eq!(
            before_answers
                .iter()
                .filter(
                    |event| event.agent_session_id == h.root.identity().session.session_id
                        && event.has_turn_ended()
                )
                .count(),
            0
        );
        assert_eq!(
            before_answers
                .iter()
                .filter(
                    |event| event.agent_session_id == h.root.identity().session.session_id
                        && event.has_agent_notice()
                        && event.agent_notice().kind == "pending_child_question_reminder"
                )
                .count(),
            1
        );
        {
            let requests = h.built.provider.requests.lock().unwrap();
            let last = requests.last().unwrap();
            assert!(
                last.messages
                    .iter()
                    .any(|message| message.role == sessions::Role::Assistant
                        && message.content() == "premature")
            );
            assert!(
                last.messages
                    .iter()
                    .any(|message| message.role == sessions::Role::System
                        && message.content().contains("- first")
                        && message.content().contains("- second")
                        && message.content().contains("answer"))
            );
            // Real parent-steer inputs are request-ID deduplicated and stay in
            // this parent, rather than being broadcast to another agent.
            assert_eq!(
                last.messages
                    .iter()
                    .filter(|message| message.role == sessions::Role::User
                        && message.content().contains("Child agent first"))
                    .count(),
                1
            );
        }
        let answer = AnswerTool::<_, SharedSecurity<support::Security>>::new(
            ConfiguredToolDefinition::new("answer", "{}"),
            h.built.bridge.clone(),
            None,
        );
        for name in ["first", "second"] {
            let result = answer
                .invoke(
                    ToolInvocation {
                        arguments_json: format!(
                            "{{\"agent_name\":\"{name}\",\"answers\":[\"Yes\"]}}"
                        ),
                        assistant_sequence: 0,
                        call_id: format!("answer-{name}"),
                    },
                    context(),
                )
                .await
                .unwrap();
            assert!(result.contains("Answered the pending question"));
        }
        finished.open();
    };
    let (first, second, ()) = tokio::join!(first, second, observe);
    assert!(first.unwrap().contains("Answer: Yes"));
    assert!(second.unwrap().contains("Answer: Yes"));
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().output,
        "finished"
    );
    h.close().await;
    let ended = h.events().await;
    assert_eq!(
        ended
            .iter()
            .filter(
                |event| event.agent_session_id == h.root.identity().session.session_id
                    && event.has_turn_ended()
            )
            .count(),
        1
    );
    assert!(first_child.history.conversation().await.unwrap().is_empty());
    assert!(
        second_child
            .history
            .conversation()
            .await
            .unwrap()
            .is_empty()
    );
}
#[tokio::test]
async fn host_parent_questions_guard_completion_before_promotion() {
    two_child_questions_guard_completion(false).await;
}
#[tokio::test]
async fn host_parent_questions_guard_completion_after_promotion() {
    two_child_questions_guard_completion(true).await;
}

#[tokio::test]
async fn host_root_question_uses_only_user_broker_and_cancellation_withdraws_child_question() {
    let h = Harness::new().await;
    let asking_user = h.built.bridge.ask(questions("root"), &NeverCancelled);
    let reply_user = async {
        bounded(async {
            while h.user_questions().pending().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(h.built.questions.pending().is_empty());
        assert!(!h.built.session.is_active());
        let pending = h.user_questions().pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].questions, questions("root"));
        h.user_questions()
            .reply(
                &pending[0].id,
                parrot_domain::contracts::interactions::QuestionReply::answered(vec!["Yes".into()]),
            )
            .unwrap();
    };
    let (reply, ()) = tokio::join!(asking_user, reply_user);
    assert_eq!(
        reply.unwrap(),
        parrot_domain::contracts::interactions::QuestionReply::answered(vec!["Yes".into()])
    );
    assert!(h.user_questions().pending().is_empty());
    h.child("first").await;
    let first = h.built("first");
    let cancelled = Signal::new();
    let gate = h.built.provider.step(done_completion());
    let asking = first.bridge.ask(questions("child"), &cancelled);
    tokio::pin!(asking);
    let cancel = async {
        bounded(async {
            while h.built.questions.pending().len() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        cancelled.cancel();
    };
    let (result, ()) = tokio::join!(asking, cancel);
    assert_eq!(result, Err(OperationError::Cancelled));
    assert!(h.built.questions.pending().is_empty());
    gate.entered().await;
    gate.open();
    bounded(async {
        while h.built.session.is_active() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(
        first
            .bridge
            .reply(
                "first",
                parrot_domain::contracts::interactions::QuestionReply::answered(vec!["Yes".into()])
            )
            .is_err()
    );
    h.close().await;
}
