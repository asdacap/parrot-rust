use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation},
        filenames::FilenameGenerator,
        paths::{RootOrigin, SelectedSessionRoot},
    },
    values::config::{ModelProfileConfig, ModelProfiles},
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_provider::{
    catalogs::Model,
    routing::{
        aliases::{ModelAliasCatalog, ModelRouter, ModelRouting, ResolvedModelSelection},
        registry::ProviderRegistry,
    },
    sessions::{
        Completion, Content, Error, Event, EventStream, Message, Provider, ProviderSessions,
        Request, Role, Session, ToolCall,
    },
};
use parrot_runtime::compaction::{
    CompactionError, CompactionResult, CompactionSettings, Compactor, PlanningError, PlanningInput,
    RetryObserver, group_messages,
};
use parrot_storage::artifacts::compaction::CompactionGroupBlobStore;
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
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        FakeSession(self.0.clone()).call(request)
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
impl RetryObserver for Observer {
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
}
struct Names(AtomicUsize);
impl FilenameGenerator for Names {
    fn next_filename(&self) -> String {
        format!("group-{}-arse.dat", self.0.fetch_add(1, Ordering::SeqCst))
    }
}
struct Fixture {
    path: PathBuf,
    blobs: CompactionGroupBlobStore,
    state: Arc<State>,
    selection: ResolvedModelSelection,
    sessions: ProviderSessions,
    token: Arc<Token>,
    observer: Observer,
}
impl Fixture {
    fn new(limit: i32, scripts: Vec<Vec<Result<Event, Error>>>) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::current_dir().unwrap().join(".tmp").join(format!(
            "t45-execution-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(path.join("agent/blobs")).unwrap();
        let root = SelectedSessionRoot::new(path.clone(), RootOrigin::Current).unwrap();
        let blobs = CompactionGroupBlobStore::for_session(
            &root,
            "agent",
            Arc::new(Names(AtomicUsize::new(0))),
        )
        .unwrap();
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
            blobs,
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
    ) -> Result<Option<CompactionResult>, CompactionError> {
        let groups = group_messages(history);
        compactor
            .compact(
                &self.selection,
                None,
                &PlanningInput {
                    instructions: "instructions",
                    tools: &[],
                    groups: &groups,
                    base_watermark: 0,
                    fixed_message: &Message::system("status"),
                },
                &mut self.blobs,
                &self.sessions,
                &self.observer,
                self.token.as_ref(),
            )
            .await
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
    configured_compactor(input_budget, "{path}", ModelProfiles::new(&BTreeMap::new()))
}
fn configured_compactor(input_budget: i32, notice: &str, profiles: ModelProfiles) -> Compactor {
    let yaml = format!(
        "prompt_templates:\n  compaction.summary-instructions:\n    template: 'Sum.'\n  compaction.summary-prefix:\n    template: 'S:'\n  compaction.oversized-tool-group-notice:\n    template: '{notice}'\n"
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
    let compactor = configured_compactor(1, "{path}", profiles);
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
            parrot_runtime::compaction::CompactionPlanner::estimate_tokens(
                &fixture.selection.canonical_model,
                &request.messages
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
    assert_eq!(result.watermark, 2);
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
    for events in [
        vec![done("")],
        vec![Ok(Event::TextDelta("not a completed summary".into()))],
        vec![],
    ] {
        let mut fixture = Fixture::new(1000, vec![events]);
        let error = fixture
            .compact(
                &compactor(500),
                &[Message::user("a"), Message::user("tail")],
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "The compaction provider did not complete with a summary."
        );
        assert_eq!(fixture.requests().len(), 1);
        assert_eq!(fixture.state.stream_closes.load(Ordering::SeqCst), 1);
        fixture.close().await;
    }
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
    assert!(
        matches!(error, CompactionError::Provider(Error::Io(ref text)) if text == "publication failed")
    );
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
        Err(CompactionError::Cancelled)
    ));
    assert!(fixture.requests().is_empty());
    fixture.close().await;
    let mut fixture = Fixture::new(1000, vec![vec![retry(), done("not reached")]]);
    fixture.observer.cancel = Some(fixture.token.clone());
    assert!(matches!(
        fixture.compact(&compactor(500), &history).await,
        Err(CompactionError::Provider(Error::Cancelled))
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
    assert!(
        matches!(error, CompactionError::Provider(Error::Io(ref text)) if text == "stream failure")
    );
    assert_eq!(fixture.state.stream_closes.load(Ordering::SeqCst), 1);
    fixture.close().await;
    let mut fixture = Fixture::new(1000, vec![vec![done("ok")]]);
    *fixture.state.close_failure.lock().unwrap() = Some(Error::Io("dispose failure".into()));
    let error = fixture
        .compact(&compactor(500), &history)
        .await
        .unwrap_err();
    assert!(
        matches!(error, CompactionError::Provider(Error::Io(ref text)) if text == "dispose failure")
    );
    fixture.close().await;
}

#[tokio::test]
async fn actual_oversized_tool_group_persists_original_bytes_and_real_notice_path() {
    let mut fixture = Fixture::new(1000, vec![vec![done("spilled summary")]]);
    let mut history = tool_group(&"result-🦜".repeat(1000));
    let image = fixture.path.join("image.bin");
    fs::write(&image, [0_u8, 1, 255]).unwrap();
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
    let files = fixture.blob_files();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].file_name().unwrap(), "group-0.json");
    let artifact: serde_json::Value =
        serde_json::from_slice(&fs::read(&files[0]).unwrap()).unwrap();
    assert_eq!(artifact["messages"][0]["role"], "Assistant");
    assert_eq!(
        artifact["messages"][0]["tool_calls"][0]["arguments_json"],
        "{\"x\":1}"
    );
    assert_eq!(artifact["messages"][1]["tool_call_id"], "call");
    assert_eq!(
        artifact["messages"][1]["contents"][0]["text"],
        history[1].content()
    );
    assert_eq!(artifact["messages"][1]["contents"][1]["image"], "AAH/");
    assert_eq!(
        artifact["messages"][1]["contents"][1]["media_type"],
        "image/test"
    );
    let requests = fixture.requests();
    assert_eq!(texts(&requests[0]), ["Sum.", files[0].to_str().unwrap()]);
    assert_eq!(requests[0].messages[1].role, Role::System);
    assert_eq!((result.watermark, result.retained_messages), (1, 1));
    assert_eq!(result.history[2], history[2]);
    fixture.close().await;
}

#[tokio::test]
async fn real_path_notice_that_does_not_fit_fails_after_blob_publication_before_provider_call() {
    let mut fixture = Fixture::new(1000, vec![]);
    let mut history = tool_group(&"x".repeat(4000));
    history.push(Message::user("tail"));
    let error = fixture
        .compact(
            &configured_compactor(
                100,
                &format!("{}{{path}}", "notice".repeat(100)),
                ModelProfiles::new(&BTreeMap::new()),
            ),
            &history,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        CompactionError::Planning(PlanningError::GroupExceedsBudget)
    ));
    assert_eq!(fixture.blob_files().len(), 1);
    assert!(fixture.requests().is_empty());
    fixture.close().await;
}

#[tokio::test]
async fn earlier_eligible_spill_is_persisted_before_later_ineligible_group_is_rejected() {
    let mut fixture = Fixture::new(1000, vec![]);
    let mut history = tool_group(&"x".repeat(4000));
    history.extend([Message::user("y".repeat(4000)), Message::user("tail")]);
    let error = fixture
        .compact(&compactor(500), &history)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        CompactionError::Planning(PlanningError::GroupExceedsBudget)
    ));
    let files = fixture.blob_files();
    assert_eq!(files.len(), 1);
    let artifact: serde_json::Value =
        serde_json::from_slice(&fs::read(&files[0]).unwrap()).unwrap();
    assert_eq!(
        artifact["messages"][1]["contents"][0]["text"],
        "x".repeat(4000)
    );
    assert!(fixture.requests().is_empty());
    fixture.close().await;
}

#[tokio::test]
async fn preceding_summary_can_force_a_later_tool_group_to_spill_once() {
    let mut fixture = Fixture::new(
        1000,
        vec![vec![done(&"s".repeat(240))], vec![done("final")]],
    );
    let mut history = vec![Message::user("x".repeat(320))];
    history.extend(tool_group(&"y".repeat(240)));
    history.push(Message::user("tail"));
    // Each raw group fits alone. First + second do not; the resulting summary
    // makes the second raw group too large, while its actual path notice fits.
    let compactor = compactor(140);
    let raw = group_messages(&history);
    assert!(raw[..2].iter().all(|group| {
        compactor
            .planner()
            .estimate_summary_request_tokens(
                &fixture.selection.canonical_model,
                "",
                0,
                group.messages,
            )
            .unwrap()
            <= 140
    }));
    let result = fixture
        .compact(&compactor, &history)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.summary.content(), "S:final");
    let files = fixture.blob_files();
    assert_eq!(files.len(), 1);
    let requests = fixture.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        texts(&requests[1]),
        [
            "Sum.",
            &format!("S:{}", "s".repeat(240)),
            files[0].to_str().unwrap()
        ]
    );
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
    ] {
        let mut fixture = Fixture::new(1000, vec![vec![Err(provider_error.clone())]]);
        let error = fixture
            .compact(
                &compactor(500),
                &[Message::user("old"), Message::user("tail")],
            )
            .await
            .unwrap_err();
        assert_eq!(error.response_body(), expected_body);
        assert!(matches!(error, CompactionError::Provider(actual) if actual == provider_error));
        assert_eq!(fixture.state.stream_closes.load(Ordering::SeqCst), 1);
        fixture.close().await;
    }
}

#[tokio::test]
async fn cancelled_blob_and_missing_file_image_keep_storage_failure_without_provider_call() {
    use parrot_storage::artifacts::compaction::CompactionErrorKind;
    let mut fixture = Fixture::new(1000, vec![]);
    let mut history = tool_group(&"oversized".repeat(1000));
    history.push(Message::user("tail"));
    fixture.token.0.store(true, Ordering::SeqCst);
    let error = fixture
        .compact(&compactor(500), &history)
        .await
        .unwrap_err();
    assert!(
        matches!(error, CompactionError::Blob(error) if error.kind == CompactionErrorKind::Cancelled)
    );
    assert!(fixture.blob_files().is_empty() && fixture.requests().is_empty());
    fixture.close().await;

    let mut fixture = Fixture::new(1000, vec![]);
    history[1].set_contents(vec![Content::image_file(
        fixture.path.join("missing.png").to_str().unwrap(),
        "image/png",
        2,
        2,
    )]);
    let error = fixture
        .compact(&compactor(500), &history)
        .await
        .unwrap_err();
    assert!(matches!(error, CompactionError::Blob(error) if error.kind == CompactionErrorKind::Io));
    assert!(fixture.blob_files().is_empty() && fixture.requests().is_empty());
    fixture.close().await;
}

#[tokio::test]
async fn no_safe_progress_does_not_open_provider_or_write_blob() {
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
