use crate::{raw_conversation, request_tokens};
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::messages::parrot::CompactionProgressEvent,
    values::config::{ModelProfileConfig, ModelProfiles},
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_provider::compaction::{
    CompactionEffects, CompactionEvent, CompactionSettings, Compactor, ContextCapture,
    ReplacementCommit,
};
use parrot_provider::{
    catalogs::Model,
    http_stream::{ByteStream, HttpError, HttpRequest, HttpResponse, HttpTransport},
    responses_websocket::{ConnectedSocket, Connector, Headers, WebSocketError},
    routing::{
        aliases::{ModelAliasCatalog, ModelRouter, ModelRouting, ResolvedModelSelection},
        registry::ProviderRegistry,
    },
    sessions::{
        Completion, Content, Error, Event, EventStream, Message, Provider, ProviderSessions,
        ReasoningKind, Request, Role, Session, ToolCall,
        diagnostics::{
            DiagnosticEvent, DiagnosticSink, LastRequestDumper, MonotonicDiagnosticClock,
        },
        provider::{ApiKeySource, CompatibleOptions, OpenAICompatibleProvider},
    },
};
use parrot_runtime::compaction::CompactionObserver;
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct Token(AtomicBool);
impl Cancellation for Token {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

#[derive(Default)]
struct State {
    scripts: Mutex<VecDeque<Vec<Result<Event, Error>>>>,
    requests: Mutex<Vec<Request>>,
    audit: Mutex<Vec<&'static str>>,
    progress: Mutex<Vec<CompactionProgressEvent>>,
    opens: AtomicUsize,
    stream_closes: AtomicUsize,
    session_closes: AtomicUsize,
    close_failure: Mutex<Option<Error>>,
}
struct FakeProvider(Arc<State>);
impl Provider for FakeProvider {
    fn id(&self) -> &str {
        "provider"
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        panic!("no credential access")
    }
    fn seed_models(&self) -> Vec<Model> {
        Vec::new()
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        panic!("no model discovery")
    }
    fn calculate_image_tokens(&self, _: &Model, _: &Content) -> i64 {
        2000
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        self.0.opens.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(FakeSession(self.0.clone())))
    }
}
struct FakeSession(Arc<State>);
impl Session for FakeSession {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        self.0.requests.lock().unwrap().push(request);
        let events = self
            .0
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected provider call");
        Ok(Box::new(FakeStream {
            state: self.0.clone(),
            events: events.into(),
        }))
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
        panic!("no fallback")
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async {
            self.0.session_closes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}
struct FakeStream {
    state: Arc<State>,
    events: VecDeque<Result<Event, Error>>,
}
impl EventStream for FakeStream {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<Event>, Error>> {
        Box::pin(async move {
            self.state.audit.lock().unwrap().push("next");
            cancellation.check().map_err(|_| Error::Cancelled)?;
            self.events.pop_front().transpose()
        })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async {
            self.state.audit.lock().unwrap().push("close");
            self.state.stream_closes.fetch_add(1, Ordering::SeqCst);
            self.state
                .close_failure
                .lock()
                .unwrap()
                .take()
                .map_or(Ok(()), Err)
        })
    }
}
struct Observer {
    state: Arc<State>,
    cancel: Option<Arc<Token>>,
    failure: Option<Error>,
}
impl CompactionObserver for Observer {
    fn observe<'a>(
        &'a self,
        event: &'a Event,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            assert!(matches!(event.base_event(), Event::Retry { .. }));
            self.state.audit.lock().unwrap().push("retry-start");
            tokio::task::yield_now().await;
            if let Some(token) = &self.cancel {
                token.0.store(true, Ordering::SeqCst);
            }
            self.state.audit.lock().unwrap().push("retry-finish");
            self.failure.clone().map_or(Ok(()), Err)
        })
    }
    fn observe_progress<'a>(
        &'a self,
        progress: CompactionProgressEvent,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| Error::Cancelled)?;
            self.state.progress.lock().unwrap().push(progress);
            Ok(())
        })
    }
}
pub(crate) struct Effects<'a> {
    pub(crate) history: &'a [Message],
    pub(crate) instructions: &'a str,
    pub(crate) status: &'a str,
    pub(crate) observer: &'a dyn CompactionObserver,
    pub(crate) replacement: Option<ReplacementCommit>,
}

#[derive(Debug)]
pub(crate) struct ObservedResult {
    pub(crate) history: Vec<Message>,
    pub(crate) summary: Message,
    pub(crate) retained_messages: usize,
    pub(crate) watermark: i64,
}

impl Effects<'_> {
    pub(crate) fn result(&self, message: parrot_core::Message) -> ObservedResult {
        let replacement = self
            .replacement
            .as_ref()
            .expect("returned replacement was committed");
        assert_eq!(replacement.message, message);
        let retained: Vec<_> = self
            .history
            .iter()
            .enumerate()
            .filter(|(index, _)| *index as i64 + 1 > replacement.watermark)
            .map(|(_, message)| message.clone())
            .collect();
        let summary: Message = message.into();
        let mut history = vec![summary.clone(), Message::system(&replacement.status)];
        history.extend(retained.iter().cloned());
        ObservedResult {
            history,
            summary,
            retained_messages: retained.len(),
            watermark: replacement.watermark,
        }
    }
}

fn core_error(error: Error) -> parrot_core::Error {
    let code = parrot_provider::sessions::diagnostic_session::failure_code(&error);
    if code == parrot_core::ErrorCode::Cancelled {
        return parrot_core::Error::cancelled();
    }
    parrot_core::Error {
        code,
        response_body: error.response_body().into(),
        ..parrot_core::Error::failed(error.to_string())
    }
}

impl CompactionEffects for Effects<'_> {
    fn capture_context<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ContextCapture, parrot_core::Error>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| parrot_core::Error::cancelled())?;
            Ok(ContextCapture {
                instructions: self.instructions.into(),
                conversation: raw_conversation(self.history),
            })
        })
    }
    fn offer_status<'a>(
        &'a self,
        _: parrot_core::ContextSnapshot,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<parrot_core::StatusOffering, parrot_core::Error>> {
        Box::pin(async move {
            let mut offering = parrot_core::StatusOffering::default();
            offering.insert(
                parrot_core::StatusKey::new("status"),
                parrot_core::StatusOffer {
                    hash: String::new(),
                    text: self.status.into(),
                    is_auto_update: false,
                },
            );
            Ok(offering)
        })
    }
    fn commit_replacement<'a>(
        &'a mut self,
        replacement: ReplacementCommit,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), parrot_core::Error>> {
        Box::pin(async move {
            assert!(self.replacement.is_none(), "one replacement per operation");
            self.replacement = Some(replacement);
            Ok(())
        })
    }
    fn publish<'a>(
        &'a self,
        event: CompactionEvent,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), parrot_core::Error>> {
        Box::pin(async move {
            match event {
                CompactionEvent::Retry(event) => self.observer.observe(&event, cancellation).await.map_err(core_error),
                CompactionEvent::Progress(progress) => self.observer.observe_progress(CompactionProgressEvent {
                    phase: parrot_domain::protocol::messages::parrot::CompactionPhase::COMPACTION_PHASE_SUMMARIZING.into(),
                    completed_groups: progress.completed_groups as u64,
                    total_groups: progress.total_groups as u64,
                    ..CompactionProgressEvent::new()
                }, cancellation).await.map_err(core_error),
                _ => Ok(()),
            }
        })
    }
}

struct Fixture {
    path: PathBuf,
    state: Arc<State>,
    selection: ResolvedModelSelection,
    sessions: ProviderSessions,
    token: Arc<Token>,
    observer: Observer,
}
impl Fixture {
    fn new(limit: i32, scripts: Vec<Vec<Result<Event, Error>>>) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "t45-execution-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(path.join("agent/blobs")).unwrap();
        let state = Arc::new(State::default());
        *state.scripts.lock().unwrap() = scripts.into();
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider(state.clone()));
        let mut model = Model::new("model", "provider");
        model.context_window = limit;
        model.max_output_tokens = 1;
        let registry = Arc::new(
            ProviderRegistry::new(
                vec![provider],
                BTreeMap::from([("provider".into(), vec![model])]),
            )
            .unwrap(),
        );
        let aliases = Arc::new(ModelAliasCatalog::new(registry.clone(), []).unwrap());
        let routing = Arc::new(ModelRouting::new(aliases, "provider/model"));
        let selection = ModelRouter::new(registry, routing)
            .resolve("provider/model")
            .unwrap();
        Self {
            path,
            selection,
            sessions: ProviderSessions::new(),
            token: Arc::new(Token::default()),
            observer: Observer {
                state: state.clone(),
                cancel: None,
                failure: None,
            },
            state,
        }
    }
    async fn compact(
        &mut self,
        compactor: &Compactor,
        history: &[Message],
    ) -> Result<Option<ObservedResult>, parrot_core::Error> {
        let mut effects = Effects {
            history,
            instructions: "instructions",
            status: "status",
            observer: &self.observer,
            replacement: None,
        };
        let replacement = compactor
            .compact(
                &self.selection,
                parrot_core::CompactionRequest::default(),
                &self.sessions,
                &mut effects,
                self.token.as_ref(),
            )
            .await?;
        Ok(replacement.map(|message| effects.result(message)))
    }
    fn requests(&self) -> Vec<Request> {
        self.state.requests.lock().unwrap().clone()
    }
    fn blob_files(&self) -> Vec<PathBuf> {
        fs::read_dir(self.path.join("agent/blobs"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect()
    }
    async fn close(&self) {
        self.sessions.close().await.unwrap();
        assert_eq!(
            self.state.session_closes.load(Ordering::SeqCst),
            usize::from(self.state.opens.load(Ordering::SeqCst) > 0)
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.path).unwrap();
    }
}
fn done(text: &str) -> Result<Event, Error> {
    Ok(Event::Completed(Completion {
        assistant_text: text.into(),
        ..Completion::default()
    }))
}
fn retry() -> Result<Event, Error> {
    Ok(Event::Retry {
        attempt: 2,
        retry_after: Duration::from_secs(3),
        reason: "temporary".into(),
    })
}
fn compactor(input_budget: i32) -> Compactor {
    configured_compactor(input_budget, "[...]", ModelProfiles::new(&BTreeMap::new()))
}
fn configured_compactor(input_budget: i32, notice: &str, profiles: ModelProfiles) -> Compactor {
    let yaml = format!(
        "prompt_templates:\n  compaction.summary-instructions:\n    template: 'Sum.'\n  compaction.summary-prefix:\n    template: 'S:'\n  compaction.truncated-group-notice:\n    template: '{notice}'\n    allowed_arguments: []\n    required_arguments: []\n"
    );
    let environment = [
        ("HOME", "/tmp/t45-config/home"),
        ("XDG_CONFIG_HOME", "/tmp/t45-config/config"),
        ("XDG_STATE_HOME", "/tmp/t45-config/state"),
        ("XDG_DATA_HOME", "/tmp/t45-config/data"),
        ("WORKDIR", "/tmp/t45-config/workspace"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect();
    let (_, templates) = PromptTemplateCatalog::from_content(&yaml, &environment).unwrap();
    Compactor::new(
        CompactionSettings {
            trigger_percent: 90,
            target_percent: 50,
            maximum_input_tokens: input_budget,
            maximum_output_tokens: 0,
            summary_output_tokens: 45,
            summary_instructions: "ignored settings instructions".into(),
            summary_prefix: "ignored settings prefix".into(),
        },
        profiles,
        Arc::new(templates),
    )
    .unwrap()
}
fn tool_group(text: &str) -> Vec<Message> {
    vec![
        Message::assistant(
            "",
            vec![ToolCall {
                id: "call".into(),
                name: "read".into(),
                arguments_json: "{\"x\":1}".into(),
            }],
        ),
        Message::tool_result("call", text),
    ]
}
fn texts(request: &Request) -> Vec<&str> {
    request.messages.iter().map(Message::content).collect()
}

struct ArchiveKey;
impl ApiKeySource for ArchiveKey {
    fn api_key<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async { Ok("private-archive-key".into()) })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
}
struct NoSocket;
impl Connector for NoSocket {
    fn connect<'a>(
        &'a self,
        _: &'a str,
        _: &'a Headers,
        _: Option<Duration>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ConnectedSocket, WebSocketError>> {
        Box::pin(async { panic!("summary request must use HTTP") })
    }
}
const SUMMARY_RESPONSE: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"private archive summary\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
struct SummaryBody(&'static [u8]);
impl ByteStream for SummaryBody {
    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            let count = buffer.len().min(self.0.len());
            buffer[..count].copy_from_slice(&self.0[..count]);
            self.0 = &self.0[count..];
            Ok(count)
        })
    }
}
#[derive(Default)]
struct SummaryTransport(Mutex<Vec<HttpRequest>>);
impl HttpTransport for SummaryTransport {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            self.0.lock().unwrap().push(request);
            Ok(HttpResponse {
                status: 200,
                reason: "OK".into(),
                headers: Vec::new(),
                body: Box::new(SummaryBody(SUMMARY_RESPONSE.as_bytes())),
            })
        })
    }
}
#[derive(Default)]
struct ArchiveDiagnostics(Mutex<Vec<DiagnosticEvent>>);
impl DiagnosticSink for ArchiveDiagnostics {
    fn write(&self, event: DiagnosticEvent) {
        self.0.lock().unwrap().push(event);
    }
}
#[derive(Debug, PartialEq, serde::Deserialize)]
#[serde(tag = "encoding", content = "data", rename_all = "snake_case")]
enum ArchivedBody {
    Utf8(String),
    Bytes(Vec<u8>),
}
#[derive(serde::Deserialize)]
struct ArchivedRequest {
    number: u64,
    transport: String,
    request: Option<ArchivedBody>,
    response: Vec<ArchivedBody>,
    outcome: String,
    error_code: Option<String>,
}
#[derive(serde::Deserialize)]
struct SummaryRequest {
    model: String,
    messages: Vec<SummaryMessage>,
    max_tokens: i32,
}
#[derive(Debug, PartialEq, serde::Deserialize)]
struct SummaryMessage {
    role: String,
    content: String,
}

#[tokio::test]
async fn summary_compaction_archives_real_provider_request_and_raw_response() {
    let mut fixture = Fixture::new(1000, vec![]);
    let transport = Arc::new(SummaryTransport::default());
    let provider: Arc<dyn Provider> = Arc::new(
        OpenAICompatibleProvider::new(
            CompatibleOptions::new("provider", "https://archive.test/v1", Arc::new(ArchiveKey)),
            transport.clone(),
            Arc::new(NoSocket),
        )
        .unwrap(),
    );
    let mut model = Model::new("model", "provider");
    model.context_window = 1000;
    model.max_output_tokens = 1;
    let registry = Arc::new(
        ProviderRegistry::new(
            vec![provider],
            BTreeMap::from([("provider".into(), vec![model])]),
        )
        .unwrap(),
    );
    let aliases = Arc::new(ModelAliasCatalog::new(registry.clone(), []).unwrap());
    let routing = Arc::new(ModelRouting::new(aliases, "provider/model"));
    fixture.selection = ModelRouter::new(registry, routing)
        .resolve("provider/model")
        .unwrap();
    let diagnostics = Arc::new(ArchiveDiagnostics::default());
    let latest_path = fixture.path.join("agent/requests/last_requests.json");
    fixture.sessions = ProviderSessions::with_diagnostics(
        diagnostics.clone(),
        Arc::new(MonotonicDiagnosticClock::default()),
        "agent".into(),
        Some(Arc::new(LastRequestDumper::new(
            &latest_path,
            diagnostics.clone(),
        ))),
    );
    let history = vec![
        Message::user("private old input"),
        Message::assistant("private old answer", vec![]),
        Message::user("retain this"),
    ];
    let result = fixture
        .compact(&compactor(200), &history)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        result.history,
        vec![
            Message::system("S:private archive summary"),
            Message::system("status"),
            history[2].clone(),
        ]
    );
    assert_eq!(result.summary, result.history[0]);
    assert_eq!((result.retained_messages, result.watermark), (1, 2));
    fixture.sessions.close().await.unwrap();

    let latest = fs::read(&latest_path).unwrap();
    {
        let requests = transport.0.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(latest, requests[0].body);
        assert!(requests[0].headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("Authorization") && value == "Bearer private-archive-key"
        }));
    }
    let archived: ArchivedRequest = serde_json::from_slice(
        &fs::read(fixture.path.join("agent/requests/request_1.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(archived.number, 1);
    assert_eq!(archived.transport, "http_sse");
    assert_eq!(
        archived.request,
        Some(ArchivedBody::Utf8(
            String::from_utf8(latest.clone()).unwrap()
        ))
    );
    assert_eq!(
        archived.response,
        vec![ArchivedBody::Utf8(SUMMARY_RESPONSE.into())]
    );
    assert_eq!(archived.outcome, "completed");
    assert_eq!(archived.error_code, None);
    let request: SummaryRequest = serde_json::from_slice(&latest).unwrap();
    assert_eq!(request.model, "model");
    assert_eq!(request.max_tokens, 45);
    assert_eq!(
        request.messages,
        vec![
            SummaryMessage {
                role: "system".into(),
                content: "Sum.".into()
            },
            SummaryMessage {
                role: "user".into(),
                content: "private old input".into()
            },
            SummaryMessage {
                role: "assistant".into(),
                content: "private old answer".into()
            },
        ]
    );
    let events = diagnostics.0.lock().unwrap();
    for operation in ["call_finished", "request_finished"] {
        let terminal = events
            .iter()
            .filter(|event| event.operation == operation)
            .collect::<Vec<_>>();
        assert_eq!(terminal.len(), 1);
        assert_eq!(terminal[0].outcome.as_deref(), Some("completed"));
    }
    let metadata = format!("{events:?}").to_ascii_lowercase();
    for sensitive in [
        "private old input",
        "private old answer",
        "private archive summary",
        "Sum.",
        SUMMARY_RESPONSE,
        "Authorization",
        "Bearer",
        "private-archive-key",
    ] {
        assert!(!metadata.contains(&sensitive.to_ascii_lowercase()));
    }
}

#[tokio::test]
async fn summary_request_uses_actual_templates_profiles_model_and_retained_history() {
    let profiles = ModelProfiles::new(&BTreeMap::from([(
        "provider/model".into(),
        ModelProfileConfig {
            maximum_input_tokens: Some(200),
            summary_output_tokens: Some(77),
            ..ModelProfileConfig::default()
        },
    )]));
    let compactor = configured_compactor(1, "[...]", profiles);
    let mut fixture = Fixture::new(1000, vec![vec![done("actual summary")]]);
    let history = vec![
        Message::user("old input"),
        Message::assistant("old answer", vec![]),
        Message::user("retain this"),
    ];
    let result = fixture
        .compact(&compactor, &history)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        result.history,
        vec![
            Message::system("S:actual summary"),
            Message::system("status"),
            history[2].clone()
        ]
    );
    assert_eq!(result.summary, result.history[0]);
    assert_eq!((result.retained_messages, result.watermark), (1, 2));
    let requests = fixture.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(texts(&requests[0]), ["Sum.", "old input", "old answer"]);
    assert_eq!(requests[0].model, "model");
    assert_eq!(requests[0].max_output_tokens, 77);
    assert!(requests[0].instructions.is_empty() && requests[0].tools.is_empty());
    assert!(!requests[0].include_router_metadata && requests[0].provider_preferences.is_empty());
    assert_eq!(fixture.state.stream_closes.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.state.session_closes.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.state.opens.load(Ordering::SeqCst), 1);
    assert!(fixture.blob_files().is_empty());
    fixture.close().await;
}

#[tokio::test]
async fn progress_counts_one_summary_request_for_many_history_groups() {
    let mut fixture = Fixture::new(1000, vec![vec![done("summary")]]);
    let mut history = vec![Message::user("old input"); 37];
    history.push(Message::user("tail"));
    fixture
        .compact(&compactor(500), &history)
        .await
        .unwrap()
        .unwrap();
    let requests = fixture.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].messages.len(), 38);
    assert_eq!(
        fixture
            .state
            .progress
            .lock()
            .unwrap()
            .iter()
            .map(|progress| (progress.completed_groups, progress.total_groups))
            .collect::<Vec<_>>(),
        [(0, 1), (1, 1)]
    );
    fixture.close().await;
}

#[tokio::test]
async fn chunk_budget_folds_in_order_carrying_only_previous_summary() {
    let compactor = compactor(100);
    let mut fixture = Fixture::new(
        1000,
        vec![
            vec![done("first")],
            vec![done("second")],
            vec![done("third")],
        ],
    );
    let history = vec![
        Message::user("a".repeat(200)),
        Message::user("b".repeat(200)),
        Message::user("c".repeat(200)),
        Message::user("tail"),
    ];
    let result = fixture
        .compact(&compactor, &history)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.summary.content(), "S:third");
    assert_eq!(
        fixture
            .state
            .progress
            .lock()
            .unwrap()
            .iter()
            .map(|progress| (progress.completed_groups, progress.total_groups))
            .collect::<Vec<_>>(),
        [(0, 1), (1, 1), (1, 2), (2, 2), (2, 3), (3, 3)]
    );
    let requests = fixture.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(texts(&requests[0]), ["Sum.", history[0].content()]);
    assert_eq!(
        texts(&requests[1]),
        ["Sum.", "S:first", history[1].content()]
    );
    assert_eq!(
        texts(&requests[2]),
        ["Sum.", "S:second", history[2].content()]
    );
    for request in &requests {
        assert!(
            request_tokens(
                &fixture.selection.canonical_model,
                &request.instructions,
                &request.tools,
                &request.messages,
            )
            .unwrap()
                <= 100
        );
    }
    assert_eq!(fixture.state.stream_closes.load(Ordering::SeqCst), 3);
    fixture.close().await;
}

#[tokio::test]
async fn empty_summary_halves_odd_group_count_without_losing_or_reordering_any_group() {
    let mut fixture = Fixture::new(
        1000,
        vec![
            vec![done("")],
            vec![done("")],
            vec![done("A")],
            vec![done("AB")],
            vec![done("ABC")],
        ],
    );
    let history = [
        Message::user("a"),
        Message::user("b"),
        Message::user("c"),
        Message::user("tail"),
    ];
    let result = fixture
        .compact(&compactor(500), &history)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.summary.content(), "S:ABC");
    assert_eq!(
        fixture
            .state
            .progress
            .lock()
            .unwrap()
            .iter()
            .map(|progress| (progress.completed_groups, progress.total_groups))
            .collect::<Vec<_>>(),
        [
            (0, 1),
            (1, 1),
            (1, 2),
            (2, 2),
            (2, 3),
            (3, 3),
            (3, 4),
            (4, 4),
            (4, 5),
            (5, 5)
        ]
    );
    let requests = fixture.requests();
    assert_eq!(
        requests.iter().map(texts).collect::<Vec<_>>(),
        vec![
            vec!["Sum.", "a", "b", "c"],
            vec!["Sum.", "a", "b"],
            vec!["Sum.", "a"],
            vec!["Sum.", "S:A", "b"],
            vec!["Sum.", "S:AB", "c"],
        ]
    );
    assert_eq!(fixture.state.stream_closes.load(Ordering::SeqCst), 5);
    fixture.close().await;
}

#[tokio::test]
async fn recursive_fold_keeps_assistant_and_all_tool_results_in_one_atomic_group() {
    let mut fixture = Fixture::new(
        1000,
        vec![
            vec![done("")],
            vec![done("tool summary")],
            vec![done("final")],
        ],
    );
    let mut history = tool_group("result");
    history.extend([Message::user("later"), Message::user("tail")]);
    let result = fixture
        .compact(&compactor(500), &history)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.watermark, 3);
    let requests = fixture.requests();
    assert_eq!(requests[1].messages[1..], history[..2]);
    assert_eq!(requests[2].messages[2..], history[2..3]);
    assert_eq!(
        requests[1].messages[1].tool_calls[0].id,
        requests[1].messages[2].tool_call_id
    );
    fixture.close().await;
}

#[tokio::test]
async fn single_empty_or_missing_completion_fails_finitely_without_using_deltas() {
    for (events, expected) in [
        (
            vec![done("")],
            "The compaction provider completed with empty assistant text (finish reason: \"\"; assistant text length: 0 UTF-8 bytes; provider input tokens: 0; provider output tokens: 0). Reasoning delta length: 0 UTF-8 bytes.",
        ),
        (
            vec![Ok(Event::TextDelta("not a completed summary".into()))],
            "The compaction provider stream ended without a Completed event. Reasoning delta length: 0 UTF-8 bytes.",
        ),
        (
            vec![],
            "The compaction provider stream ended without a Completed event. Reasoning delta length: 0 UTF-8 bytes.",
        ),
    ] {
        let mut fixture = Fixture::new(1000, vec![events]);
        let error = fixture
            .compact(
                &compactor(500),
                &[Message::user("a"), Message::user("tail")],
            )
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), expected);
        assert_eq!(fixture.requests().len(), 1);
        assert_eq!(fixture.state.stream_closes.load(Ordering::SeqCst), 1);
        fixture.close().await;
    }
}

#[tokio::test]
async fn empty_completion_reports_metadata_and_reasoning_bytes_without_content() {
    let mut fixture = Fixture::new(
        1000,
        vec![vec![
            Ok(Event::ReasoningDelta {
                text: "private reasoning".into(),
                kind: ReasoningKind::Raw,
                part_id: "raw".into(),
                completed: false,
            }),
            Ok(Event::ReasoningDelta {
                text: "🦜".into(),
                kind: ReasoningKind::Summary,
                part_id: "summary".into(),
                completed: true,
            }
            .with_text("private event text")),
            Ok(Event::Completed(Completion {
                finish_reason: "length".into(),
                input_tokens: 123,
                output_tokens: 456,
                provider_content: "private provider content".into(),
                ..Completion::default()
            })
            .with_text("private completion text")),
        ]],
    );
    let error = fixture
        .compact(
            &compactor(500),
            &[Message::user("a"), Message::user("tail")],
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, parrot_core::ErrorCode::InvalidOperation);
    assert_eq!(
        error.to_string(),
        "The compaction provider completed with empty assistant text (finish reason: \"length\"; assistant text length: 0 UTF-8 bytes; provider input tokens: 123; provider output tokens: 456). Reasoning delta length: 21 UTF-8 bytes."
    );
    assert_eq!(fixture.requests().len(), 1);
    assert_eq!(fixture.state.stream_closes.load(Ordering::SeqCst), 1);
    fixture.close().await;
}

#[tokio::test]
async fn missing_completion_subdivides_successfully_without_using_deltas() {
    let mut fixture = Fixture::new(
        1000,
        vec![
            vec![Ok(Event::TextDelta("not a completed summary".into()))],
            vec![done("A")],
            vec![done("AB")],
        ],
    );
    let result = fixture
        .compact(
            &compactor(500),
            &[
                Message::user("a"),
                Message::user("b"),
                Message::user("tail"),
            ],
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.summary.content(), "S:AB");
    assert_eq!(
        fixture.requests().iter().map(texts).collect::<Vec<_>>(),
        [
            vec!["Sum.", "a", "b"],
            vec!["Sum.", "a"],
            vec!["Sum.", "S:A", "b"],
        ]
    );
    assert_eq!(fixture.state.stream_closes.load(Ordering::SeqCst), 3);
    fixture.close().await;
}

#[tokio::test]
async fn subdivision_failure_reports_the_terminal_attempt() {
    let mut fixture = Fixture::new(
        1000,
        vec![
            vec![Ok(Event::Completed(Completion {
                finish_reason: "length".into(),
                input_tokens: 100,
                output_tokens: 200,
                ..Completion::default()
            }))],
            vec![Ok(Event::Completed(Completion {
                finish_reason: "stop".into(),
                input_tokens: 10,
                output_tokens: 20,
                ..Completion::default()
            }))],
        ],
    );
    let error = fixture
        .compact(
            &compactor(500),
            &[
                Message::user("a"),
                Message::user("b"),
                Message::user("tail"),
            ],
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "The compaction provider completed with empty assistant text (finish reason: \"stop\"; assistant text length: 0 UTF-8 bytes; provider input tokens: 10; provider output tokens: 20). Reasoning delta length: 0 UTF-8 bytes."
    );
    assert_eq!(fixture.requests().len(), 2);
    assert_eq!(fixture.state.stream_closes.load(Ordering::SeqCst), 2);
    fixture.close().await;
}

#[tokio::test]
async fn every_event_after_nonempty_completion_fails_and_closes_stream() {
    for after in [
        done("duplicate"),
        retry(),
        Ok(Event::TextDelta("late".into())),
        Ok(Event::HttpResponseHeadersReceived),
    ] {
        let mut fixture = Fixture::new(1000, vec![vec![done("summary"), after]]);
        let error = fixture
            .compact(
                &compactor(500),
                &[Message::user("a"), Message::user("tail")],
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "The compaction provider emitted an event after completion."
        );
        assert_eq!(fixture.state.stream_closes.load(Ordering::SeqCst), 1);
        assert!(!fixture.state.audit.lock().unwrap().contains(&"retry-start"));
        fixture.close().await;
    }
}

#[tokio::test]
async fn empty_completion_is_not_a_nonempty_completion_barrier_and_whitespace_is_a_summary() {
    let mut fixture = Fixture::new(1000, vec![vec![done(""), retry(), done(" ")]]);
    let result = fixture
        .compact(
            &compactor(500),
            &[Message::user("a"), Message::user("tail")],
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.summary.content(), "S: ");
    assert_eq!(
        *fixture.state.audit.lock().unwrap(),
        [
            "next",
            "next",
            "retry-start",
            "retry-finish",
            "next",
            "next",
            "close"
        ]
    );
    fixture.close().await;
}

#[tokio::test]
async fn retry_observer_is_awaited_and_observer_failure_closes_stream() {
    let mut fixture = Fixture::new(1000, vec![vec![retry(), done("unread")]]);
    fixture.observer.failure = Some(Error::Io("publication failed".into()));
    let error = fixture
        .compact(
            &compactor(500),
            &[Message::user("a"), Message::user("tail")],
        )
        .await
        .unwrap_err();
    assert!(error == core_error(Error::Io("publication failed".into())));
    assert_eq!(
        *fixture.state.audit.lock().unwrap(),
        ["next", "retry-start", "retry-finish", "close"]
    );
    fixture.close().await;
}

#[tokio::test]
async fn cancellation_before_call_and_during_retry_is_joined_without_more_events() {
    let mut fixture = Fixture::new(1000, vec![]);
    fixture.token.0.store(true, Ordering::SeqCst);
    let history = [Message::user("a"), Message::user("tail")];
    assert!(matches!(
        fixture.compact(&compactor(500), &history).await,
        Err(error) if error.kind == parrot_core::ErrorKind::Cancelled
    ));
    assert!(fixture.requests().is_empty());
    fixture.close().await;
    let mut fixture = Fixture::new(1000, vec![vec![retry(), done("not reached")]]);
    fixture.observer.cancel = Some(fixture.token.clone());
    assert!(matches!(
        fixture.compact(&compactor(500), &history).await,
        Err(error) if error.kind == parrot_core::ErrorKind::Cancelled
    ));
    assert_eq!(fixture.state.stream_closes.load(Ordering::SeqCst), 1);
    assert_eq!(
        *fixture.state.audit.lock().unwrap(),
        ["next", "retry-start", "retry-finish", "next", "close"]
    );
    fixture.close().await;
}

#[tokio::test]
async fn stream_failure_is_preserved_and_disposal_failure_is_not_silenced() {
    let history = [Message::user("a"), Message::user("tail")];
    let mut fixture = Fixture::new(1000, vec![vec![Err(Error::Io("stream failure".into()))]]);
    let error = fixture
        .compact(&compactor(500), &history)
        .await
        .unwrap_err();
    assert!(error == core_error(Error::Io("stream failure".into())));
    assert_eq!(
        fixture
            .state
            .progress
            .lock()
            .unwrap()
            .iter()
            .map(|progress| (progress.completed_groups, progress.total_groups))
            .collect::<Vec<_>>(),
        [(0, 1)]
    );
    assert_eq!(fixture.state.stream_closes.load(Ordering::SeqCst), 1);
    fixture.close().await;
    let mut fixture = Fixture::new(1000, vec![vec![done("ok")]]);
    *fixture.state.close_failure.lock().unwrap() = Some(Error::Io("dispose failure".into()));
    let error = fixture
        .compact(&compactor(500), &history)
        .await
        .unwrap_err();
    assert!(error == core_error(Error::Io("dispose failure".into())));
    fixture.close().await;
}

#[tokio::test]
async fn oversized_tool_group_truncates_summary_input_without_mutating_source_or_writing_files() {
    let mut fixture = Fixture::new(1000, vec![vec![done("truncated summary")]]);
    let mut history = tool_group(&"result-🦜".repeat(1000));
    let image = fixture.path.join("missing-image.bin");
    let text = history[1].content().to_owned();
    history[1].set_contents(vec![
        Content::text(text),
        Content::image_file(image.to_str().unwrap(), "image/test", 1, 1),
    ]);
    history.push(Message::user("tail"));
    let original = history.clone();
    let result = fixture
        .compact(&compactor(500), &history)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(history, original);
    assert!(fixture.blob_files().is_empty());
    assert!(!image.exists());
    let requests = fixture.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.messages[0].tool_calls, history[0].tool_calls);
    assert_eq!(request.messages[1].role, Role::Tool);
    assert_eq!(request.messages[1].tool_call_id, "call");
    let clipped = request.messages[1].content();
    assert!(clipped.ends_with("[...]"));
    assert!(
        history[1]
            .content()
            .starts_with(clipped.trim_end_matches("[...]"))
    );
    assert!(clipped.len() < history[1].content().len());
    assert!(
        request
            .messages
            .iter()
            .flat_map(Message::contents)
            .all(|content| !matches!(content, Content::Image { .. }))
    );
    assert!(
        request_tokens(
            &fixture.selection.canonical_model,
            &request.instructions,
            &request.tools,
            &request.messages
        )
        .unwrap()
            <= 500
    );
    assert_eq!((result.watermark, result.retained_messages), (2, 1));
    assert_eq!(result.history[2], history[2]);
    fixture.close().await;
}

#[tokio::test]
async fn truncation_notice_that_cannot_fit_fails_before_provider_call_without_writing_files() {
    let mut fixture = Fixture::new(1000, vec![]);
    let mut history = tool_group(&"x".repeat(4000));
    history.push(Message::user("tail"));
    let original = history.clone();
    let error = fixture
        .compact(
            &configured_compactor(
                100,
                &"notice".repeat(100),
                ModelProfiles::new(&BTreeMap::new()),
            ),
            &history,
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.detail,
        Some(parrot_core::ErrorDetail::Compaction(
            parrot_core::CompactionFailure::GroupExceedsBudget
        ))
    );
    assert_eq!(history, original);
    assert!(fixture.blob_files().is_empty());
    assert!(fixture.requests().is_empty());
    fixture.close().await;
}

#[tokio::test]
async fn oversized_tool_and_user_groups_are_bounded_without_mutating_source_or_writing_files() {
    let mut fixture = Fixture::new(1000, vec![vec![done("first")], vec![done("final")]]);
    let mut history = tool_group(&"x".repeat(4000));
    history.extend([Message::user("🦜".repeat(4000)), Message::user("tail")]);
    let original = history.clone();
    let result = fixture
        .compact(&compactor(500), &history)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.summary.content(), "S:final");
    assert_eq!(history, original);
    assert!(fixture.blob_files().is_empty());
    let requests = fixture.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].messages[0].tool_calls, history[0].tool_calls);
    assert_eq!(
        requests[0].messages[1].tool_call_id,
        history[1].tool_call_id
    );
    let clipped_user = requests[1]
        .messages
        .iter()
        .find(|message| message.role == Role::User)
        .unwrap()
        .content();
    assert!(clipped_user.ends_with("[...]"));
    assert!(
        history[2]
            .content()
            .starts_with(clipped_user.trim_end_matches("[...]"))
    );
    assert!(requests.iter().all(|request| {
        request_tokens(
            &fixture.selection.canonical_model,
            &request.instructions,
            &request.tools,
            &request.messages,
        )
        .unwrap()
            <= 500
    }));
    fixture.close().await;
}

#[tokio::test]
async fn preceding_summary_can_force_later_tool_summary_input_to_be_truncated() {
    let mut fixture = Fixture::new(
        1000,
        vec![vec![done(&"s".repeat(240))], vec![done("final")]],
    );
    let mut history = vec![Message::user("x".repeat(320))];
    history.extend(tool_group(&"y".repeat(240)));
    history.push(Message::user("tail"));
    let original = history.clone();
    for group in [&history[..1], &history[1..3]] {
        let messages = group
            .iter()
            .cloned()
            .chain([Message::system("Sum.")])
            .collect::<Vec<_>>();
        assert!(
            request_tokens(&fixture.selection.canonical_model, "", &[], &messages).unwrap() <= 140
        );
    }
    let result = fixture
        .compact(&compactor(140), &history)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.summary.content(), "S:final");
    assert_eq!(history, original);
    assert!(fixture.blob_files().is_empty());
    let requests = fixture.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].messages[0].content(),
        format!("S:{}", "s".repeat(240))
    );
    assert_eq!(requests[1].messages[1].tool_calls, history[1].tool_calls);
    assert_eq!(
        requests[1].messages[2].tool_call_id,
        history[2].tool_call_id
    );
    let clipped = requests[1].messages[2].content();
    assert!(clipped.ends_with("[...]"));
    assert!(
        history[2]
            .content()
            .starts_with(clipped.trim_end_matches("[...]"))
    );
    assert!(requests.iter().all(|request| {
        request_tokens(
            &fixture.selection.canonical_model,
            &request.instructions,
            &request.tools,
            &request.messages,
        )
        .unwrap()
            <= 140
    }));
    fixture.close().await;
}

#[tokio::test]
async fn irreducible_tool_metadata_fails_without_rewriting_arguments_or_publishing_files() {
    let mut fixture = Fixture::new(1000, vec![]);
    let mut history = tool_group("result");
    history[0].tool_calls[0].arguments_json = serde_json::to_string(&"🦜".repeat(1000)).unwrap();
    history.push(Message::user("tail"));
    let original = history.clone();
    let error = fixture
        .compact(&compactor(100), &history)
        .await
        .unwrap_err();
    assert_eq!(
        error.detail,
        Some(parrot_core::ErrorDetail::Compaction(
            parrot_core::CompactionFailure::GroupExceedsBudget
        ))
    );
    assert_eq!(history, original);
    assert!(fixture.requests().is_empty());
    assert!(fixture.blob_files().is_empty());
    fixture.close().await;
}

#[tokio::test]
async fn summary_and_fixed_retained_history_are_checked_against_final_selected_input_limit() {
    let mut fixture = Fixture::new(100, vec![vec![done(&"s".repeat(500))]]);
    let error = fixture
        .compact(
            &compactor(100),
            &[Message::user("old"), Message::user("tail")],
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "The compacted conversation exceeds the selected model input limit."
    );
    assert_eq!(fixture.requests().len(), 1);
    assert_eq!(fixture.state.stream_closes.load(Ordering::SeqCst), 1);
    fixture.close().await;
}

#[tokio::test]
async fn provider_response_body_stays_concrete_and_does_not_traverse_wrapped_causes() {
    use parrot_provider::http_stream::{HttpError, HttpFailure};
    let failure = Error::Http(HttpError::Response(HttpFailure {
        retry_after: None,
        status: 413,
        error_type: "context".into(),
        error_code: "too_large".into(),
        detail: "synthetic failure".into(),
        response_body: "{\"private\":\"synthetic response\"}".into(),
    }));
    for (provider_error, expected_body) in [
        (failure.clone(), "{\"private\":\"synthetic response\"}"),
        (
            Error::ProviderWithCause {
                message: "wrapped".into(),
                cause: Box::new(failure),
            },
            "",
        ),
        (Error::Io("ordinary failure".into()), ""),
        (
            Error::Http(HttpError::Transport("provider stream idle timeout".into())),
            "",
        ),
    ] {
        let mut fixture = Fixture::new(1000, vec![vec![Err(provider_error.clone())]]);
        let error = fixture
            .compact(
                &compactor(500),
                &[Message::user("old"), Message::user("tail")],
            )
            .await
            .unwrap_err();
        assert_eq!(error.response_body, expected_body);
        assert_eq!(error, core_error(provider_error));
        assert_eq!(fixture.state.stream_closes.load(Ordering::SeqCst), 1);
        fixture.close().await;
    }
}

#[tokio::test]
async fn cancelled_compaction_stops_before_provider_call_and_oversized_images_are_omitted_only_from_summary_input()
 {
    let mut fixture = Fixture::new(1000, vec![]);
    let mut history = tool_group(&"oversized".repeat(1000));
    history.push(Message::user("tail"));
    fixture.token.0.store(true, Ordering::SeqCst);
    let error = fixture
        .compact(&compactor(500), &history)
        .await
        .unwrap_err();
    assert_eq!(error.kind, parrot_core::ErrorKind::Cancelled);
    assert!(fixture.blob_files().is_empty() && fixture.requests().is_empty());
    fixture.close().await;

    let mut fixture = Fixture::new(1000, vec![vec![done("truncated summary")]]);
    let image = fixture.path.join("missing.png");
    history[1].set_contents(vec![Content::image_file(
        image.to_str().unwrap(),
        "image/png",
        2,
        2,
    )]);
    let original = history.clone();
    fixture
        .compact(&compactor(500), &history)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(history, original);
    assert!(fixture.blob_files().is_empty());
    let requests = fixture.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].messages[1].tool_call_id, "call");
    assert_eq!(requests[0].messages[1].content(), "[...]");
    assert!(!image.exists());
    fixture.close().await;
}

#[tokio::test]
async fn complete_tool_results_stay_together_and_incomplete_calls_prevent_compaction() {
    let history = [
        Message::assistant(
            "",
            vec![
                ToolCall {
                    id: "a".into(),
                    name: "read".into(),
                    arguments_json: "{}".into(),
                },
                ToolCall {
                    id: "b".into(),
                    name: "read".into(),
                    arguments_json: "{}".into(),
                },
            ],
        ),
        Message::tool_result("b", "second"),
        Message::tool_result("a", "first"),
        Message::user("next"),
        Message::tool_result("orphan", "later"),
    ];
    let mut fixture = Fixture::new(1000, vec![vec![done("summary")]]);
    let result = fixture
        .compact(&compactor(500), &history)
        .await
        .unwrap()
        .unwrap();
    let requests = fixture.requests();
    assert_eq!(&requests[0].messages[..3], &history[..3]);
    assert_eq!(result.history.last().unwrap(), &history[4]);
    assert_eq!(result.watermark, 4);
    fixture.close().await;
    let mut fixture = Fixture::new(1000, vec![]);
    assert!(
        fixture
            .compact(&compactor(500), &history[..2])
            .await
            .unwrap()
            .is_none()
    );
    assert!(fixture.requests().is_empty());
    fixture.close().await;
}

#[tokio::test]
async fn retained_native_replay_reaches_the_next_provider_request_unchanged() {
    let mut tail = Message::assistant("", vec![]);
    tail.provider_id = "provider".into();
    tail.provider_content = "opaque continuation bytes".into();
    let history = [Message::user("old input"), tail.clone()];
    let mut fixture = Fixture::new(1000, vec![vec![done("summary")], vec![done("answer")]]);
    let result = fixture
        .compact(&compactor(500), &history)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.history.last().unwrap(), &tail);
    let session = fixture
        .sessions
        .get(fixture.selection.canonical_model.provider.clone())
        .await
        .unwrap();
    let mut stream = session
        .call(Request {
            messages: result.history,
            ..Request::default()
        })
        .unwrap();
    while stream.next(fixture.token.as_ref()).await.unwrap().is_some() {}
    stream.close().await.unwrap();
    let requests = fixture.requests();
    assert_eq!(requests[1].messages.last().unwrap(), &tail);
    fixture.close().await;
}

#[tokio::test]
async fn no_safe_progress_does_not_open_provider_or_write_group_files() {
    for history in [
        vec![],
        vec![Message::user("only")],
        vec![
            Message::assistant(
                "",
                vec![ToolCall {
                    id: "missing".into(),
                    ..ToolCall::default()
                }],
            ),
            Message::user("tail"),
        ],
    ] {
        let mut fixture = Fixture::new(1000, vec![]);
        assert!(
            fixture
                .compact(&compactor(500), &history)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(fixture.state.opens.load(Ordering::SeqCst), 0);
        assert!(fixture.blob_files().is_empty());
        fixture.close().await;
    }
}
