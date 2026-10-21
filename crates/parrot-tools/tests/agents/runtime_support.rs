//! Test-only binding to retained runtime scopes. P08's production binding remains open.
//! The reused T48 snapshot intentionally cannot authorize filesystem operations (P05 unverified).
#[path = "../../../parrot-runtime/tests/children/admission_publisher.rs"]
pub mod admission_publisher;
#[path = "../../../parrot-runtime/tests/children/support.rs"]
pub mod support;

use std::{
    collections::{BTreeMap, VecDeque},
    fs::File,
    io::{self, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationError},
        runtime::AgentRuntime,
        security::{InvocationContext, RestrictableAccessSnapshot},
    },
    protocol::messages::parrot::Event,
    values::{
        accounting::RetainedAgentBudget, activity::AgentSessionActivity,
        tools::ConfiguredToolDefinition,
    },
};
use parrot_provider::{
    catalogs::Model,
    routing::{
        aliases::{
            ModelAliasCatalog, ModelAliasDefinition, ModelRouter, ModelRouting,
            ResolvedModelSelection,
        },
        registry::ProviderRegistry,
    },
    sessions::{self, Completion, Error, EventStream, Provider, Request, Session},
};
use parrot_runtime::{
    active_work::exit_reminders::ExitReminders,
    agent_turn::{
        activity::{LiveActivity, TurnDiagnostics},
        engine::{RuntimeBackend, RuntimeConfig},
        lifecycle::{AgentSession, Execution, ExecutionHandle, LifecycleError, Signal},
        provider_call::OutputWriter,
        services::*,
    },
    children::*,
    compaction::{ContextResult, ContextSnapshot, ContextStatus},
    event_broker::EventBroker,
    interactions::TokioClock,
    prompt_context::{PromptResult, SkillRendering, SystemPrompt},
};
use parrot_storage::{
    conversation::{
        ConversationMessage, HistoryForkBoundary, HistoryForkSelection, MessageRole, ToolCall,
    },
    events::{ConversationPart, NeverCancelled},
};
use parrot_tools::agents::{
    AgentSelection, AgentTool, AgentToolError, AgentToolKind, ModelSelector, NamedRequest,
    SendRequest, SendResult, SpawnRequest, SpawnResult, ToolInvocation,
    input::{ForkSelection, WaitInput},
    status::{StatusChild, StatusSnapshot},
};

pub use support::{Gate, Security, bounded};

#[derive(Default)]
pub struct StreamPlan {
    pub gate: Option<Arc<Gate>>,
    pub unwind: Option<Arc<Gate>>,
    pub answer: String,
}
#[derive(Clone, Default)]
pub struct OfflineProvider {
    pub requests: Arc<Mutex<Vec<Request>>>,
    pub plans: Arc<Mutex<VecDeque<StreamPlan>>>,
}
impl OfflineProvider {
    fn stream(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        self.requests.lock().unwrap().push(request);
        let mut plan = self.plans.lock().unwrap().pop_front().unwrap_or_default();
        if plan.answer.is_empty() {
            plan.answer = "answer".into();
        }
        Ok(Box::new(OfflineStream {
            plan,
            emitted_text: false,
            completed: false,
        }))
    }
}
impl Provider for OfflineProvider {
    fn id(&self) -> &str {
        "scripted"
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        self.stream(request)
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        Ok(Arc::new(self.clone()))
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        panic!("offline fixture has no credentials")
    }
    fn seed_models(&self) -> Vec<Model> {
        vec![]
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        panic!("offline fixture must not use network")
    }
}
impl Session for OfflineProvider {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        self.stream(request)
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
        Box::pin(async { Ok(false) })
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}
struct OfflineStream {
    plan: StreamPlan,
    emitted_text: bool,
    completed: bool,
}
impl EventStream for OfflineStream {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<sessions::Event>, Error>> {
        Box::pin(async move {
            if self.completed {
                return Ok(None);
            }
            if let Some(gate) = self.plan.gate.take() {
                gate.wait(cancellation).await?;
            }
            if !self.emitted_text {
                self.emitted_text = true;
                return Ok(Some(sessions::Event::TextDelta(self.plan.answer.clone())));
            }
            self.completed = true;
            Ok(Some(sessions::Event::Completed(Completion {
                assistant_text: self.plan.answer.clone(),
                finish_reason: "stop".into(),
                input_tokens: 3,
                output_tokens: 2,
                ..Completion::default()
            })))
        })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            if let Some(gate) = self.plan.unwind.take() {
                gate.wait(&NeverCancelled).await?;
            }
            Ok(())
        })
    }
}

pub struct Models(pub Arc<ModelRouter>);
impl ModelSelector for Models {
    fn resolve(&self, selector: &str) -> Result<String, AgentToolError> {
        self.0
            .resolve(selector)
            .map(|model| model.requested_selector.value)
            .map_err(|error| AgentToolError::Provider(error.to_string()))
    }
}
struct Profiles;
impl ProfileResolver<Security> for Profiles {
    fn resolve_child(&self, id: &str) -> ChildResult<ChildProfile<Security>> {
        let id = match id {
            "worker" | "explore" => "worker",
            "revised" => "revised",
            _ => {
                return Err(ChildError::registry(format!(
                    "profile is not agent selectable: {id}"
                )));
            }
        };
        Ok(ChildProfile {
            profile: support::profile(id),
            security: Security::new(0xff),
            recursion_limit: 8,
        })
    }
}
struct Prompt;
impl SystemPrompt for Prompt {
    fn renew_epoch(&mut self) {}
    fn sample_epoch<'a>(&'a mut self, _: &'a dyn Cancellation) -> BoxFuture<'a, PromptResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn build(&mut self, _: &ResolvedModelSelection) -> PromptResult<String> {
        Ok("offline T58 runtime fixture".into())
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
struct Diagnostics;
impl TurnDiagnostics for Diagnostics {
    fn started(&self, _: &str) {}
    fn finished(&self, _: &str, _: &str, _: Option<&str>) {}
}
struct NoParent;
impl ParentCompletion for NoParent {
    fn deliver<'a>(&'a self, _: &'a Execution) -> BoxFuture<'a, Result<(), TurnError>> {
        Box::pin(async { Ok(()) })
    }
}
struct LiveFile(Mutex<File>);
impl OutputWriter for LiveFile {
    fn append<'a>(&'a self, text: &'a str) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            let mut file = self.0.lock().unwrap();
            file.write_all(text.as_bytes())?;
            file.flush()
        })
    }
}
impl SessionOutput for LiveFile {
    fn close(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async { self.0.lock().unwrap().flush() })
    }
}
pub struct SessionServices {
    pub adapter: Arc<support::Adapter>,
    pub activity: Arc<AgentSessionActivity>,
    pub output_path: PathBuf,
    pub executions: Mutex<Vec<ExecutionHandle>>,
}
impl SessionServices {
    pub async fn settle(&self) -> Execution {
        let execution = self.executions.lock().unwrap().last().unwrap().clone();
        bounded(execution.join()).await.unwrap()
    }
}
struct Factory {
    histories: Arc<support::Histories>,
    parents: Mutex<BTreeMap<String, String>>,
}
impl ScopeFactory<Security> for Factory {
    fn create(
        &self,
        prepared: PreparedScope<Security>,
    ) -> BoxFuture<'_, ChildResult<ScopeParts<Security>>> {
        Box::pin(async move {
            let broker = Arc::new(EventBroker::new());
            let output_path = self
                .histories
                .path
                .join(format!("{}.output", prepared.identity.name_path.join("--")));
            let output = Arc::new(LiveFile(Mutex::new(File::create(&output_path).unwrap())));
            let templates = prepared.identity.templates.clone();
            let publisher: Arc<parrot_runtime::agent_turn::engine::Publisher> = broker.clone();
            let reminders = Arc::new(
                ExitReminders::new(prepared.history.clone(), publisher, templates.clone())
                    .await
                    .unwrap(),
            );
            let activity = Arc::new(AgentSessionActivity::new(Arc::new(TokioClock)));
            let statistics = prepared
                .history
                .repository()
                .events()
                .get_runtime_statistics()
                .await
                .unwrap();
            let mut ancestors = Vec::new();
            let mut parent = prepared.identity.session.parent_session_id.clone();
            while !parent.is_empty() {
                ancestors.push((parent.clone(), statistics.get_agent_statistics(&parent)));
                parent = self.parents.lock().unwrap().get(&parent).unwrap().clone();
            }
            self.parents.lock().unwrap().insert(
                prepared.identity.session.session_id.clone(),
                prepared.identity.session.parent_session_id.clone(),
            );
            let backend = RuntimeBackend::new(RuntimeConfig {
                identity: prepared.identity.session.clone(),
                history: prepared.history.clone(),
                broker: broker.clone(),
                publisher: broker,
                selection: prepared.selection.clone(),
                tools: Arc::new(support::Tools::default()),
                prompt: Box::new(Prompt),
                skills: Arc::new(Skills),
                context: self.histories.controller(prepared.history),
                status: Arc::new(Status),
                completions: vec![],
                templates,
                reminders,
                output,
                activity: Arc::new(LiveActivity::new(activity.clone(), Arc::new(Diagnostics))),
                parent_completion: prepared.parent_completion,
                ancestors,
                event_id: Arc::new(support::id),
            })
            .await
            .unwrap();
            let adapter = Arc::new(SessionAdapter::new(
                prepared.identity,
                prepared.selection,
                AgentSession::new(backend, prepared.lifetime),
            ));
            let services = Arc::new(SessionServices {
                adapter: adapter.clone(),
                activity,
                output_path,
                executions: Mutex::new(vec![]),
            });
            Ok(ScopeParts {
                session: adapter,
                services: vec![ServiceRegistration::new(services)],
                publishers: vec![],
                work: vec![],
                resources: vec![],
                final_resources: vec![],
            })
        })
    }
}

pub struct Harness {
    pub histories: Arc<support::Histories>,
    pub registry: Arc<AgentRegistry<Security>>,
    pub provider: OfflineProvider,
    pub models: Arc<Models>,
    factory: Arc<Factory>,
}
impl Default for Harness {
    fn default() -> Self {
        let histories = support::Histories::new();
        let factory = Arc::new(Factory {
            histories: histories.clone(),
            parents: Mutex::new(BTreeMap::new()),
        });
        let provider = OfflineProvider::default();
        let mut first = Model::new("first", "scripted");
        first.context_window = 100000;
        let mut second = Model::new("second", "scripted");
        second.context_window = 100000;
        let providers = Arc::new(
            ProviderRegistry::new(
                vec![Arc::new(provider.clone())],
                BTreeMap::from([("scripted".into(), vec![first, second])]),
            )
            .unwrap(),
        );
        let aliases = Arc::new(
            ModelAliasCatalog::new(
                providers.clone(),
                [
                    ModelAliasDefinition::new("fast", "scripted/first", "test"),
                    ModelAliasDefinition::new("slow", "scripted/second", "test"),
                ],
            )
            .unwrap(),
        );
        let models = Arc::new(Models(Arc::new(ModelRouter::new(
            providers,
            Arc::new(ModelRouting::new(aliases, "fast")),
        ))));
        let registry = AgentRegistry::new(RegistryConfig {
            profiles: Arc::new(Profiles),
            factory: factory.clone(),
            histories: histories.clone(),
            ids: Arc::new(support::Ids),
            diagnostics: Arc::new(support::Diagnostics::default()),
            retained: RetainedAgentBudget::new(32),
            lifetime: Arc::new(Signal::new()),
            validate_child: Arc::new(|_| Ok(())),
        });
        Self {
            histories,
            registry,
            provider,
            models,
            factory,
        }
    }
}
impl Harness {
    pub async fn root(&self, name: &str) -> Arc<Scope<Security>> {
        let identity = AgentIdentity::root(support::id(), name.into(), support::templates());
        let history = self.histories.history(&identity);
        let lineage = PolicyLineage::root();
        let selection = Arc::new(SelectionCell::new(
            TurnSelection {
                model: self.models.0.resolve("fast").unwrap(),
                profile: support::profile("root"),
                security: Security::new(0xff),
            },
            lineage.clone(),
        ));
        let parts = self
            .factory
            .create(PreparedScope {
                identity: identity.clone(),
                history: history.clone(),
                selection: selection.clone(),
                lineage,
                parent_completion: Arc::new(NoParent),
                lifetime: self.registry.lifetime(),
            })
            .await
            .unwrap();
        let root = bounded(
            Scope::create(ScopeConfig {
                authority: self.registry.clone(),
                identity,
                history,
                selection,
                parent: None,
                delivery: CompletionDelivery::RetainedOnly,
                retention: None,
                parts,
            })
            .join(),
        )
        .await
        .unwrap();
        self.registry.register_root(root.clone()).unwrap();
        root
    }
    pub async fn child(&self, parent: &Arc<Scope<Security>>, name: &str) -> Arc<Scope<Security>> {
        bounded(
            parent
                .spawner()
                .spawn(LaunchRequest {
                    parent: parent.session().clone(),
                    requested_name: name.into(),
                    requested_profile: "worker".into(),
                    model: self.models.0.resolve("fast").unwrap(),
                    requested_scope: "test scope".into(),
                    delivery: CompletionDelivery::RetainedOnly,
                    boundary: HistoryForkBoundary::AfterCompletedHistory,
                    fork: HistoryForkSelection::parse("empty"),
                    history_source: HistorySource::Parent,
                })
                .unwrap()
                .join(),
        )
        .await
        .unwrap()
    }
    pub fn bridge(&self, owner: &Arc<Scope<Security>>) -> Arc<RuntimeBridge> {
        Arc::new(RuntimeBridge {
            owner: owner.clone(),
            resolver: AgentResolver::new(owner, &self.registry),
            models: self.models.clone(),
        })
    }
    pub fn tool(
        &self,
        kind: AgentToolKind,
        owner: &Arc<Scope<Security>>,
        allow_parent: bool,
    ) -> AgentTool<RuntimeBridge, Models> {
        AgentTool::new(
            kind,
            ConfiguredToolDefinition::new("runtime fixture", "{}"),
            self.bridge(owner),
            self.models.clone(),
            None,
            allow_parent,
        )
    }
    pub async fn close(&self) {
        bounded(self.registry.shutdown().join()).await.unwrap();
    }
}

pub fn services(scope: &Arc<Scope<Security>>) -> Arc<SessionServices> {
    scope.get_service::<SessionServices>().unwrap()
}
pub fn invocation(arguments: serde_json::Value) -> ToolInvocation {
    ToolInvocation {
        arguments_json: arguments.to_string(),
        assistant_sequence: 0,
        call_id: "spawn-call".into(),
    }
}
pub fn context<'a>(
    scope: &Arc<Scope<Security>>,
    cancellation: &'a dyn Cancellation,
) -> InvocationContext<'a, Security, AgentSelection> {
    let selection = scope.session().selection().capture().unwrap();
    InvocationContext::new(
        (*selection.security).clone(),
        AgentSelection {
            requested_model: selection.model.requested_selector.value,
        },
        cancellation,
    )
}
pub async fn append(scope: &Arc<Scope<Security>>, message: ConversationMessage) -> i64 {
    scope
        .history()
        .append_conversation(
            Event {
                id: support::id(),
                agent_session_id: scope.history().agent_session_id().into(),
                ..Default::default()
            },
            message,
        )
        .await
        .unwrap();
    scope
        .history()
        .conversation()
        .await
        .unwrap()
        .last()
        .unwrap()
        .sequence
}
pub async fn spawn_batch(scope: &Arc<Scope<Security>>) -> i64 {
    let mut message = ConversationMessage::text(MessageRole::Assistant, "current spawn batch");
    message.tool_calls.push(ToolCall {
        id: "spawn-call".into(),
        name: "agent_spawn".into(),
        arguments_json: "{}".into(),
    });
    append(scope, message).await
}
pub async fn history_text(scope: &Arc<Scope<Security>>) -> Vec<String> {
    scope
        .history()
        .conversation()
        .await
        .unwrap()
        .into_iter()
        .flat_map(|item| item.message.parts)
        .filter_map(|part| {
            if let ConversationPart::Text(text) = part {
                Some(text)
            } else {
                None
            }
        })
        .collect()
}
fn error(error: impl std::fmt::Display) -> AgentToolError {
    AgentToolError::Registry(error.to_string())
}
fn operation(failure: ChildError) -> OperationError<AgentToolError> {
    match failure {
        ChildError::Cancelled => OperationError::Cancelled,
        other => OperationError::Failed(error(other)),
    }
}
fn lifecycle(error: LifecycleError) -> OperationError<AgentToolError> {
    match error {
        LifecycleError::Cancelled => OperationError::Cancelled,
        other => OperationError::Failed(AgentToolError::Service(other.to_string())),
    }
}

/// Composition glue used only by this test target, not a production adapter or security proof.
pub struct RuntimeBridge {
    owner: Arc<Scope<Security>>,
    resolver: AgentResolver<Security>,
    models: Arc<Models>,
}
impl RuntimeBridge {
    async fn send_text(
        &self,
        target: Arc<dyn ScopedSession<Security>>,
        text: String,
        cancellation: &dyn Cancellation,
    ) -> Result<SendResult, OperationError<AgentToolError>> {
        cancellation
            .check()
            .map_err(|_| OperationError::Cancelled)?;
        let signal = Arc::new(Signal::new());
        let scope = self
            .resolver
            .resolve_status_scope(&format!("/{}", target.identity().name_path.join("/")))
            .map_err(operation)?;
        let handle = services(&scope)
            .adapter
            .session()
            .send_and_select_execution(
                vec![ConversationPart::text(text)],
                support::id(),
                signal.clone(),
            )
            .map_err(lifecycle)?;
        let admitted = handle.wait(cancellation).await;
        if matches!(admitted, Err(LifecycleError::Cancelled)) {
            signal.cancel();
        }
        let admitted = admitted.map_err(lifecycle)?;
        // Retain the actual public execution, including terminal callbacks, for deterministic settlement.
        services(&scope)
            .executions
            .lock()
            .unwrap()
            .push(admitted.execution);
        Ok(SendResult {
            name: target.identity().session.name.clone(),
            message_id: admitted.admission.input.message_id,
            follow_up: admitted.follow_up,
        })
    }
}
impl AgentRuntime for RuntimeBridge {
    type Security = Security;
    type SpawnRequest = SpawnRequest;
    type SpawnResult = SpawnResult;
    type SendRequest = SendRequest;
    type SendResult = SendResult;
    type InterruptRequest = NamedRequest;
    type InterruptResult = ();
    type StatusRequest = NamedRequest;
    type StatusResult = StatusSnapshot;
    type WaitRequest = WaitInput;
    type WaitResult = String;
    type Error = AgentToolError;
    // LaunchRequest has no captured-security field. Snapshot forwarding remains a P08 boundary.
    fn spawn<'a>(
        &'a self,
        request: SpawnRequest,
        _: Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<SpawnResult, OperationError<AgentToolError>>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            let model = self
                .models
                .0
                .resolve(&request.input.model)
                .map_err(|failure| {
                    OperationError::Failed(AgentToolError::Provider(failure.to_string()))
                })?;
            let fork = match &request.input.fork {
                ForkSelection::Empty => "empty",
                ForkSelection::Full => "full",
                ForkSelection::Named(title) => title,
            };
            let child = self
                .owner
                .spawner()
                .spawn_or_resume(LaunchRequest {
                    parent: self.owner.session().clone(),
                    requested_name: request.input.name,
                    requested_profile: request.input.agent,
                    model,
                    requested_scope: request.input.scope,
                    delivery: CompletionDelivery::Automatic,
                    boundary: HistoryForkBoundary::BeforeToolBatch {
                        assistant_sequence: request.assistant_sequence,
                        tool_call_id: request.call_id,
                    },
                    fork: HistoryForkSelection::parse(fork),
                    history_source: HistorySource::Parent,
                })
                .map_err(operation)?
                .join()
                .await
                .map_err(operation)?;
            self.send_text(child.session().clone(), request.input.prompt, cancellation)
                .await?;
            Ok(SpawnResult {
                name: child.identity().session.name.clone(),
                depth: child.identity().session.depth,
                output_path: services(&child).output_path.display().to_string(),
            })
        })
    }
    fn send<'a>(
        &'a self,
        request: SendRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<SendResult, OperationError<AgentToolError>>> {
        Box::pin(async move {
            let target = self
                .resolver
                .resolve_recipient(&request.input.name)
                .map_err(operation)?;
            let ancestor = self.resolver.is_ancestor(&target);
            if ancestor && !request.allow_parent {
                return Err(OperationError::Failed(error(
                    "sending to the parent agent is disabled; report through the final message instead",
                )));
            }
            if !ancestor {
                let sender = self
                    .owner
                    .session()
                    .selection()
                    .capture()
                    .map_err(|failure| OperationError::Failed(error(failure)))?;
                let recipient = target
                    .selection()
                    .capture()
                    .map_err(|failure| OperationError::Failed(error(failure)))?;
                if sender.security.restrict_with(&recipient.security) != *recipient.security {
                    return Err(OperationError::Failed(error(
                        "sending to an agent with a more permissive security profile is not allowed",
                    )));
                }
            }
            self.send_text(target, request.input.message, cancellation)
                .await
        })
    }
    fn interrupt<'a>(
        &'a self,
        request: NamedRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<AgentToolError>>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            let child = self
                .owner
                .children()
                .resolve(&request.name)
                .map_err(operation)?;
            // The real session owns unwind. Do not await its interrupt operation on the tool path.
            services(&child)
                .adapter
                .session()
                .interrupt_handle()
                .map_err(lifecycle)?;
            Ok(())
        })
    }
    fn status(&self, request: NamedRequest) -> Result<StatusSnapshot, AgentToolError> {
        let scope = self
            .resolver
            .resolve_status_scope(&request.name)
            .map_err(error)?;
        let session = services(&scope);
        Ok(StatusSnapshot {
            session_id: scope.identity().session.session_id.clone(),
            name: scope.identity().session.name.clone(),
            output_path: session.output_path.display().to_string(),
            activity: session.activity.capture(),
            children: scope
                .children()
                .snapshot_children()
                .into_iter()
                .map(|child| StatusChild {
                    session_id: child.identity().session.session_id.clone(),
                    parent_session_id: child.identity().session.parent_session_id.clone(),
                    name: child.identity().session.name.clone(),
                    active: child.session().is_active(),
                })
                .collect(),
            processes: vec![],
        })
    }
    fn wait<'a>(
        &'a self,
        _: WaitInput,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<String, OperationError<AgentToolError>>> {
        Box::pin(async {
            Err(OperationError::Failed(AgentToolError::Service(
                "wait is outside this test bridge".into(),
            )))
        })
    }
}
