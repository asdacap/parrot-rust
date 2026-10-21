pub use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationError},
        events::EventPublisher,
        filenames::FilenameGenerator,
        paths::{ResolvedRead, RootOrigin, SelectedSessionRoot},
        security::AccessSnapshot,
        storage::Committed,
    },
    protocol::messages::parrot::{self as protocol, Event},
    values::config::ModelProfiles,
};
pub use parrot_foundation::template_catalog::PromptTemplateCatalog;
pub use parrot_provider::{
    catalogs::Model,
    routing::{
        aliases::{ModelAliasCatalog, ModelRouter, ModelRouting},
        registry::ProviderRegistry,
    },
    sessions::{self, Completion, Error, EventStream, Provider, Request, Session},
};
pub use parrot_runtime::{
    agent_turn::services::*,
    compaction::*,
    event_broker::{BrokerError, EventBroker},
    prompt_context::{PromptResult, SystemPrompt},
};
pub use parrot_storage::{
    artifacts::compaction::CompactionGroupBlobStore,
    conversation::{
        AgentConversation, AgentHistoryView, ConversationMessage, ConversationRepository,
        MessageRole,
    },
    database::SessionDatabase,
    events::{ConversationPart, EventRepository, NeverCancelled},
    history_files::AgentHistoryFile,
};
pub use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
pub use tokio::sync::Semaphore;

static NEXT: AtomicU64 = AtomicU64::new(0);
pub fn id() -> String {
    format!("t47-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}
pub async fn bounded<F: std::future::Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("T47 owned work failed to join")
}
pub fn call(id: &str, name: &str) -> sessions::ToolCall {
    sessions::ToolCall {
        id: id.into(),
        name: name.into(),
        arguments_json: "{}".into(),
    }
}
pub struct Names;
impl FilenameGenerator for Names {
    fn next_filename(&self) -> String {
        format!("{}-arse.dat", id())
    }
}
pub fn templates() -> Arc<PromptTemplateCatalog> {
    let env = [
        ("HOME", "/tmp/t47/home"),
        ("XDG_CONFIG_HOME", "/tmp/t47/config"),
        ("XDG_STATE_HOME", "/tmp/t47/state"),
        ("XDG_DATA_HOME", "/tmp/t47/data"),
        ("WORKDIR", "/tmp/t47/work"),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .collect();
    Arc::new(PromptTemplateCatalog::from_content("", &env).unwrap().1)
}
pub struct Fixture {
    pub path: PathBuf,
    pub repository: ConversationRepository,
    pub history: AgentHistoryView,
}
impl Fixture {
    pub fn new() -> Self {
        let path = std::env::current_dir().unwrap().join(".tmp").join(format!(
            "settlement-{}-{}",
            std::process::id(),
            id()
        ));
        std::fs::create_dir_all(path.join("scratch/blobs")).unwrap();
        let repository = Self::open(&path);
        let history = Self::history_for(&path, &repository);
        Self {
            path,
            repository,
            history,
        }
    }
    pub fn open(path: &Path) -> ConversationRepository {
        let root = SelectedSessionRoot::new(path.into(), RootOrigin::Current).unwrap();
        ConversationRepository::new(EventRepository::new(
            SessionDatabase::open(root.clone()).unwrap(),
        ))
        .with_image_store(
            parrot_storage::artifacts::ImageArtifactStore::for_session(&root, "resources").unwrap(),
        )
        .unwrap()
    }
    pub fn history_for(path: &Path, repository: &ConversationRepository) -> AgentHistoryView {
        repository
            .bind_agent_history(
                "agent".into(),
                Arc::new(
                    AgentHistoryFile::new(
                        repository.events().database().selected_root().clone(),
                        path.join("agent.jsonl"),
                        vec!["agent".into()],
                    )
                    .unwrap(),
                ),
            )
            .unwrap()
    }
    pub async fn batch(&self, calls: Vec<sessions::ToolCall>) -> i64 {
        let mut event = Event::new();
        event.id = id();
        event.agent_session_id = "agent".into();
        let mut message = ConversationMessage::text(MessageRole::Assistant, "tool batch");
        message.tool_calls = calls
            .into_iter()
            .map(|call| parrot_storage::events::ToolCall {
                id: call.id,
                name: call.name,
                arguments_json: call.arguments_json,
            })
            .collect();
        self.history
            .append_conversation(event, message)
            .await
            .unwrap();
        self.history
            .conversation()
            .await
            .unwrap()
            .last()
            .unwrap()
            .sequence
    }
    pub async fn seed(&self) {
        for index in 0..6 {
            for role in [MessageRole::User, MessageRole::Assistant] {
                let mut event = Event::new();
                event.id = id();
                event.agent_session_id = "agent".into();
                self.history
                    .append_conversation(
                        event,
                        ConversationMessage::text(
                            role,
                            format!("seed-{index}-{}", "x ".repeat(200)),
                        ),
                    )
                    .await
                    .unwrap();
            }
        }
    }
    pub fn controller(&self) -> ContextController {
        let templates = templates();
        let compactor = Compactor::new(
            CompactionSettings {
                trigger_percent: 95,
                target_percent: 30,
                maximum_input_tokens: 60000,
                summary_output_tokens: 1024,
                summary_instructions: String::new(),
                summary_prefix: String::new(),
            },
            ModelProfiles::new(&BTreeMap::new()),
            templates.clone(),
        )
        .unwrap();
        let blobs = CompactionGroupBlobStore::for_session(
            self.repository.events().database().selected_root(),
            "scratch",
            Arc::new(Names),
        )
        .unwrap();
        ContextController::new(compactor, self.history.clone(), blobs, templates)
    }
    pub async fn events(&self) -> Vec<Event> {
        self.repository.events().replay().await.unwrap()
    }
    pub async fn conversation(&self) -> AgentConversation {
        AgentConversation::new(self.history.clone()).await.unwrap()
    }
    pub fn jsonl(&self) -> String {
        std::fs::read_to_string(self.path.join("agent.jsonl")).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Immutable version; replacement captures never mutate authority of an already-running call.
pub struct Security {
    pub version: usize,
}
impl AccessSnapshot for Security {
    type Error = ();
    fn check_read(&self, _: &ResolvedRead) -> Result<(), ()> {
        panic!("inert authority version is not filesystem permission")
    }
    fn check_write(&self, _: &Path) -> Result<(), ()> {
        panic!("inert authority version is not filesystem permission")
    }
}
pub struct Selection(pub Mutex<TurnSelection<Security>>);
impl SelectionSource<Security> for Selection {
    fn capture(&self) -> Result<TurnSelection<Security>, TurnError> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn update(&self, selection: TurnSelection<Security>) -> Result<(), TurnError> {
        *self.0.lock().unwrap() = selection;
        Ok(())
    }
}
pub fn selection(provider: &Scripted) -> TurnSelection<Security> {
    TurnSelection {
        model: provider.selection(),
        profile: TurnProfile {
            id: "original-profile".into(),
            maximum_requests: 8,
            allowed_tools: None,
            disabled_tools: vec![],
        },
        security: Arc::new(Security { version: 1 }),
    }
}
pub struct Gate {
    pub entered: Semaphore,
    pub release: Semaphore,
}
impl Gate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        })
    }
    pub async fn entered(&self) {
        bounded(self.entered.acquire()).await.unwrap().forget();
    }
    pub fn open(&self) {
        self.release.add_permits(1);
    }
    pub async fn wait(&self, cancellation: &dyn Cancellation) -> Result<(), Error> {
        self.entered.add_permits(1);
        tokio::select! { permit=self.release.acquire() => { permit.unwrap().forget(); Ok(()) }, _=cancellation.cancelled() => Err(Error::Cancelled) }
    }
}
pub struct Prompt;
impl SystemPrompt for Prompt {
    fn renew_epoch(&mut self) {}
    fn sample_epoch<'a>(&'a mut self, _: &'a dyn Cancellation) -> BoxFuture<'a, PromptResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn build(
        &mut self,
        _: &parrot_provider::routing::aliases::ResolvedModelSelection,
    ) -> PromptResult<String> {
        Ok("settlement instructions".into())
    }
}
pub struct Status;
impl ContextStatus for Status {
    fn observe<'a>(
        &'a self,
        _: &'a ContextSnapshot,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<String>> {
        Box::pin(async { Ok("status".into()) })
    }
    fn observe_fitted<'a>(
        &'a self,
        estimate: &'a (dyn Fn(&str) -> ContextResult<ContextSnapshot> + Send + Sync),
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<String>> {
        Box::pin(async move {
            estimate("status")?;
            Ok("status".into())
        })
    }
}
impl TurnStatus<Security> for Status {
    fn bind(&self, _: &TurnSelection<Security>) -> Arc<dyn ContextStatus> {
        Arc::new(Status)
    }
}

pub struct Step {
    pub completion: Result<Completion, Error>,
    pub gate: Option<Arc<Gate>>,
}
impl Step {
    pub fn complete(text: &str) -> Self {
        Self {
            completion: Ok(Completion {
                assistant_text: text.into(),
                finish_reason: "stop".into(),
                input_tokens: 17,
                output_tokens: 9,
                ..Completion::default()
            }),
            gate: None,
        }
    }
}
#[derive(Clone, Default)]
pub struct Scripted {
    pub steps: Arc<Mutex<VecDeque<Step>>>,
    pub requests: Arc<Mutex<Vec<Request>>>,
    pub closes: Arc<AtomicUsize>,
}
impl Scripted {
    pub fn new(steps: Vec<Step>) -> Self {
        Self {
            steps: Arc::new(Mutex::new(steps.into())),
            ..Self::default()
        }
    }
    pub fn selection(&self) -> parrot_provider::routing::aliases::ResolvedModelSelection {
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
        ModelRouter::new(
            registry,
            Arc::new(ModelRouting::new(aliases, "scripted/model")),
        )
        .resolve("scripted/model")
        .unwrap()
    }
    fn call_impl(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        self.requests.lock().unwrap().push(request);
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected provider request");
        Ok(Box::new(Stream {
            step: Some(step),
            closes: self.closes.clone(),
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
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        panic!("no credentials")
    }
    fn seed_models(&self) -> Vec<Model> {
        vec![]
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        panic!("no network")
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
    step: Option<Step>,
    closes: Arc<AtomicUsize>,
}
impl EventStream for Stream {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<sessions::Event>, Error>> {
        Box::pin(async move {
            let Some(step) = self.step.take() else {
                return Ok(None);
            };
            if let Some(gate) = step.gate {
                gate.wait(cancellation).await?;
            }
            cancellation.check().map_err(|_| Error::Cancelled)?;
            Ok(Some(sessions::Event::Completed(step.completion?)))
        })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}
