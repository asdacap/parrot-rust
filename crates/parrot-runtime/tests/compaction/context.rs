#[path = "image_budgets.rs"]
mod image_budgets;
#[path = "context_lifecycle.rs"]
mod lifecycle;
#[path = "publication.rs"]
mod publication;

use super::{CompactionSettings, ModelProfiles};
use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation},
        filenames::FilenameGenerator,
        paths::{RootOrigin, SelectedSessionRoot},
    },
    protocol::messages::parrot::{AgentNotice, Event},
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_provider::{
    catalogs::Model,
    routing::{
        aliases::{ModelAliasCatalog, ModelRouter, ModelRouting},
        registry::ProviderRegistry,
    },
    sessions::{
        self, Completion, Content, Error, EventStream, Provider, ProviderSessions, Request, Session,
    },
};
use parrot_runtime::{
    compaction::*,
    prompt_context::{PromptResult, SessionPrompt, SystemPrompt},
};
use parrot_storage::{
    artifacts::compaction::CompactionGroupBlobStore,
    conversation::{
        AgentConversation, AgentHistoryView, ConversationMessage, ConversationRepository,
        HistoryForkBoundary, HistoryForkSelection, MessageRole, ModelContent, ModelMessage,
        ProviderContent, ToolCall,
    },
    database::SessionDatabase,
    events::{EventRepository, NeverCancelled},
    history_files::AgentHistoryFile,
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Names;
impl FilenameGenerator for Names {
    fn next_filename(&self) -> String {
        format!("t45-{}-arse.dat", NEXT.fetch_add(1, Ordering::Relaxed))
    }
}
fn templates() -> Arc<PromptTemplateCatalog> {
    let environment = [
        ("HOME", "/tmp/t45/home"),
        ("XDG_CONFIG_HOME", "/tmp/t45/config"),
        ("XDG_STATE_HOME", "/tmp/t45/state"),
        ("XDG_DATA_HOME", "/tmp/t45/data"),
        ("WORKDIR", "/tmp/t45/workspace"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect();
    Arc::new(
        PromptTemplateCatalog::from_content("", &environment)
            .unwrap()
            .1,
    )
}
struct Fixture {
    path: PathBuf,
    repository: ConversationRepository,
}
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "t45-context-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(path.join("scratch/blobs")).unwrap();
        Self {
            repository: Self::open(&path),
            path,
        }
    }
    fn open(path: &Path) -> ConversationRepository {
        ConversationRepository::new(EventRepository::new(
            SessionDatabase::open(
                SelectedSessionRoot::new(path.to_path_buf(), RootOrigin::Current).unwrap(),
            )
            .unwrap(),
        ))
    }
    fn history_for(repository: &ConversationRepository, agent: &str) -> AgentHistoryView {
        let root = repository.events().database().selected_root().clone();
        let projection = AgentHistoryFile::new(
            root.clone(),
            root.path().join(format!("{agent}.jsonl")),
            vec![agent.into()],
        )
        .unwrap();
        repository
            .bind_agent_history(agent.into(), Arc::new(projection))
            .unwrap()
    }
    fn history(&self) -> AgentHistoryView {
        Self::history_for(&self.repository, "agent")
    }
    fn controller(&self, history: AgentHistoryView, trigger: i32) -> ContextController {
        let templates = templates();
        let compactor = Compactor::new(
            CompactionSettings {
                trigger_percent: trigger,
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
        ContextController::new(compactor, history, blobs, templates)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
fn event(agent: &str) -> Event {
    let mut event = Event::new();
    event.id = format!("t45-event-{}", NEXT.fetch_add(1, Ordering::Relaxed));
    event.agent_session_id = agent.into();
    event.set_agent_notice(AgentNotice {
        kind: "test".into(),
        notice: "synthetic".into(),
        ..AgentNotice::new()
    });
    event
}
async fn append(history: &AgentHistoryView, message: ConversationMessage) {
    history
        .append_conversation(event(history.agent_session_id()), message)
        .await
        .unwrap();
}
async fn user(history: &AgentHistoryView, text: impl Into<String>) {
    append(history, ConversationMessage::text(MessageRole::User, text)).await;
}
async fn reminder_count(history: &AgentHistoryView) -> usize {
    AgentConversation::new(history.clone())
        .await
        .unwrap()
        .messages()
        .iter()
        .filter(|message| {
            message.role == MessageRole::System
                && message.content().starts_with("Context usage entered")
        })
        .count()
}
fn call(id: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: if id == "checkpoint" {
            "set_checkpoint"
        } else {
            "read"
        }
        .into(),
        arguments_json: "{}".into(),
    }
}
async fn tool_group(history: &AgentHistoryView, ids: &[&str]) -> i64 {
    let mut assistant = ConversationMessage::text(MessageRole::Assistant, "calling");
    assistant.tool_calls = ids.iter().map(|id| call(id)).collect();
    append(history, assistant).await;
    let sequence = history
        .conversation()
        .await
        .unwrap()
        .last()
        .unwrap()
        .sequence;
    for id in ids {
        history
            .append_tool_settlement(
                event(history.agent_session_id()),
                sequence,
                parrot_storage::events::ToolExecutionTerminal {
                    tool_call_id: (*id).into(),
                    tool_name: call(id).name,
                    status: parrot_storage::events::ToolExecutionStatus::Finished,
                    result_parts: vec![parrot_storage::conversation::ConversationPart::text(
                        format!("result-{id}"),
                    )],
                    message: format!("result-{id}"),
                },
            )
            .await
            .unwrap();
    }
    sequence
}
#[derive(Clone, Default)]
struct Scripted {
    requests: Arc<Mutex<Vec<Request>>>,
    failure: Arc<Mutex<Option<Error>>>,
    closed: Arc<AtomicUsize>,
    image_reads: Arc<Mutex<Vec<Vec<u8>>>>,
    openai_images: bool,
}
impl Scripted {
    fn call_impl(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        for content in request
            .messages
            .iter()
            .flat_map(|message| message.contents())
        {
            if matches!(content, Content::Image { .. }) {
                self.image_reads.lock().unwrap().push(
                    content
                        .read_image()
                        .map_err(|error| Error::Io(error.to_string()))?,
                );
            }
        }
        self.requests.lock().unwrap().push(request);
        Ok(Box::new(ScriptStream {
            event: Some(self.failure.lock().unwrap().clone().map_or_else(
                || {
                    Ok(sessions::Event::Completed(Completion {
                        assistant_text: "summary".into(),
                        ..Completion::default()
                    }))
                },
                Err,
            )),
            closed: self.closed.clone(),
        }))
    }
    fn selection(&self, context: i32) -> parrot_provider::routing::aliases::ResolvedModelSelection {
        self.selection_model(context, "model")
    }
    fn selection_model(
        &self,
        context: i32,
        id: &str,
    ) -> parrot_provider::routing::aliases::ResolvedModelSelection {
        let selector = format!("scripted/{id}");
        let mut model = Model::new(id, "scripted");
        model.context_window = context;
        model.max_output_tokens = 1;
        let registry = Arc::new(
            ProviderRegistry::new(
                vec![Arc::new(self.clone())],
                BTreeMap::from([("scripted".into(), vec![model])]),
            )
            .unwrap(),
        );
        let aliases = Arc::new(ModelAliasCatalog::new(registry.clone(), Vec::new()).unwrap());
        ModelRouter::new(registry, Arc::new(ModelRouting::new(aliases, &selector)))
            .resolve(&selector)
            .unwrap()
    }
}
impl Provider for Scripted {
    fn calculate_image_tokens(&self, model: &Model, content: &Content) -> i64 {
        if self.openai_images {
            parrot_provider::image_inspection::calculate_content_tokens(
                &model.id,
                matches!(content, Content::Image { .. }),
                content.image_width(),
                content.image_height(),
            )
        } else {
            parrot_provider::image_inspection::FALLBACK_IMAGE_TOKENS
        }
    }
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
        panic!("no credential I/O")
    }
    fn seed_models(&self) -> Vec<Model> {
        Vec::new()
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        panic!("no catalogue I/O")
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
        panic!("no fallback")
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}
struct ScriptStream {
    event: Option<Result<sessions::Event, Error>>,
    closed: Arc<AtomicUsize>,
}
impl EventStream for ScriptStream {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<sessions::Event>, Error>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| Error::Cancelled)?;
            self.event.take().transpose()
        })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        self.closed.fetch_add(1, Ordering::Relaxed);
        Box::pin(async { Ok(()) })
    }
}
struct EpochPrompt {
    prompt: SessionPrompt,
    sampled: Arc<AtomicUsize>,
}
impl Default for EpochPrompt {
    fn default() -> Self {
        let sampled = Arc::new(AtomicUsize::new(0));
        Self {
            prompt: SessionPrompt::new(Box::new(EpochProduct {
                sampled: sampled.clone(),
                sampling: false,
            })),
            sampled,
        }
    }
}
struct EpochProduct {
    sampled: Arc<AtomicUsize>,
    sampling: bool,
}
impl SystemPrompt for EpochProduct {
    fn renew_epoch(&mut self) {
        self.sampled.fetch_add(1, Ordering::SeqCst);
    }
    fn sample_epoch<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<()>> {
        Box::pin(async move {
            self.sampling = true;
            tokio::task::yield_now().await;
            cancellation
                .check()
                .map_err(|_| parrot_runtime::prompt_context::PromptError::Cancelled)?;
            self.sampled.fetch_add(1, Ordering::SeqCst);
            self.sampling = false;
            Ok(())
        })
    }
    fn build(
        &mut self,
        _: &parrot_provider::routing::aliases::ResolvedModelSelection,
    ) -> PromptResult<String> {
        assert!(!self.sampling);
        assert!(self.sampled.load(Ordering::SeqCst) > 0);
        Ok("instructions".into())
    }
}
struct Status {
    content: String,
    fitted: String,
    estimates: Mutex<Vec<ContextSnapshot>>,
}
impl Default for Status {
    fn default() -> Self {
        Self {
            content: "status".into(),
            fitted: "fitted status".into(),
            estimates: Mutex::new(Vec::new()),
        }
    }
}
impl ContextStatus for Status {
    fn observe<'a>(
        &'a self,
        _: &'a ContextSnapshot,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<String>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| ContextError::Cancelled)?;
            Ok(self.content.clone())
        })
    }
    fn observe_fitted<'a>(
        &'a self,
        estimate: &'a (dyn Fn(&str) -> ContextResult<ContextSnapshot> + Send + Sync),
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<String>> {
        Box::pin(async move {
            self.estimates.lock().unwrap().push(estimate(&self.fitted)?);
            Ok(self.fitted.clone())
        })
    }
}
struct Events {
    history: AgentHistoryView,
    published: Mutex<Vec<Event>>,
    committed: Mutex<Vec<Event>>,
    fail_finished: AtomicBool,
}
impl Events {
    fn new(history: AgentHistoryView) -> Self {
        Self {
            history,
            published: Mutex::new(Vec::new()),
            committed: Mutex::new(Vec::new()),
            fail_finished: AtomicBool::new(false),
        }
    }
}
impl RetryObserver for Events {
    fn observe<'a>(
        &'a self,
        _: &'a sessions::Event,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move { cancellation.check().map_err(|_| Error::Cancelled) })
    }
}
impl ContextEvents for Events {
    fn emit<'a>(
        &'a self,
        event: Event,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<()>> {
        Box::pin(async move {
            assert!(!cancellation.is_cancelled());
            if event.has_compaction_finished() && self.fail_finished.load(Ordering::Relaxed) {
                return Err(ContextError::Operation("finish publication failed".into()));
            }
            self.history.append(event.clone(), None, None).await?;
            self.publish_committed(
                parrot_domain::contracts::storage::Committed::after_commit(event),
                cancellation,
            )
            .await
        })
    }
    fn publish_committed<'a>(
        &'a self,
        event: parrot_domain::contracts::storage::Committed<Event>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<()>> {
        Box::pin(async move {
            assert!(
                self.history
                    .repository()
                    .events()
                    .replay()
                    .await?
                    .iter()
                    .any(|value| value.id == event.value().id)
            );
            self.committed.lock().unwrap().push(event.value().clone());
            self.publish(event.into_value(), cancellation).await
        })
    }
    fn publish<'a>(
        &'a self,
        event: Event,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<()>> {
        Box::pin(async move {
            assert!(!cancellation.is_cancelled());
            let recorded = self
                .history
                .repository()
                .events()
                .replay()
                .await?
                .iter()
                .any(|value| value.id == event.id);
            if !recorded {
                // These source operations commit a projection/checkpoint, not an Event row.
                assert!(event.has_agent_notice());
                let conversation = self.history.conversation().await?;
                assert_eq!(
                    conversation.last().unwrap().message.role,
                    MessageRole::System
                );
                if event.agent_notice().kind == "context_reminder" {
                    assert!(self.history.latest_context_reminder().await?.is_some());
                } else {
                    assert!(
                        [
                            "status",
                            "final_provider_request_prompt",
                            "tool_availability_restored_prompt"
                        ]
                        .contains(&event.agent_notice().kind.as_str())
                    );
                }
            }
            self.published.lock().unwrap().push(event);
            Ok(())
        })
    }
}
fn input<'a>(
    selection: &'a parrot_provider::routing::aliases::ResolvedModelSelection,
    conversation: &'a mut AgentConversation,
    prompt: &'a mut EpochPrompt,
    sessions: &'a ProviderSessions,
    status: &'a Status,
    events: &'a Events,
) -> EpochInput<'a> {
    EpochInput {
        selection,
        tools: &[],
        conversation,
        prompt: &prompt.prompt,
        sessions,
        status,
        events,
    }
}

#[tokio::test]
async fn actual_checkpoint_compaction_reopen_and_full_fork_preserve_tool_causality() {
    let fixture = Fixture::new();
    let history = fixture.history();
    user(&history, "first").await;
    tool_group(&history, &["old"]).await;
    user(&history, "second").await;
    let assistant = tool_group(&history, &["checkpoint", "b"]).await;
    history
        .record_checkpoint("cut".into(), assistant, "checkpoint".into())
        .await
        .unwrap();
    let before = history.conversation().await.unwrap();
    let mut conversation = AgentConversation::new(history.clone()).await.unwrap();
    let provider = Scripted::default();
    let selection = provider.selection(10000);
    let sessions = ProviderSessions::new();
    let mut prompt = EpochPrompt::default();
    let status = Status::default();
    let events = Events::new(history.clone());
    let mut controller = fixture.controller(history.clone(), 90);
    let result = controller
        .compact(
            input(
                &selection,
                &mut conversation,
                &mut prompt,
                &sessions,
                &status,
                &events,
            ),
            None,
            &NeverCancelled,
        )
        .await
        .unwrap();
    assert!(result.reduced);
    assert_eq!(
        events
            .committed
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.has_agent_notice() && event.agent_notice().kind == "status")
            .count(),
        1
    );
    assert_eq!(prompt.sampled.load(Ordering::SeqCst), 2);
    let snapshot = fixture
        .repository
        .compaction("agent".into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.watermark, before[before.len() - 4].sequence);
    assert_eq!(
        conversation.messages()[0].content(),
        "Summary of the earlier conversation:\nsummary"
    );
    assert_eq!(conversation.messages()[1].content(), status.fitted);
    let messages = conversation.messages();
    let tools: Vec<_> = messages
        .iter()
        .filter(|message| message.role == MessageRole::Tool)
        .map(|message| message.tool_call_id.as_str())
        .collect();
    assert_eq!(tools, ["checkpoint", "b"]);
    assert_eq!(
        &history.conversation().await.unwrap()[..before.len()],
        before.as_slice()
    );
    let after = conversation.messages().to_vec();
    fixture
        .repository
        .events()
        .database()
        .close()
        .await
        .unwrap();
    let reopened = Fixture::open(&fixture.path);
    let reopened_history = Fixture::history_for(&reopened, "agent");
    assert_eq!(
        AgentConversation::new(reopened_history.clone())
            .await
            .unwrap()
            .messages(),
        after
    );
    let child = Fixture::history_for(&reopened, "child");
    child
        .initialize_fork(
            "agent".into(),
            HistoryForkBoundary::AfterCompletedHistory,
            HistoryForkSelection::parse("full"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        AgentConversation::new(child).await.unwrap().messages(),
        after
    );
    let named = Fixture::history_for(&reopened, "named");
    named
        .initialize_fork(
            "agent".into(),
            HistoryForkBoundary::AfterCompletedHistory,
            HistoryForkSelection::parse("cut"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        AgentConversation::new(named).await.unwrap().messages(),
        &after[2..]
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    sessions.close().await.unwrap();
    reopened.events().database().close().await.unwrap();
}

#[tokio::test]
async fn second_compaction_uses_snapshot_once_and_advances_only_after_new_history() {
    let fixture = Fixture::new();
    let history = fixture.history();
    user(&history, "old").await;
    user(&history, "tail").await;
    let provider = Scripted::default();
    let selection = provider.selection(10000);
    let sessions = ProviderSessions::new();
    let mut conversation = AgentConversation::new(history.clone()).await.unwrap();
    let mut prompt = EpochPrompt::default();
    let status = Status::default();
    let events = Events::new(history.clone());
    let mut controller = fixture.controller(history.clone(), 90);
    assert!(
        controller
            .compact(
                input(
                    &selection,
                    &mut conversation,
                    &mut prompt,
                    &sessions,
                    &status,
                    &events
                ),
                None,
                &NeverCancelled
            )
            .await
            .unwrap()
            .reduced
    );
    let snapshot = fixture.repository.compaction("agent".into()).await.unwrap();
    assert!(
        !controller
            .compact(
                input(
                    &selection,
                    &mut conversation,
                    &mut prompt,
                    &sessions,
                    &status,
                    &events
                ),
                None,
                &NeverCancelled
            )
            .await
            .unwrap()
            .reduced
    );
    assert_eq!(
        fixture.repository.compaction("agent".into()).await.unwrap(),
        snapshot
    );
    user(&history, "new tail").await;
    conversation.reload().await.unwrap();
    assert!(
        controller
            .compact(
                input(
                    &selection,
                    &mut conversation,
                    &mut prompt,
                    &sessions,
                    &status,
                    &events
                ),
                None,
                &NeverCancelled
            )
            .await
            .unwrap()
            .reduced
    );
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1]
            .messages
            .iter()
            .filter(|message| message
                .content()
                .starts_with("Summary of the earlier conversation:"))
            .count(),
        1
    );
    assert_eq!(
        conversation.messages().last().unwrap().content(),
        "new tail"
    );
    drop(requests);
    sessions.close().await.unwrap();
    fixture
        .repository
        .events()
        .database()
        .close()
        .await
        .unwrap();
}

#[tokio::test]
async fn no_safe_boundary_or_fitted_status_overflow_emits_failure_without_snapshot() {
    for incomplete in [true, false] {
        let fixture = Fixture::new();
        let history = fixture.history();
        if incomplete {
            let mut assistant = ConversationMessage::text(MessageRole::Assistant, "x".repeat(4000));
            assistant.tool_calls.push(call("missing"));
            append(&history, assistant).await;
        } else {
            user(&history, "old").await;
        }
        user(&history, "tail").await;
        let provider = Scripted::default();
        let selection = provider.selection(1000);
        let sessions = ProviderSessions::new();
        let mut conversation = AgentConversation::new(history.clone()).await.unwrap();
        let before = conversation.messages().to_vec();
        let mut prompt = EpochPrompt::default();
        let status = Status {
            content: "status".into(),
            fitted: "s".repeat(10000),
            ..Status::default()
        };
        let events = Events::new(history.clone());
        let mut controller = fixture.controller(history.clone(), 90);
        let error = controller
            .compact(
                input(
                    &selection,
                    &mut conversation,
                    &mut prompt,
                    &sessions,
                    &status,
                    &events,
                ),
                None,
                &NeverCancelled,
            )
            .await
            .unwrap_err();
        assert!(matches!(&error, ContextError::InvalidOperation(_)));
        assert_eq!(
            error.to_string(),
            if incomplete {
                "The conversation has no safe compaction boundary before the input limit."
            } else {
                "The compacted conversation exceeds the selected model input limit."
            }
        );
        assert!(
            fixture
                .repository
                .compaction("agent".into())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(conversation.messages(), before);
        let emitted = events.published.lock().unwrap().clone();
        assert_eq!(emitted.len(), 2);
        assert!(emitted[0].has_compaction_started());
        assert!(emitted[1].has_compaction_failed());
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            usize::from(!incomplete)
        );
        drop(emitted);
        sessions.close().await.unwrap();
        fixture
            .repository
            .events()
            .database()
            .close()
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn calibrated_prepare_awaits_new_epoch_and_resets_budget_only_after_commit() {
    let fixture = Fixture::new();
    let history = fixture.history();
    user(&history, "old").await;
    user(&history, "tail").await;
    let provider = Scripted::default();
    let selection = provider.selection(10000);
    let sessions = ProviderSessions::new();
    let mut conversation = AgentConversation::new(history.clone()).await.unwrap();
    let mut prompt = EpochPrompt::default();
    let status = Status::default();
    let events = Events::new(history.clone());
    let mut controller = fixture.controller(history.clone(), 90);
    controller.budget_mut().observe_usage(
        "scripted/model",
        1,
        &Completion {
            input_tokens: 9500,
            ..Completion::default()
        },
    );
    let prepared = controller
        .prepare_epoch(
            input(
                &selection,
                &mut conversation,
                &mut prompt,
                &sessions,
                &status,
                &events,
            ),
            &NeverCancelled,
        )
        .await
        .unwrap();
    assert!(prepared.reduced);
    assert_eq!(prompt.sampled.load(Ordering::SeqCst), 2);
    assert_eq!(
        controller
            .budget()
            .estimate_input_tokens("scripted/model", 1),
        1
    );
    assert_eq!(provider.closed.load(Ordering::Relaxed), 1);
    sessions.close().await.unwrap();
    fixture
        .repository
        .events()
        .database()
        .close()
        .await
        .unwrap();
}

#[tokio::test]
async fn durable_reminder_coalesces_after_restart_and_rebases_model_window() {
    let fixture = Fixture::new();
    let history = fixture.history();
    user(&history, "x".repeat(2000)).await;
    let provider = Scripted::default();
    let selection = provider.selection(10000);
    let sessions = ProviderSessions::new();
    let mut conversation = AgentConversation::new(history.clone()).await.unwrap();
    let mut prompt = EpochPrompt::default();
    let status = Status::default();
    let events = Events::new(history.clone());
    let mut controller = fixture.controller(history.clone(), 99);
    controller
        .prepare_epoch(
            input(
                &selection,
                &mut conversation,
                &mut prompt,
                &sessions,
                &status,
                &events,
            ),
            &NeverCancelled,
        )
        .await
        .unwrap();
    user(&history, "y".repeat(18000)).await;
    conversation.reload().await.unwrap();
    controller
        .prepare_epoch(
            input(
                &selection,
                &mut conversation,
                &mut prompt,
                &sessions,
                &status,
                &events,
            ),
            &NeverCancelled,
        )
        .await
        .unwrap();
    let checkpoint = history.latest_context_reminder().await.unwrap().unwrap();
    assert_eq!(checkpoint.percentage, 50);
    assert_eq!(checkpoint.context_limit, 10000);
    let reminders = reminder_count(&history).await;
    assert_eq!(reminders, 1);
    let mut restarted = fixture.controller(history.clone(), 99);
    let mut restarted_prompt = EpochPrompt::default();
    restarted
        .prepare_epoch(
            input(
                &selection,
                &mut conversation,
                &mut restarted_prompt,
                &sessions,
                &status,
                &events,
            ),
            &NeverCancelled,
        )
        .await
        .unwrap();
    assert_eq!(reminder_count(&history).await, reminders);
    let changed = provider.selection(20000);
    restarted
        .prepare_epoch(
            input(
                &changed,
                &mut conversation,
                &mut restarted_prompt,
                &sessions,
                &status,
                &events,
            ),
            &NeverCancelled,
        )
        .await
        .unwrap();
    assert_eq!(reminder_count(&history).await, reminders);
    user(&history, "z".repeat(40000)).await;
    conversation.reload().await.unwrap();
    restarted
        .prepare_epoch(
            input(
                &changed,
                &mut conversation,
                &mut restarted_prompt,
                &sessions,
                &status,
                &events,
            ),
            &NeverCancelled,
        )
        .await
        .unwrap();
    assert_eq!(
        history
            .latest_context_reminder()
            .await
            .unwrap()
            .unwrap()
            .context_limit,
        20000
    );
    assert_eq!(reminder_count(&history).await, reminders + 1);
    assert!(provider.requests.lock().unwrap().is_empty());
    sessions.close().await.unwrap();
    fixture
        .repository
        .events()
        .database()
        .close()
        .await
        .unwrap();
}

#[tokio::test]
async fn terminal_publication_failure_reports_failed_after_durable_snapshot() {
    let fixture = Fixture::new();
    let history = fixture.history();
    user(&history, "old").await;
    user(&history, "tail").await;
    let provider = Scripted::default();
    let selection = provider.selection(10000);
    let sessions = ProviderSessions::new();
    let mut conversation = AgentConversation::new(history.clone()).await.unwrap();
    let mut prompt = EpochPrompt::default();
    let status = Status::default();
    let events = Events::new(history.clone());
    events.fail_finished.store(true, Ordering::Relaxed);
    let mut controller = fixture.controller(history.clone(), 90);
    let error = controller
        .compact(
            input(
                &selection,
                &mut conversation,
                &mut prompt,
                &sessions,
                &status,
                &events,
            ),
            None,
            &NeverCancelled,
        )
        .await
        .unwrap_err();
    assert!(matches!(&error, ContextError::Operation(_)));
    assert_eq!(error.to_string(), "finish publication failed");
    assert!(
        fixture
            .repository
            .compaction("agent".into())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        events
            .published
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.has_compaction_failed())
            .count(),
        1
    );
    assert_eq!(
        AgentConversation::new(history).await.unwrap().messages(),
        conversation.messages()
    );
    sessions.close().await.unwrap();
    fixture
        .repository
        .events()
        .database()
        .close()
        .await
        .unwrap();
}

#[test]
fn storage_materialization_mapping_preserves_images_and_native_reasoning() {
    let message = ModelMessage {
        role: MessageRole::Assistant,
        contents: vec![
            ModelContent::Text("text".into()),
            ModelContent::ImageFile {
                path: PathBuf::from("/not-read.png"),
                media_type: "image/png".into(),
                width: 12,
                height: 34,
            },
        ],
        tool_calls: vec![call("tool")],
        tool_call_id: "result".into(),
        provider_content: ProviderContent {
            provider_id: "responses".into(),
            content: "native reasoning".into(),
        },
    };
    let mapped = provider_message(&message);
    assert_eq!(mapped.content(), "text");
    assert_eq!(mapped.provider_id, "responses");
    assert_eq!(mapped.provider_content, "native reasoning");
    assert_eq!(mapped.tool_calls[0].id, "tool");
    assert_eq!(mapped.tool_call_id, "result");
    assert!(
        matches!(&mapped.contents()[1], Content::Image { path, width:12, height:34, bytes:None, .. } if path == "/not-read.png")
    );
}
