//! Test-only hosted-session boundary. This is not a production composition proof.
//! Storage acquisition, activation leases, input/event persistence, images and
//! question/permission brokers are real; agent execution and skill discovery are not.
use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation},
        storage::Committed,
    },
    protocol::messages::parrot as wire,
    values::{context::ContextSize, project_workspace::ProjectWorkspace},
};
use parrot_foundation::config::Configuration;
use parrot_host::composition::protocol::{models::ModelOperations, stream::*, *};
use parrot_provider::{
    catalogs::Model,
    routing::{
        aliases::{ModelAliasCatalog, ModelRouter, ModelRouting, ResolvedModelSelection},
        configuration::{ModelConfigurationCoordinator, definitions_from_config},
        registry::ProviderRegistry,
    },
    sessions::{
        Content, Error as ProviderError, EventStream, Provider, Request, Session,
        decorators::{UsageReporter, UsageReportingProvider},
    },
};
use parrot_runtime::{
    interactions::*,
    prompt_context::{SkillMetadata, SkillSnapshot},
};
use parrot_storage::{
    artifacts::ImageArtifactMetadata,
    diagnostics::{DiagnosticEvent, FileDiagnosticLog},
    events::{Admission, ConversationPart},
    history_files::SessionCatalog,
    ownership::*,
};
use parrot_tools::process::execution::runner::SandboxGate;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex as AsyncMutex, Semaphore, watch};

pub struct Cancel(watch::Sender<bool>);
impl Cancel {
    pub fn new(cancelled: bool) -> Arc<Self> {
        Arc::new(Self(watch::channel(cancelled).0))
    }
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }
}
impl Cancellation for Cancel {
    fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let mut receiver = self.0.subscribe();
            loop {
                if *receiver.borrow_and_update() {
                    return;
                }
                if receiver.changed().await.is_err() {
                    return;
                }
            }
        })
    }
}
pub fn active() -> Arc<Cancel> {
    Cancel::new(false)
}
pub fn checked(cancellation: &dyn Cancellation) -> Result<()> {
    cancellation.check().map_err(|_| RpcError::cancelled())
}
pub fn internal(error: impl std::fmt::Display) -> RpcError {
    RpcError::new(RpcCode::Internal, error.to_string())
}
pub async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("bounded protocol test timed out")
}
pub async fn until(mut predicate: impl FnMut() -> bool) {
    bounded(async {
        while !predicate() {
            tokio::task::yield_now().await;
        }
    })
    .await;
}
pub fn code<T>(result: Result<T>, expected: RpcCode) {
    match result {
        Err(error) => assert_eq!(error.code, expected, "{error}"),
        Ok(_) => panic!("expected {expected:?}, got success"),
    }
}

struct SyntheticProvider;
impl Provider for SyntheticProvider {
    fn id(&self) -> &str {
        "scripted"
    }
    fn call(&self, _: Request) -> std::result::Result<Box<dyn EventStream>, ProviderError> {
        panic!("offline routing fixture must not execute a provider")
    }
    fn seed_models(&self) -> Vec<Model> {
        vec![Model::new("model", "scripted")]
    }
    fn list_models<'a>(
        &'a self,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, std::result::Result<Vec<Model>, ProviderError>> {
        Box::pin(async move {
            if c.is_cancelled() {
                Err(ProviderError::Cancelled)
            } else {
                Ok(self.seed_models())
            }
        })
    }
    fn has_credential<'a>(
        &'a self,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, std::result::Result<bool, ProviderError>> {
        Box::pin(async move {
            if c.is_cancelled() {
                Err(ProviderError::Cancelled)
            } else {
                Ok(true)
            }
        })
    }
    fn calculate_image_tokens(&self, _: &Model, _: &Content) -> i64 {
        0
    }
    fn open_session(&self) -> std::result::Result<Arc<dyn Session>, ProviderError> {
        panic!("no provider session in routing fixture")
    }
}
struct Router;
impl SessionModelRouter for Router {
    fn resolve(
        &self,
        selector: &str,
    ) -> std::result::Result<SessionModelSelection, SessionStoreError> {
        Ok(SessionModelSelection {
            provider_id: "scripted".into(),
            canonical_model: "model".into(),
            selector: selector.into(),
        })
    }
}
struct Workspace;
impl SessionWorkspaceResolver for Workspace {
    fn resolve(&self, path: &Path) -> std::result::Result<PathBuf, SessionStoreError> {
        std::fs::canonicalize(path).map_err(SessionStoreError::failure)
    }
}
#[derive(Default)]
struct Diagnostics(Mutex<Vec<BrokerDiagnostic>>);
impl InteractionDiagnostics for Diagnostics {
    fn write(&self, event: BrokerDiagnostic) {
        self.0.lock().unwrap().push(event);
    }
}
#[derive(Default)]
pub struct PermissionLog(pub Mutex<Vec<String>>);
impl PermissionEvents<PermissionTargetDescription> for PermissionLog {
    fn append<'a>(
        &'a self,
        pending: &'a PendingPermission<PermissionTargetDescription>,
    ) -> BoxFuture<'a, std::result::Result<Committed<wire::Event>, InteractionError>> {
        Box::pin(async move {
            self.0
                .lock()
                .unwrap()
                .push(format!("append:{}", pending.id));
            // Fixture attestation only, not an actual durable permission append.
            Ok(Committed::after_commit(wire::Event {
                id: pending.id.clone(),
                ..Default::default()
            }))
        })
    }
    fn publish<'a>(
        &'a self,
        event: &'a Committed<wire::Event>,
    ) -> BoxFuture<'a, std::result::Result<(), InteractionError>> {
        Box::pin(async move {
            self.0
                .lock()
                .unwrap()
                .push(format!("publish:{}", event.value().id));
            Ok(())
        })
    }
}
pub struct Approval(pub AtomicBool);
impl TargetApproval<PermissionTargetDescription> for Approval {
    fn validate(
        &self,
        _: &[PermissionTargetDescription],
    ) -> std::result::Result<(), InteractionError> {
        Ok(())
    }
    fn approve(
        &self,
        _: &[PermissionTargetDescription],
    ) -> std::result::Result<(), InteractionError> {
        self.0.store(true, Ordering::SeqCst);
        Ok(())
    }
}
struct Stored {
    initialization: SessionInitialization,
    resources: Arc<SessionResourceLease>,
}
impl StoredUserSession for Stored {
    fn model(&self) -> SessionModelSelection {
        self.initialization.model.clone()
    }
    fn mode(&self) -> String {
        self.initialization.mode.clone()
    }
    fn dispose(&mut self) -> BoxFuture<'_, std::result::Result<(), SessionStoreError>> {
        Box::pin(async move {
            // This fixture factory starts no host tasks: Stored owns only its
            // initialization and lease, with no producer or pending disposer.
            // The database actor is storage-owned; this is not production host
            // settlement proof. Attest only when its actual disposer is polled.
            self.resources.confirm_producers_settled();
            Ok(())
        })
    }
}

#[tokio::test]
async fn no_producer_fixture_confirms_settlement_only_during_actual_disposal() {
    let fixture = Fixture::new();
    let session = fixture.create().await;
    let mut close = Box::pin(session.resources.close());
    std::future::poll_fn(|cx| {
        assert!(std::future::Future::poll(close.as_mut(), cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    assert!(session.resources.events.replay().await.is_ok());
    fixture.close().await;
    bounded(close).await.unwrap();
}

type QuestionTiming = (
    Option<Duration>,
    Arc<dyn parrot_domain::contracts::async_ops::Clock>,
);
#[derive(Default)]
pub struct Factory {
    pub created: Mutex<BTreeMap<String, Arc<FakeHostedSession>>>,
    pub question_timing: Mutex<Option<QuestionTiming>>,
}
impl StoredUserSessionFactory for Factory {
    fn create(
        &self,
        resources: Arc<SessionResourceLease>,
        initialization: SessionInitialization,
    ) -> BoxFuture<'_, std::result::Result<Box<dyn StoredUserSession>, SessionStoreError>> {
        Box::pin(async move {
            let mut fake = FakeHostedSession::new(resources.clone(), &initialization);
            if let Some((timeout, clock)) = self.question_timing.lock().unwrap().as_ref() {
                fake.questions = QuestionBroker::with_clock(
                    *timeout,
                    clock.clone(),
                    Arc::new(Diagnostics::default()),
                )
                .unwrap();
            }
            let fake = Arc::new(fake);
            self.created
                .lock()
                .unwrap()
                .insert(initialization.id.value().into(), fake);
            Ok(Box::new(Stored {
                initialization,
                resources,
            }) as Box<dyn StoredUserSession>)
        })
    }
}
impl HostedSessionFactory for Factory {
    fn bind_opened(&self, opened: OpenedSession) -> BoxFuture<'_, Result<Arc<dyn HostedSession>>> {
        Box::pin(async move {
            let fake = self
                .created
                .lock()
                .unwrap()
                .get(opened.initialization.id.value())
                .unwrap()
                .clone();
            assert!(Arc::ptr_eq(&fake.resources, &opened.resources));
            *fake.opened.lock().await = Some(opened);
            Ok(fake as Arc<dyn HostedSession>)
        })
    }
    fn list_modes(&self) -> Vec<String> {
        vec!["default".into(), "plan".into()]
    }
    fn resolve_mode(&self, mode: &str) -> Result<()> {
        if ["", "default", "plan"].contains(&mode) {
            Ok(())
        } else {
            Err(RpcError::new(RpcCode::InvalidArgument, "unknown mode"))
        }
    }
    fn resolve_workspace(&self, path: &Path) -> Result<ProjectWorkspace> {
        let path = std::fs::canonicalize(path)
            .map_err(|_| RpcError::new(RpcCode::InvalidArgument, "invalid workspace"))?;
        if !path.is_dir() {
            return Err(RpcError::new(
                RpcCode::InvalidArgument,
                "workspace must be a directory",
            ));
        }
        Ok(ProjectWorkspace::from_resolved(
            path.clone(),
            path,
            None,
            None,
        ))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Call {
    Update(Option<String>, Option<String>),
    Send(Vec<ConversationPart>, String, wire::Delivery),
    Interrupt,
    Compact(Option<ContextSize>),
    Goal(Option<String>),
    Instruction(String),
    Status,
    Listen(Option<String>),
    Skills,
    ConfigureSkill(String, bool),
    Opened,
}
pub struct FakeHostedSession {
    pub value: Mutex<wire::UserSession>,
    pub resources: Arc<SessionResourceLease>,
    pub root_agent: String,
    opened: AsyncMutex<Option<OpenedSession>>,
    pub questions: QuestionBroker,
    pub permissions: PermissionBroker<PermissionTargetDescription>,
    pub permission_log: Arc<PermissionLog>,
    pub skills: Mutex<SkillSnapshot>,
    pub calls: Mutex<Vec<Call>>,
    pub failure: Mutex<Option<RpcError>>,
    pub compact_gate: Mutex<Option<Arc<Semaphore>>>,
    pub permission_gate: Mutex<Option<Arc<Semaphore>>>,
    pub writers: Mutex<Vec<ClientStreamWriter<wire::Event>>>,
    pub closed: AtomicBool,
    pub lifecycle: Arc<Mutex<Vec<String>>>,
    pub close_error: AtomicBool,
}
impl FakeHostedSession {
    fn new(resources: Arc<SessionResourceLease>, init: &SessionInitialization) -> Self {
        let permissions = Arc::new(PermissionLog::default());
        Self {
            value: Mutex::new(wire::UserSession {
                id: init.id.value().into(),
                model: init.model.selector.clone(),
                mode: init.mode.clone(),
                working_directory: String::new(),
                ..Default::default()
            }),
            root_agent: init.root_agent_name.clone(),
            resources,
            opened: AsyncMutex::new(None),
            questions: QuestionBroker::new(None, Arc::new(Diagnostics::default())).unwrap(),
            permissions: PermissionBroker::new(
                init.interactive_permissions,
                None,
                Arc::new(Diagnostics::default()),
                permissions.clone(),
            )
            .unwrap(),
            permission_log: permissions,
            skills: Mutex::new(SkillSnapshot::default()),
            calls: Mutex::new(Vec::new()),
            failure: Mutex::new(None),
            compact_gate: Mutex::new(None),
            permission_gate: Mutex::new(None),
            writers: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
            lifecycle: Arc::new(Mutex::new(Vec::new())),
            close_error: AtomicBool::new(false),
        }
    }
    pub fn id(&self) -> String {
        self.snapshot().id
    }
    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
    fn route(&self, call: Call, cancellation: &dyn Cancellation) -> Result<()> {
        checked(cancellation)?;
        self.calls.lock().unwrap().push(call);
        self.failure.lock().unwrap().clone().map_or(Ok(()), Err)
    }
}
impl HostedSession for FakeHostedSession {
    fn snapshot(&self) -> wire::UserSession {
        self.value.lock().unwrap().clone()
    }
    fn resolve_mode(&self, mode: &str) -> Result<String> {
        Factory::default().resolve_mode(mode)?;
        Ok(mode.into())
    }
    fn update(
        &self,
        model: Option<ResolvedModelSelection>,
        mode: Option<String>,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.route(
                Call::Update(
                    model.as_ref().map(|m| m.requested_selector.value.clone()),
                    mode.clone(),
                ),
                active().as_ref(),
            )?;
            let mut value = self.value.lock().unwrap();
            if let Some(model) = model {
                value.model = model.requested_selector.value;
            }
            if let Some(mode) = mode {
                value.mode = mode;
            }
            Ok(())
        })
    }
    fn update_selection(
        &self,
        selection: ResolvedModelSelection,
    ) -> parrot_provider::routing::configuration::Result<()> {
        self.value.lock().unwrap().model = selection.requested_selector.value;
        Ok(())
    }
    fn persist_image<'a>(
        &'a self,
        upload: ImageUpload,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ImageArtifactMetadata>> {
        Box::pin(async move {
            self.resources
                .images
                .persist(
                    std::io::Cursor::new(upload.content),
                    &upload.upload_id,
                    &upload.display_name,
                    "upload",
                    Some(&upload.media_type),
                    Some(upload.byte_length),
                    cancellation,
                )
                .await
                .map_err(RpcError::from)
        })
    }
    fn resolve_image<'a>(
        &'a self,
        id: &'a str,
    ) -> BoxFuture<'a, Result<Option<ImageArtifactMetadata>>> {
        Box::pin(async move {
            self.resources
                .images
                .resolve(id, &parrot_storage::events::NeverCancelled)
                .await
                .map_err(RpcError::from)
        })
    }
    fn pending_questions(&self) -> Result<Vec<PendingQuestion>> {
        if let Some(error) = self.failure.lock().unwrap().clone() {
            return Err(error);
        }
        Ok(self.questions.pending())
    }
    fn reply_question(
        &self,
        id: &str,
        reply: QuestionReply,
    ) -> std::result::Result<(), InteractionError> {
        self.questions.reply(id, reply)
    }
    fn reject_question(&self, id: &str) -> std::result::Result<(), InteractionError> {
        self.questions.reject(id)
    }
    fn pending_permissions(&self) -> BoxFuture<'_, Result<Vec<wire::PendingPermission>>> {
        Box::pin(async move {
            let gate = self.permission_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.acquire().await.unwrap().forget();
            }
            if let Some(error) = self.failure.lock().unwrap().clone() {
                return Err(error);
            }
            Ok(self
                .permissions
                .pending()
                .await
                .iter()
                .map(|p| map_pending_permission(p, Clone::clone))
                .collect())
        })
    }
    fn reply_permission<'a>(
        &'a self,
        id: &'a str,
        choice: &'a str,
        reason: &'a str,
    ) -> BoxFuture<'a, std::result::Result<(), InteractionError>> {
        Box::pin(async move {
            let gate = self.permission_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.acquire().await.unwrap().forget();
            }
            self.permissions.reply(id, choice, reason).await
        })
    }
    fn refresh_skills<'a>(
        &'a self,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<SkillSnapshot>> {
        Box::pin(async move {
            self.route(Call::Skills, c)?;
            Ok(self.skills.lock().unwrap().clone())
        })
    }
    fn configure_skill<'a>(
        &'a self,
        path: &'a str,
        enabled: bool,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<SkillMetadata>> {
        Box::pin(async move {
            self.route(Call::ConfigureSkill(path.into(), enabled), c)?;
            let mut skills = self.skills.lock().unwrap();
            let skill = skills
                .skills
                .iter_mut()
                .find(|s| s.path == path)
                .ok_or_else(|| RpcError::new(RpcCode::NotFound, "unknown exact skill path"))?;
            skill.enabled = enabled;
            Ok(skill.clone())
        })
    }
    fn send(
        &self,
        parts: Vec<ConversationPart>,
        message: String,
        delivery: wire::Delivery,
        c: Arc<dyn Cancellation>,
    ) -> BoxFuture<'_, Result<Admission>> {
        Box::pin(async move {
            self.route(
                Call::Send(parts.clone(), message.clone(), delivery),
                c.as_ref(),
            )?;
            let agent = self.root_agent.clone();
            let ticket = self
                .resources
                .events
                .admit_input(
                    agent.clone(),
                    message,
                    parts,
                    delivery,
                    move |input| {
                        let mut event = wire::Event {
                            id: format!("evt-{}", input.id),
                            agent_session_id: agent,
                            ..Default::default()
                        };
                        event.set_input_admitted(wire::InputAdmitted {
                            input_id: input.id.clone(),
                            message_id: input.message_id.clone(),
                            ..Default::default()
                        });
                        Ok(event)
                    },
                    c.as_ref(),
                )
                .map_err(|error| match error {
                    parrot_domain::contracts::storage::AdmissionError::Cancelled => {
                        RpcError::cancelled()
                    }
                    parrot_domain::contracts::storage::AdmissionError::Rejected(error) => {
                        internal(error)
                    }
                })?;
            Ok(ticket
                .into_completion()
                .await
                .map_err(internal)?
                .into_value()
                .into_output())
        })
    }
    fn interrupt(&self, c: Arc<dyn Cancellation>) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { self.route(Call::Interrupt, c.as_ref()) })
    }
    fn compact(
        &self,
        target: Option<ContextSize>,
        c: Arc<dyn Cancellation>,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.route(Call::Compact(target), c.as_ref())?;
            let gate = self.compact_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                tokio::select! { _ = c.cancelled() => return Err(RpcError::cancelled()), permit = gate.acquire() => { permit.unwrap().forget(); } }
            }
            Ok(())
        })
    }
    fn set_goal(
        &self,
        goal: Option<String>,
        c: Arc<dyn Cancellation>,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { self.route(Call::Goal(goal), c.as_ref()) })
    }
    fn set_standing_instruction(&self, instruction: String) -> Result<()> {
        self.route(Call::Instruction(instruction), active().as_ref())
    }
    fn observe_status<'a>(&'a self, c: &'a dyn Cancellation) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            self.route(Call::Status, c)?;
            Ok(format!("status:{}", self.id()))
        })
    }
    fn listen(
        &self,
        replay: Option<String>,
        c: Arc<dyn Cancellation>,
    ) -> Result<Box<dyn SessionEventStream>> {
        self.route(Call::Listen(replay.clone()), c.as_ref())?;
        let (writer, reader) = bounded_client_stream();
        self.writers.lock().unwrap().push(writer);
        Ok(Box::new(ReplayStream {
            replay,
            pending: None,
            events: self.resources.events.clone(),
            reader,
        }))
    }
    fn record_opened(&self) -> Result<()> {
        self.calls.lock().unwrap().push(Call::Opened);
        Ok(())
    }
    fn write_diagnostic(&self, event: &DiagnosticEvent) {
        self.resources.write_diagnostic(event);
    }
    fn close(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            if !self.closed.swap(true, Ordering::SeqCst) {
                self.lifecycle
                    .lock()
                    .unwrap()
                    .push(format!("session:{}", self.id()));
                for writer in self.writers.lock().unwrap().drain(..) {
                    writer.stop();
                }
                self.questions.close();
                self.permissions.close().await;
                if let Some(mut opened) = self.opened.lock().await.take() {
                    opened.close().await.map_err(internal)?;
                }
            }
            if self.close_error.load(Ordering::SeqCst) {
                Err(RpcError::new(RpcCode::Internal, "session close failed"))
            } else {
                Ok(())
            }
        })
    }
}
struct ReplayStream {
    replay: Option<String>,
    pending: Option<std::collections::VecDeque<wire::Event>>,
    events: parrot_storage::events::EventRepository,
    reader: ClientStreamReader<wire::Event>,
}
impl SessionEventStream for ReplayStream {
    fn next<'a>(
        &'a mut self,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<wire::Event>>> {
        Box::pin(async move {
            checked(c)?;
            if self.pending.is_none() {
                let events = if let Some(after) = &self.replay {
                    let mut events = self.events.replay().await.map_err(internal)?;
                    if !after.is_empty() {
                        if let Some(index) = events.iter().position(|event| &event.id == after) {
                            events.drain(..=index);
                        }
                    }
                    events
                } else {
                    Vec::new()
                };
                self.pending = Some(events.into());
            }
            if let Some(event) = self.pending.as_mut().unwrap().pop_front() {
                return Ok(Some(event));
            }
            self.reader.next(c).await
        })
    }
}

#[derive(Default)]
pub struct Host {
    pub fail: AtomicBool,
    pub listener_error: AtomicBool,
}
struct Listener {
    session: Arc<dyn HostedSession>,
    fail: bool,
}
impl SessionListener for Listener {
    fn close(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            // A diagnostic proves listener close is invoked while the session's
            // actual lease remains live; session close is tested independently.
            self.session.write_diagnostic(&DiagnosticEvent::new(
                "test",
                "listener_close",
                parrot_storage::diagnostics::DiagnosticSeverity::Information,
            ));
            if self.fail {
                Err(RpcError::new(RpcCode::Internal, "listener close failed"))
            } else {
                Ok(())
            }
        })
    }
}
impl SessionHost for Host {
    fn host(
        &self,
        session: Arc<dyn HostedSession>,
        c: Arc<dyn Cancellation>,
    ) -> BoxFuture<'_, Result<Arc<dyn SessionListener>>> {
        Box::pin(async move {
            checked(c.as_ref())?;
            if self.fail.load(Ordering::SeqCst) {
                return Err(RpcError::new(RpcCode::Internal, "host failed"));
            }
            Ok(Arc::new(Listener {
                session,
                fail: self.listener_error.load(Ordering::SeqCst),
            }) as Arc<dyn SessionListener>)
        })
    }
}

pub struct Fixture {
    pub root: PathBuf,
    pub workspace: PathBuf,
    pub state: PathBuf,
    pub service: Arc<ParrotService>,
    pub factory: Arc<Factory>,
    pub store: Arc<SessionStore>,
    pub admissions: Arc<SessionAdmissions>,
    pub host: Arc<Host>,
    pub claim: Arc<WorkingDirectoryClaim>,
    pub diagnostics: Arc<FileDiagnosticLog>,
    pub sandbox: Arc<SandboxGate>,
    pub coordinator: Arc<ModelConfigurationCoordinator>,
    pub models: Arc<ModelOperations>,
}
impl Fixture {
    pub fn new() -> Self {
        Self::with_provider(Arc::new(SyntheticProvider))
    }
    pub fn with_usage_reporter(reporter: Arc<dyn UsageReporter>) -> Self {
        Self::with_provider(Arc::new(UsageReportingProvider::new(
            Arc::new(SyntheticProvider),
            reporter,
        )))
    }
    fn with_provider(provider: Arc<dyn Provider>) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::current_dir().unwrap().join(".tmp").join(format!(
            "t62-service-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let workspace = root.join("workspace");
        let state = root.join("state");
        std::fs::create_dir_all(&workspace).unwrap();
        let config = root.join("config.yaml");
        std::fs::write(&config, "model: scripted/model\nmodel_aliases:\n  test-alias:\n    model_string: scripted/model\n    usage: Test routing alias\nprovider_model_alias_defaults:\n  scripted:\n    low_llm: scripted/low\n    medium_llm: scripted/medium\n    high_llm: scripted/high\n    xhigh_llm: scripted/xhigh\n").unwrap();
        let environment = [
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "WORKDIR",
        ]
        .into_iter()
        .map(|key| (key.into(), root.join(key).to_string_lossy().into_owned()))
        .collect();
        let config =
            Configuration::load(&config, &root.join("predefined.yaml"), &environment, |_| {
                Ok(())
            })
            .unwrap();
        let registry = Arc::new(ProviderRegistry::new(vec![provider], BTreeMap::new()).unwrap());
        let aliases = Arc::new(
            ModelAliasCatalog::new(
                registry.clone(),
                definitions_from_config(&config.models.model_aliases).unwrap(),
            )
            .unwrap(),
        );
        let routing = Arc::new(ModelRouting::new(aliases, "scripted/model"));
        let router = Arc::new(ModelRouter::new(registry.clone(), routing.clone()));
        let coordinator = Arc::new(ModelConfigurationCoordinator::new(
            config,
            routing,
            router.clone(),
        ));
        let models = Arc::new(ModelOperations::new(coordinator.clone(), registry, router));
        let runtime = capture_runtime_identity("protocol-test-host").unwrap();
        let claim = Arc::new(WorkingDirectoryClaim::new(&state, runtime.clone()));
        let admissions = Arc::new(SessionAdmissions::new(
            OwnershipRoots::new(state.clone(), root.join("legacy")).unwrap(),
            runtime,
            false,
        ));
        let diagnostics = Arc::new(
            FileDiagnosticLog::open_global(
                root.join("logs"),
                FileDiagnosticLog::create_instance_id(),
                |message| panic!("diagnostics disabled: {message}"),
            )
            .unwrap(),
        );
        let factory = Arc::new(Factory::default());
        let store = Arc::new(SessionStore::new(
            admissions.clone(),
            workspace.clone(),
            "default".into(),
            factory.clone(),
            Arc::new(Router),
            Arc::new(Workspace),
            diagnostics.clone(),
        ));
        let host = Arc::new(Host::default());
        let sandbox = Arc::new(SandboxGate::new(true));
        let service = Arc::new(ParrotService::new(
            models.clone(),
            store.clone(),
            SessionCatalog::new(&state),
            factory.clone(),
            host.clone(),
            claim.clone(),
            sandbox.clone(),
            diagnostics.clone(),
            tokio::runtime::Handle::current(),
        ));
        Self {
            root,
            workspace,
            state,
            service,
            factory,
            store,
            admissions,
            host,
            claim,
            diagnostics,
            sandbox,
            coordinator,
            models,
        }
    }
    pub async fn create(&self) -> Arc<FakeHostedSession> {
        self.create_interactive(false).await
    }
    pub async fn create_interactive(&self, interactive: bool) -> Arc<FakeHostedSession> {
        let value = bounded(self.service.create_session(
            &wire::CreateSessionRequest {
                model: "scripted/model".into(),
                interactive_permissions: interactive,
                ..Default::default()
            },
            active(),
        ))
        .await
        .unwrap();
        let fake = self
            .factory
            .created
            .lock()
            .unwrap()
            .get(&value.id)
            .unwrap()
            .clone();
        fake.value.lock().unwrap().working_directory =
            self.workspace.to_string_lossy().into_owned();
        fake
    }
    pub async fn unhosted(&self) -> Arc<FakeHostedSession> {
        let opened = bounded(self.store.create_fresh(
            SessionModelSelection {
                provider_id: "scripted".into(),
                canonical_model: "model".into(),
                selector: "scripted/model".into(),
            },
            "default",
            false,
        ))
        .await
        .unwrap();
        let id = opened.initialization.id.value().to_owned();
        self.factory.bind_opened(opened).await.unwrap();
        self.factory
            .created
            .lock()
            .unwrap()
            .get(&id)
            .unwrap()
            .clone()
    }
    pub fn new_service(&self) -> ParrotService {
        ParrotService::new(
            self.models.clone(),
            self.store.clone(),
            SessionCatalog::new(&self.state),
            self.factory.clone(),
            self.host.clone(),
            self.claim.clone(),
            self.sandbox.clone(),
            self.diagnostics.clone(),
            tokio::runtime::Handle::current(),
        )
    }
    pub async fn close(&self) {
        bounded(self.service.close()).await.unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

pub struct Frames(pub std::collections::VecDeque<Result<wire::AttachmentUploadFrame>>);
impl UploadStream for Frames {
    fn next<'a>(
        &'a mut self,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<wire::AttachmentUploadFrame>>> {
        Box::pin(async move {
            checked(c)?;
            self.0.pop_front().transpose()
        })
    }
}
pub fn frames(values: Vec<wire::AttachmentUploadFrame>) -> Frames {
    Frames(values.into_iter().map(Ok).collect())
}
pub fn header(session: &str, upload: &str) -> wire::AttachmentUploadFrame {
    wire::AttachmentUploadFrame {
        payload: Some(wire::attachment_upload_frame::Payload::Header(
            wire::AttachmentUploadHeader {
                user_session_id: session.into(),
                upload_id: upload.into(),
                ..Default::default()
            },
        )),
        ..Default::default()
    }
}
pub fn chunk(bytes: Vec<u8>) -> wire::AttachmentUploadFrame {
    wire::AttachmentUploadFrame {
        payload: Some(wire::attachment_upload_frame::Payload::Chunk(bytes)),
        ..Default::default()
    }
}
pub fn description(name: &str, media: &str, length: i64) -> wire::AttachmentUploadFrame {
    wire::AttachmentUploadFrame {
        payload: Some(wire::attachment_upload_frame::Payload::Description(
            wire::AttachmentUploadDescription {
                display_name: name.into(),
                media_type: media.into(),
                byte_length: length,
                ..Default::default()
            },
        )),
        ..Default::default()
    }
}
pub fn prompt(session: &str, message: &str) -> wire::SendMessageRequest {
    wire::SendMessageRequest {
        user_session_id: session.into(),
        message_id: message.into(),
        text: "hello".into(),
        delivery: wire::Delivery::DELIVERY_STEER.into(),
        ..Default::default()
    }
}
