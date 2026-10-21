//! Exact A003 source predicates through the real drain; injected completion/tool
//! services here are test ports, not the production T47/T48 composition.
use super::*;
use parrot_domain::values::accounting::AgentUsageTotals;
use parrot_provider::catalogs::ModelMetadataFields;

#[derive(Clone)]
struct ReminderBindings {
    reminders: Arc<ExitReminders<RuntimePublisher>>,
    publisher: Arc<Publisher>,
}
struct ReminderCompletion {
    history: AgentHistoryView,
    bindings: Mutex<Option<ReminderBindings>>,
    calls: AtomicUsize,
}
impl TurnCompletion<Security> for ReminderCompletion {
    fn complete<'a>(
        &'a self,
        _: &'a CompletionCandidate<Security>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<CompletionDecision, TurnError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let ReminderBindings {
                reminders,
                publisher,
            } = self.bindings.lock().unwrap().clone().unwrap();
            let Some(notice) = reminders
                .build()
                .await
                .map_err(|error| TurnError::Service(error.to_string()))?
            else {
                return Ok(CompletionDecision::Complete);
            };
            let mut event = Event::new();
            event.id = id();
            event.agent_session_id = "agent".into();
            event.set_agent_notice(protocol::AgentNotice {
                kind: "exit_reminder".into(),
                notice: notice.clone(),
                ..Default::default()
            });
            self.history.append(event.clone(), None, None).await?;
            publisher
                .publish_committed(&Committed::after_commit(event), cancellation)
                .await
                .map_err(TurnError::Publication)?;
            Ok(CompletionDecision::Retry {
                system_message: notice,
                retain_candidate_assistant: false,
                completion_retry_pending: Some(true),
            })
        })
    }
}

#[tokio::test]
async fn a003_completion_observes_shared_reminder_changed_after_construction_with_one_ending() {
    let fixture = Fixture::new();
    let first_gate = Gate::new();
    let second_gate = Gate::new();
    let mut first = Step::complete("premature");
    first.gate = Some(first_gate.clone());
    let mut second = Step::complete("finished");
    second.gate = Some(second_gate.clone());
    let provider = Scripted::new(vec![first, second]);
    let callback = Arc::new(ReminderCompletion {
        history: fixture.history.clone(),
        bindings: Mutex::new(None),
        calls: AtomicUsize::new(0),
    });
    let runtime = fixture
        .runtime(
            provider.clone(),
            Arc::new(Tools::default()),
            vec![callback.clone()],
            Arc::new(Signal::new()),
            0,
        )
        .await;
    *callback.bindings.lock().unwrap() = Some(ReminderBindings {
        reminders: runtime.session.backend().reminders().clone(),
        publisher: runtime.publisher.clone(),
    });
    let sent = runtime
        .send("question", protocol::Delivery::DELIVERY_STEER)
        .await;
    first_gate.entered().await;
    runtime
        .session
        .set_exit_reminder(
            "goal".into(),
            "changed after construction".into(),
            Arc::new(NeverCancelled),
        )
        .unwrap()
        .join()
        .await
        .unwrap();
    first_gate.open();
    second_gate.entered().await;
    let events = fixture.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_agent_notice() && event.agent_notice().kind == "exit_reminder")
            .count(),
        1
    );
    assert!(
        provider.requests.lock().unwrap()[1]
            .messages
            .iter()
            .any(|message| {
                message.role == sessions::Role::System
                    && message.content().contains("changed after construction")
            })
    );
    runtime
        .session
        .clear_exit_reminder("goal".into(), Arc::new(NeverCancelled))
        .unwrap()
        .join()
        .await
        .unwrap();
    second_gate.open();
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    runtime.close().await;
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    assert_eq!(callback.calls.load(Ordering::SeqCst), 2);
    let events = fixture.events().await;
    assert_eq!(
        events.iter().filter(|event| event.has_turn_ended()).count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .find(|event| event.has_turn_ended())
            .unwrap()
            .turn_ended()
            .finish_reason,
        "stop"
    );
}

#[tokio::test]
async fn a003_queued_prompt_gets_its_own_turn_with_two_starts_and_two_endings() {
    let fixture = Fixture::new();
    let first_gate = Gate::new();
    let second_gate = Gate::new();
    let mut first = Step::complete("first answer");
    first.gate = Some(first_gate.clone());
    let mut second = Step::complete("second answer");
    second.gate = Some(second_gate.clone());
    let provider = Scripted::new(vec![first, second]);
    let runtime = fixture
        .runtime(
            provider.clone(),
            Arc::new(Tools::default()),
            vec![],
            Arc::new(Signal::new()),
            0,
        )
        .await;
    let sent = runtime
        .send("first prompt", protocol::Delivery::DELIVERY_STEER)
        .await;
    first_gate.entered().await;
    let queued = runtime
        .send("queued prompt", protocol::Delivery::DELIVERY_QUEUE)
        .await;
    first_gate.open();
    second_gate.entered().await;
    let prompts = provider.requests.lock().unwrap()[1]
        .messages
        .iter()
        .filter(|message| message.role == sessions::Role::User)
        .map(|message| message.content())
        .collect::<Vec<_>>()
        .join(" | ");
    assert_eq!(prompts, "first prompt | queued prompt");
    second_gate.open();
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    assert_eq!(
        bounded(queued.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    runtime.close().await;
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    let events = fixture.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_turn_started())
            .count(),
        2
    );
    assert_eq!(
        events.iter().filter(|event| event.has_turn_ended()).count(),
        2
    );
    assert!(
        events
            .iter()
            .filter(|event| event.has_turn_ended())
            .all(|event| event.turn_ended().finish_reason == "stop")
    );
}

/// Records the real usage fact before the injected tool service's actual durable
/// settlement. The production tool settlement adapter remains T47-owned.
#[derive(Default)]
struct UsageTools(Tools);
impl TurnTools<Security> for UsageTools {
    type Snapshot = Vec<sessions::Tool>;
    fn materialize(&self) -> Result<Self::Snapshot, TurnError> {
        self.0.materialize()
    }
    fn permitted(
        &self,
        snapshot: &Self::Snapshot,
        selection: &TurnSelection<Security>,
    ) -> Result<Self::Snapshot, TurnError> {
        self.0.permitted(snapshot, selection)
    }
    fn definitions<'a>(&self, snapshot: &'a Self::Snapshot) -> &'a [sessions::Tool] {
        snapshot
    }
    fn enabled_after_interruption(&self, snapshot: &Self::Snapshot) -> Self::Snapshot {
        snapshot.clone()
    }
    fn reconcile<'a>(
        &'a self,
        resources: TurnResources<'a>,
        selection: &'a TurnSelection<Security>,
        snapshot: &'a Self::Snapshot,
        source: &'a dyn SelectionSource<Security>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), TurnError>> {
        Box::pin(async move {
            for assistant in resources.history.conversation().await? {
                for call in &assistant.message.tool_calls {
                    if resources
                        .history
                        .tool_terminals_of_calls(vec![call.id.clone()])
                        .await?
                        .is_empty()
                    {
                        resources
                            .usage
                            .record_tool(
                                assistant.sequence,
                                &sessions::ToolCall {
                                    id: call.id.clone(),
                                    name: call.name.clone(),
                                    arguments_json: call.arguments_json.clone(),
                                },
                            )
                            .await?;
                        let mut started = Event::new();
                        started.id = id();
                        started.agent_session_id = "agent".into();
                        started.set_tool_started(protocol::ToolStarted {
                            tool_call_id: call.id.clone(),
                            tool_name: call.name.clone(),
                            ..Default::default()
                        });
                        resources.events.emit(started, cancellation).await?;
                    }
                }
            }
            self.0
                .reconcile(resources, selection, snapshot, source, cancellation)
                .await
        })
    }
}

async fn usage_runtime(
    fixture: &Fixture,
    provider: Scripted,
) -> AgentSession<RuntimeBackend<Security, UsageTools>> {
    let tools = Arc::new(UsageTools::default());
    let trace = tools.0.trace.clone();
    let broker = Arc::new(EventBroker::new());
    let publisher: Arc<RuntimePublisher> = Arc::new(Publisher {
        history: fixture.history.clone(),
        broker: broker.clone(),
        terminal_failure: 0,
        terminals: AtomicUsize::new(0),
        terminal_observed: AtomicBool::new(false),
        cancel_retry: AtomicBool::new(false),
        reminder_gate: Mutex::new(None),
        trace: trace.clone(),
    });
    let mut model = Model::new("model", "scripted");
    model.context_window = 100000;
    model.input_price = 0.125;
    model.cached_input_price = 0.025;
    model.output_price = 0.25;
    model.fields.insert(ModelMetadataFields::CACHED_INPUT_PRICE);
    let registry = Arc::new(
        ProviderRegistry::new(
            vec![Arc::new(provider)],
            BTreeMap::from([("scripted".into(), vec![model])]),
        )
        .unwrap(),
    );
    let aliases = Arc::new(ModelAliasCatalog::new(registry.clone(), vec![]).unwrap());
    let model = ModelRouter::new(
        registry,
        Arc::new(ModelRouting::new(aliases, "scripted/model")),
    )
    .resolve("scripted/model")
    .unwrap();
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
    let backend = RuntimeBackend::new(RuntimeConfig {
        agent_limits: parrot_foundation::config::runtime::AgentLimitsConfig {
            disable_max_turns: false,
            ..Default::default()
        },
        identity: SessionIdentity {
            session_id: "agent".into(),
            name: "test".into(),
            parent_session_id: String::new(),
            depth: 1,
        },
        history: fixture.history.clone(),
        broker,
        publisher,
        selection: Arc::new(Selection(Mutex::new(TurnSelection {
            model,
            profile: TurnProfile {
                id: "mode".into(),
                maximum_requests: 8,
                allowed_tools: None,
                disabled_tools: vec![],
            },
            security: Arc::new(Security),
        }))),
        tools,
        prompt: Box::new(Prompt {
            trace: trace.clone(),
            sampled: false,
            cancel_sample: false,
        }),
        skills: Arc::new(Skills(trace.clone())),
        context: fixture.controller(),
        status: Arc::new(Status(trace.clone())),
        completions: vec![],
        templates,
        reminders,
        output: Arc::new(Output::default()),
        activity: Arc::new(Activity::new(trace.clone())),
        parent_completion: Arc::new(Parent(trace)),
        ancestors: vec![],
        event_id: Arc::new(id),
    })
    .await
    .unwrap();
    AgentSession::new(backend, Arc::new(Signal::new()))
}

async fn send_usage(session: &AgentSession<RuntimeBackend<Security, UsageTools>>, text: &str) {
    let sent = bounded(session.send(
        vec![ConversationPart::text(text)],
        id(),
        protocol::Delivery::DELIVERY_STEER,
        IncomingActivity {
            name: "test".into(),
            cause: None,
            source: protocol::InputSource::default(),
        },
        Arc::new(NeverCancelled),
    ))
    .await
    .unwrap();
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
}

#[tokio::test]
async fn a003_statistics_replay_exact_facts_tool_round_and_two_cumulative_terminal_totals() {
    let fixture = Fixture::new();
    let first = Completion {
        finish_reason: "tool_calls".into(),
        input_tokens: 10,
        cached_input_tokens: 3,
        output_tokens: 4,
        tool_calls: vec![sessions::ToolCall {
            id: "call-1".into(),
            name: "settled".into(),
            arguments_json: "{}".into(),
        }],
        ..Default::default()
    };
    let second = Completion {
        finish_reason: "stop".into(),
        assistant_text: "first".into(),
        input_tokens: 7,
        cached_input_tokens: 2,
        output_tokens: 5,
        ..Default::default()
    };
    let provider = Scripted::new(vec![
        Step {
            completion: Ok(first),
            gate: None,
            close_gate: None,
            events: VecDeque::new(),
        },
        Step {
            completion: Ok(second),
            gate: None,
            close_gate: None,
            events: VecDeque::new(),
        },
    ]);
    let session = usage_runtime(&fixture, provider.clone()).await;
    send_usage(&session, "first prompt").await;
    bounded(session.close()).await.unwrap();
    assert_eq!(provider.requests.lock().unwrap().len(), 2);

    // New SQLite connection, history projection, provider pool and usage recorder:
    // the second cumulative terminal must be restored, not manually pre-seeded.
    let repository = Fixture::open(&fixture.path);
    let restored = Fixture {
        path: fixture.path.clone(),
        history: Fixture::history_for(&fixture.path, &repository),
        repository,
    };
    let third = Completion {
        finish_reason: "stop".into(),
        assistant_text: "second".into(),
        input_tokens: 6,
        cached_input_tokens: 1,
        output_tokens: 2,
        ..Default::default()
    };
    let provider = Scripted::new(vec![Step {
        completion: Ok(third),
        gate: None,
        close_gate: None,
        events: VecDeque::new(),
    }]);
    let session = usage_runtime(&restored, provider.clone()).await;
    assert_eq!(
        session.backend().usage().capture().own.totals.input_tokens,
        17
    );
    send_usage(&session, "second prompt").await;
    bounded(session.close()).await.unwrap();
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    let replay = restored.events().await;
    let facts: Vec<_> = replay
        .iter()
        .filter(|event| event.has_request_usage_recorded())
        .map(Event::request_usage_recorded)
        .collect();
    assert_eq!(facts.len(), 3);
    assert_eq!(
        facts
            .iter()
            .map(|fact| format!(
                "{}:{}:{}:{}:{}",
                fact.input_tokens,
                fact.cached_input_tokens,
                fact.output_tokens,
                fact.context_size,
                fact.context_limit
            ))
            .collect::<Vec<_>>()
            .join(" | "),
        "10:3:4:10:100000 | 7:2:5:7:100000 | 6:1:2:6:100000"
    );
    assert_eq!(
        facts.iter().map(|fact| fact.input_cost).collect::<Vec<_>>(),
        [0.95, 0.675, 0.65]
    );
    assert_eq!(
        facts
            .iter()
            .map(|fact| fact.output_cost)
            .collect::<Vec<_>>(),
        [1.0, 1.25, 0.5]
    );
    assert_eq!(
        replay
            .iter()
            .filter(|event| event.has_turn_ended())
            .map(|event| format!(
                "{}:{}",
                event.turn_ended().input_tokens,
                event.turn_ended().output_tokens
            ))
            .collect::<Vec<_>>()
            .join(" | "),
        "17:9 | 23:11"
    );
    assert!(
        replay
            .iter()
            .filter(|event| event.has_turn_ended())
            .all(|event| event.turn_ended().finish_reason == "stop")
    );
    assert!(
        replay
            .iter()
            .position(|event| event.has_request_usage_recorded())
            .unwrap()
            < replay
                .iter()
                .position(|event| event.has_tool_started())
                .unwrap()
    );
    let recovered = restored
        .repository
        .events()
        .replay_statistics()
        .await
        .unwrap();
    let expected = AgentUsageTotals {
        input_tokens: 23,
        cached_input_tokens: 6,
        output_tokens: 11,
        tool_calls: 1,
        input_cost: 2.275,
        output_cost: 2.75,
    };
    assert_eq!(recovered.agents["agent"].own.totals, expected);
    assert_eq!(session.backend().usage().capture().own.totals, expected);
    let runtime_statistics = restored
        .repository
        .events()
        .get_runtime_statistics()
        .await
        .unwrap();
    assert_eq!(
        runtime_statistics
            .get_agent_statistics("agent")
            .capture()
            .own
            .totals,
        expected
    );
    assert!(
        !replay
            .iter()
            .any(|event| event.has_agent_statistics_updated())
    );
}
