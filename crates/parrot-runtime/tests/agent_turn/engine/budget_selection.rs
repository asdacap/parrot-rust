//! Actual drain requests for the source token-budget and captured-profile predicates.
//! Settlement here is the existing injected fixture, not the production T47 bridge.
use super::*;

struct BudgetTools {
    inner: Tools,
    definitions: Vec<sessions::Tool>,
    materializations: AtomicUsize,
}
impl BudgetTools {
    fn new(names: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            inner: Tools::default(),
            definitions: names
                .iter()
                .map(|name| sessions::Tool {
                    name: (*name).into(),
                    description: String::new(),
                    parameters_json: "{}".into(),
                })
                .collect(),
            materializations: AtomicUsize::new(0),
        })
    }
}
impl TurnTools<Security> for BudgetTools {
    type Snapshot = Vec<sessions::Tool>;
    fn materialize(&self) -> Result<Self::Snapshot, TurnError> {
        self.materializations.fetch_add(1, Ordering::SeqCst);
        Ok(self.definitions.clone())
    }
    fn permitted(
        &self,
        snapshot: &Self::Snapshot,
        selection: &TurnSelection<Security>,
    ) -> Result<Self::Snapshot, TurnError> {
        self.inner.permitted(snapshot, selection)
    }
    fn definitions<'a>(&self, snapshot: &'a Self::Snapshot) -> &'a [sessions::Tool] {
        snapshot
    }
    fn enabled_after_interruption(&self, snapshot: &Self::Snapshot) -> Self::Snapshot {
        snapshot
            .iter()
            .filter(|tool| tool.name == "survivor")
            .cloned()
            .collect()
    }
    fn reconcile<'a>(
        &'a self,
        resources: TurnResources<'a>,
        selection: &'a TurnSelection<Security>,
        snapshot: &'a Self::Snapshot,
        capture: &'a dyn SelectionSource<Security>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), TurnError>> {
        self.inner
            .reconcile(resources, selection, snapshot, capture, cancellation)
    }
}
struct BudgetRuntime {
    session: AgentSession<RuntimeBackend<Security, BudgetTools>>,
    selection: Arc<Selection>,
    activity: Arc<Activity>,
}
impl BudgetRuntime {
    async fn new(
        fixture: &Fixture,
        provider: &Scripted,
        context_window: i32,
        maximum_input_tokens: i32,
        maximum_output_tokens: i32,
        profile: TurnProfile,
        tools: Arc<BudgetTools>,
    ) -> Self {
        let mut model = Model::new("model", "scripted");
        model.context_window = context_window;
        model.max_input_tokens = maximum_input_tokens;
        model.max_output_tokens = maximum_output_tokens;
        let registry = Arc::new(
            ProviderRegistry::new(
                vec![Arc::new(provider.clone())],
                BTreeMap::from([("scripted".into(), vec![model])]),
            )
            .unwrap(),
        );
        let aliases = Arc::new(ModelAliasCatalog::new(registry.clone(), vec![]).unwrap());
        let selection = Arc::new(Selection(Mutex::new(TurnSelection {
            model: ModelRouter::new(
                registry,
                Arc::new(ModelRouting::new(aliases, "scripted/model")),
            )
            .resolve("scripted/model")
            .unwrap(),
            profile,
            security: Arc::new(Security),
        })));
        let trace = tools.inner.trace.clone();
        let broker = Arc::new(EventBroker::new());
        let publisher: Arc<RuntimePublisher> = Arc::new(Publisher {
            history: fixture.history.clone(),
            broker: broker.clone(),
            terminal_failure: 0,
            cancel_retry: AtomicBool::new(false),
            terminals: AtomicUsize::new(0),
            terminal_observed: AtomicBool::new(false),
            reminder_gate: Mutex::new(None),
            trace: trace.clone(),
        });
        let templates = templates();
        let reminders = Arc::new(
            ExitReminders::new(
                fixture.history.clone(),
                publisher.clone(),
                templates.clone(),
            )
            .await
            .unwrap(),
        );
        let activity = Arc::new(Activity::new(trace.clone()));
        let compactor = Compactor::new(
            CompactionSettings {
                trigger_percent: i32::MAX,
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
            fixture.repository.events().database().selected_root(),
            "scratch",
            Arc::new(Names),
        )
        .unwrap();
        let context =
            ContextController::new(compactor, fixture.history.clone(), blobs, templates.clone());
        let backend = RuntimeBackend::new(RuntimeConfig {
            agent_limits: parrot_foundation::config::runtime::AgentLimitsConfig {
                disable_max_turns: false,
                ..Default::default()
            },
            identity: SessionIdentity {
                session_id: "agent".into(),
                name: "budget-test".into(),
                parent_session_id: String::new(),
                depth: 1,
            },
            history: fixture.history.clone(),
            broker,
            publisher,
            selection: selection.clone(),
            tools,
            prompt: Box::new(Prompt {
                trace: trace.clone(),
                sampled: false,
                cancel_sample: false,
            }),
            skills: Arc::new(Skills(trace.clone())),
            context,
            status: Arc::new(Status(trace.clone())),
            completions: vec![],
            templates,
            reminders,
            output: Arc::new(Output::default()),
            activity: activity.clone(),
            parent_completion: Arc::new(Parent(trace)),
            ancestors: vec![],
            event_id: Arc::new(id),
        })
        .await
        .unwrap();
        Self {
            session: AgentSession::new(backend, Arc::new(Signal::new())),
            selection,
            activity,
        }
    }
    async fn send(
        &self,
        text: &str,
        delivery: protocol::Delivery,
    ) -> parrot_runtime::agent_turn::lifecycle::SendResult {
        bounded(self.session.send(
            vec![ConversationPart::text(text)],
            id(),
            delivery,
            IncomingActivity {
                name: "budget-test".into(),
                cause: None,
                source: protocol::InputSource::default(),
            },
            Arc::new(NeverCancelled),
        ))
        .await
        .unwrap()
    }
    async fn close(&self) {
        bounded(self.session.close()).await.unwrap();
    }
}
fn profile() -> TurnProfile {
    TurnProfile {
        id: "original".into(),
        maximum_requests: 8,
        allowed_tools: None,
        disabled_tools: vec![],
    }
}
fn tool_step(name: &str) -> Step {
    let mut step = Step::complete("");
    step.completion.as_mut().unwrap().tool_calls = vec![sessions::ToolCall {
        id: id(),
        name: name.into(),
        arguments_json: "{}".into(),
    }];
    step
}
fn tool_names(request: &Request) -> Vec<&str> {
    request
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect()
}

#[tokio::test]
async fn actual_requests_reserve_source_headroom_for_unknown_and_known_context_windows() {
    for (context_window, output_limit) in [(0, 0), (100_000, 0), (20_000, 8_192)] {
        let fixture = Fixture::new();
        let provider = Scripted::new(vec![Step::complete("done")]);
        let runtime = BudgetRuntime::new(
            &fixture,
            &provider,
            context_window,
            0,
            output_limit,
            profile(),
            BudgetTools::new(&[]),
        )
        .await;
        let sent = runtime
            .send("prompt", protocol::Delivery::DELIVERY_STEER)
            .await;
        assert_eq!(
            bounded(sent.execution.join()).await.unwrap().status,
            ExecutionStatus::Succeeded
        );
        {
            let requests = provider.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            let request = &requests[0];
            let selected = runtime.selection.capture().unwrap();
            let estimated = CompactionPlanner::estimate_input_tokens(
                &selected.model.canonical_model,
                &request.instructions,
                &request.tools,
                &request.messages,
            )
            .unwrap();
            let headroom = (estimated + 19) / 20;
            let requested = if output_limit > 0 {
                32_768_i64.min(i64::from(output_limit))
            } else {
                32_768
            };
            let expected = if context_window > 0 {
                requested.min(i64::from(context_window) - estimated - headroom)
            } else {
                requested
            };
            assert!(expected > 0);
            assert_eq!(i64::from(request.max_output_tokens), expected);
            if context_window > 0 {
                assert!(
                    estimated + i64::from(request.max_output_tokens) <= i64::from(context_window)
                );
            }
        }
        runtime.close().await;
    }
}

#[tokio::test]
async fn actual_reported_usage_triggers_compaction_before_consuming_reserved_output() {
    let fixture = Fixture::new();
    let mut first = Step::complete("first answer");
    first.completion.as_mut().unwrap().input_tokens = 147_776;
    let provider = Scripted::new(vec![
        first,
        Step::complete("compacted summary"),
        Step::complete("done"),
    ]);
    let runtime = BudgetRuntime::new(
        &fixture,
        &provider,
        180_000,
        0,
        0,
        profile(),
        BudgetTools::new(&[]),
    )
    .await;
    for prompt in ["first prompt", "second prompt"] {
        let sent = runtime
            .send(prompt, protocol::Delivery::DELIVERY_STEER)
            .await;
        assert_eq!(
            bounded(sent.execution.join()).await.unwrap().status,
            ExecutionStatus::Succeeded
        );
    }
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[1].max_output_tokens, 1024);
        let final_request = &requests[2];
        assert_eq!(final_request.max_output_tokens, 32_768);
        let selected = runtime.selection.capture().unwrap();
        let snapshot = fixture
            .controller()
            .compactor()
            .planner()
            .estimate_context(
                &selected.model.canonical_model,
                &final_request.instructions,
                &final_request.tools,
                &final_request.messages,
            )
            .unwrap();
        assert_eq!(snapshot.trigger_tokens, Some(140_220));
        assert!(!snapshot.exceeds_trigger());
    }
    let events = fixture.events().await;
    let usage: Vec<_> = events
        .iter()
        .filter(|event| event.has_request_usage_recorded())
        .collect();
    assert_eq!(usage.len(), 2);
    assert_eq!(usage[0].request_usage_recorded().input_tokens, 147_776);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_compaction_finished())
            .count(),
        1
    );
    assert!(
        fixture
            .repository
            .compaction("agent".into())
            .await
            .unwrap()
            .is_some()
    );
    runtime.close().await;
}

#[tokio::test]
async fn actual_input_and_exhausted_context_fail_before_any_provider_request_with_typed_diagnostics()
 {
    for (context, input_limit, message) in [
        (400_000, 1, "input limit"),
        (
            1,
            0,
            "A complete conversation group exceeds the compaction input budget.",
        ),
    ] {
        let fixture = Fixture::new();
        let provider = Scripted::default();
        let runtime = BudgetRuntime::new(
            &fixture,
            &provider,
            context,
            input_limit,
            0,
            profile(),
            BudgetTools::new(&[]),
        )
        .await;
        let sent = runtime
            .send("prompt", protocol::Delivery::DELIVERY_STEER)
            .await;
        let execution = bounded(sent.execution.join()).await.unwrap();
        assert_eq!(execution.status, ExecutionStatus::Failed);
        assert!(provider.requests.lock().unwrap().is_empty());
        assert_eq!(provider.stream_closes.load(Ordering::SeqCst), 0);
        let events = fixture.events().await;
        let failures: Vec<_> = events
            .iter()
            .filter(|event| event.has_turn_failed())
            .collect();
        assert_eq!(failures.len(), 1);
        assert!(
            failures[0].turn_failed().message.contains(message),
            "{}",
            failures[0].turn_failed().message
        );
        assert!(failures[0].turn_failed().provider_response_body.is_empty());
        let diagnostic = runtime.activity.diagnostic.lock().unwrap().clone();
        assert_eq!(diagnostic.len(), 2);
        assert!(
            diagnostic[1].ends_with(":failed:Some(\"invalid_operation\")"),
            "{diagnostic:?}"
        );
        runtime.close().await;
    }
}

#[tokio::test]
async fn actual_profile_allowed_and_disabled_tools_filter_every_request() {
    let fixture = Fixture::new();
    let provider = Scripted::new(vec![tool_step("held"), Step::complete("done")]);
    let mut selected = profile();
    selected.allowed_tools = Some(vec!["settled".into(), "held".into()]);
    selected.disabled_tools = vec!["settled".into()];
    let tools = BudgetTools::new(&["settled", "held", "not-allowed", "HELD"]);
    let runtime =
        BudgetRuntime::new(&fixture, &provider, 100_000, 0, 0, selected, tools.clone()).await;
    let sent = runtime
        .send("prompt", protocol::Delivery::DELIVERY_STEER)
        .await;
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(tool_names(&requests[0]), ["held"]);
        assert_eq!(tool_names(&requests[1]), ["held"]);
    }
    assert_eq!(tools.materializations.load(Ordering::SeqCst), 1);
    runtime.close().await;
}

#[tokio::test]
async fn actual_turn_captures_profile_then_final_phase_filters_tools_and_next_turn_restores_current_selection()
 {
    let fixture = Fixture::new();
    let first_gate = Gate::new();
    let final_gate = Gate::new();
    let next_gate = Gate::new();
    let mut first = tool_step("worker");
    first.gate = Some(first_gate.clone());
    let mut final_step = Step::complete("first answer");
    final_step.gate = Some(final_gate.clone());
    let mut next = Step::complete("queued answer");
    next.gate = Some(next_gate.clone());
    let provider = Scripted::new(vec![first, final_step, next]);
    let mut original = profile();
    original.maximum_requests = 2;
    original.allowed_tools = Some(vec!["worker".into(), "survivor".into()]);
    let tools = BudgetTools::new(&["worker", "survivor", "new-tool"]);
    let runtime =
        BudgetRuntime::new(&fixture, &provider, 100_000, 0, 0, original, tools.clone()).await;
    let sent = runtime
        .send("first prompt", protocol::Delivery::DELIVERY_STEER)
        .await;
    first_gate.entered().await;
    let mut updated = runtime.selection.capture().unwrap();
    updated.profile = profile();
    updated.profile.id = "updated".into();
    updated.profile.allowed_tools = Some(vec!["new-tool".into()]);
    runtime.selection.update(updated).unwrap();
    runtime
        .send("queued prompt", protocol::Delivery::DELIVERY_QUEUE)
        .await;
    first_gate.open();
    final_gate.entered().await;
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(tool_names(&requests[0]), ["worker", "survivor"]);
        assert_eq!(tool_names(&requests[1]), ["survivor"]);
        assert!(
            requests[1]
                .messages
                .iter()
                .any(|message| message.role == sessions::Role::System
                    && message.content().contains("final provider request"))
        );
        assert!(
            !requests[1]
                .messages
                .iter()
                .any(|message| message.role == sessions::Role::System
                    && message.content().contains("Tool access is restored"))
        );
    }
    final_gate.open();
    next_gate.entered().await;
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(tool_names(&requests[2]), ["new-tool"]);
        assert!(
            requests[2]
                .messages
                .iter()
                .any(|message| message.role == sessions::Role::System
                    && message.content().contains("Tool access is restored"))
        );
    }
    next_gate.open();
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    assert_eq!(tools.materializations.load(Ordering::SeqCst), 1);
    let events = fixture.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_turn_started())
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_turn_ended())
            .map(|event| event.turn_ended().finish_reason.as_str())
            .collect::<Vec<_>>(),
        ["stop", "stop"]
    );
    assert!(!events.iter().any(|event| event.has_turn_failed()));
    runtime.close().await;
}
