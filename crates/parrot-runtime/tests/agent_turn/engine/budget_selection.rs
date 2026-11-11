//! Actual drain requests for the source token-budget and captured-profile predicates.
//! Registered leaf tools exercise production catalog filtering and execution.
use super::*;
use parrot_domain::contracts::async_ops::{Cancelled, Clock};
use parrot_runtime::active_work::status::RuntimeStatus;
use parrot_runtime::active_work::status::StatusQuery;
use std::time::{Instant, SystemTime};

/// Reproduces the original first-turn initial status injection for these
/// budget fixtures: one offered static slot steers the assembly once.
struct InitialStatus(Arc<Trace>);
impl ContextStatus for InitialStatus {
    fn observe<'a>(
        &'a self,
        _: &'a ContextSnapshot,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<String>> {
        Box::pin(async move {
            self.0.push("status");
            Ok("actual status".into())
        })
    }
    fn slots<'a>(
        &'a self,
        _: &'a ContextSnapshot,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<parrot_core::StatusOffering>> {
        Box::pin(async move {
            self.0.push("status");
            Ok([(
                parrot_core::StatusKey::new("mode"),
                parrot_core::StatusOffer {
                    hash: "initial".into(),
                    text: "actual status".into(),
                    is_auto_update: false,
                },
            )]
            .into_iter()
            .collect())
        })
    }
}
impl TurnStatus<Security> for InitialStatus {
    fn bind(&self, _: &TurnSelection<Security>) -> Arc<dyn ContextStatus> {
        Arc::new(InitialStatus(self.0.clone()))
    }
}
/// The model budget inputs shared by every budget fixture composition.
struct BudgetLimits {
    context_window: i32,
    maximum_input_tokens: i32,
    maximum_output_tokens: i32,
}
struct FixedStatusClock;
impl Clock for FixedStatusClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn utc_now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(86_400)
    }
    fn sleep_until<'a>(
        &'a self,
        _: Instant,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Cancelled>> {
        Box::pin(async move {
            cancellation.cancelled().await;
            Err(Cancelled)
        })
    }
}
/// Offers the real runtime status slots, including the context reminder, so the
/// end-to-end test exercises the actual band hash through the engine estimator.
struct RealReminderStatus(Arc<RuntimeStatus>);
impl RealReminderStatus {
    fn new() -> Arc<Self> {
        Arc::new(Self(Arc::new(
            RuntimeStatus::new(
                templates(),
                Arc::new(FixedStatusClock),
                SystemTime::UNIX_EPOCH,
                vec![],
            )
            .unwrap(),
        )))
    }
}
impl TurnStatus<Security> for RealReminderStatus {
    fn bind(&self, selection: &TurnSelection<Security>) -> Arc<dyn ContextStatus> {
        self.0.bind_context(
            StatusQuery {
                session_id: "agent".into(),
                parent_session_id: String::new(),
                parent_session_name: String::new(),
                profile: selection.profile.id.clone(),
                requested_model: "scripted/model".into(),
            },
            String::new(),
        )
    }
}
struct BudgetTools {
    inner: Tools,
    definitions: Vec<sessions::Tool>,
    materializations: AtomicUsize,
    executions: ToolObservations,
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
            executions: Arc::new(Mutex::new(Vec::new())),
        })
    }
}
impl ToolFactory<Security> for BudgetTools {
    fn materialize(&self) -> Result<ToolInventory<Security>, TurnError> {
        self.materializations.fetch_add(1, Ordering::SeqCst);
        Ok(inventory(
            self.definitions
                .iter()
                .map(|definition| {
                    ToolHandle::Borrowed(Arc::new(FixtureTool {
                        metadata: ToolMetadata {
                            name: definition.name.clone(),
                            aliases: vec![],
                        },
                        tools: self.inner.clone(),
                        final_phase: definition.name == "survivor",
                        text: "fake tool result",
                        observations: Some(self.executions.clone()),
                    }))
                })
                .collect(),
            &self.definitions,
        ))
    }
}
struct BudgetRuntime {
    session: AgentSession<RuntimeBackend<Security>>,
    selection: Arc<Selection>,
    activity: Arc<Activity>,
}
impl BudgetRuntime {
    async fn new(
        fixture: &Fixture,
        provider: &Scripted,
        limits: BudgetLimits,
        profile: TurnProfile,
        tools: Arc<BudgetTools>,
    ) -> Self {
        Self::compose(fixture, provider, limits, profile, tools, |trace| {
            Arc::new(InitialStatus(trace.clone()))
        })
        .await
    }
    async fn compose(
        fixture: &Fixture,
        provider: &Scripted,
        limits: BudgetLimits,
        profile: TurnProfile,
        tools: Arc<BudgetTools>,
        status: impl FnOnce(Arc<Trace>) -> Arc<dyn TurnStatus<Security>>,
    ) -> Self {
        let mut model = Model::new("model", "scripted");
        model.context_window = limits.context_window;
        model.max_input_tokens = limits.maximum_input_tokens;
        model.max_output_tokens = limits.maximum_output_tokens;
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
            tool_execution: tool_execution_config(
                &fixture.history,
                templates.clone(),
                publisher.clone(),
            ),
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
            status: status(trace.clone()),
            completion_sources: vec![],
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
            BudgetLimits {
                context_window,
                maximum_input_tokens: 0,
                maximum_output_tokens: output_limit,
            },
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
        BudgetLimits {
            context_window: 180_000,
            maximum_input_tokens: 0,
            maximum_output_tokens: 0,
        },
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
async fn intermediate_usage_calibrates_later_requests_even_when_the_stream_fails() {
    for stream_fails in [false, true] {
        let fixture = Fixture::new();
        let mut first = Step::complete("first answer");
        first
            .events
            .push_back(sessions::Event::Completed(Completion {
                input_tokens: 147_776,
                ..Completion::default()
            }));
        first.completion.as_mut().unwrap().input_tokens = 0;
        if stream_fails {
            first.completion = Err(Error::Provider("failure after completion".into()));
        }
        let provider = Scripted::new(vec![
            first,
            Step::complete("compacted summary"),
            Step::complete("done"),
        ]);
        let runtime = BudgetRuntime::new(
            &fixture,
            &provider,
            BudgetLimits {
                context_window: 180_000,
                maximum_input_tokens: 0,
                maximum_output_tokens: 0,
            },
            profile(),
            BudgetTools::new(&[]),
        )
        .await;
        let first = runtime
            .send("first prompt", protocol::Delivery::DELIVERY_STEER)
            .await;
        let first = bounded(first.execution.join()).await.unwrap();
        assert_eq!(
            first.status,
            if stream_fails {
                ExecutionStatus::Failed
            } else {
                ExecutionStatus::Succeeded
            }
        );
        assert_eq!(provider.stream_closes.load(Ordering::SeqCst), 1);
        let next = runtime
            .send("second prompt", protocol::Delivery::DELIVERY_STEER)
            .await;
        assert_eq!(
            bounded(next.execution.join()).await.unwrap().status,
            ExecutionStatus::Succeeded
        );
        {
            let requests = provider.requests.lock().unwrap();
            assert_eq!(requests.len(), 3);
            assert_eq!(requests[1].max_output_tokens, 1024);
            assert_eq!(requests[2].max_output_tokens, 32_768);
        }
        let events = fixture.events().await;
        let usage: Vec<_> = events
            .iter()
            .filter(|event| event.has_request_usage_recorded())
            .map(|event| event.request_usage_recorded().input_tokens)
            .collect();
        assert_eq!(
            usage,
            if stream_fails {
                vec![147_776, 17]
            } else {
                vec![147_776, 0, 17]
            }
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.has_compaction_finished())
                .count(),
            1
        );
        runtime.close().await;
    }
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
            BudgetLimits {
                context_window: context,
                maximum_input_tokens: input_limit,
                maximum_output_tokens: 0,
            },
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
    let runtime = BudgetRuntime::new(
        &fixture,
        &provider,
        BudgetLimits {
            context_window: 100_000,
            maximum_input_tokens: 0,
            maximum_output_tokens: 0,
        },
        selected,
        tools.clone(),
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
        assert_eq!(requests.len(), 2);
        assert_eq!(tool_names(&requests[0]), ["held"]);
        assert_eq!(tool_names(&requests[1]), ["held"]);
    }
    assert_eq!(tools.materializations.load(Ordering::SeqCst), 1);
    runtime.close().await;
}

#[tokio::test]
async fn scheduled_work_precedes_queued_input_and_starts_a_fresh_core_budget_scope() {
    let fixture = Fixture::new();
    let first_gate = Gate::new();
    let second_gate = Gate::new();
    let scheduled_gate = Gate::new();
    let mut first = Step::complete("first answer");
    first.gate = Some(first_gate.clone());
    let mut second = Step::complete("queued answer");
    second.gate = Some(second_gate.clone());
    let provider = Scripted::new(vec![first, second]);
    let mut selected_profile = profile();
    selected_profile.maximum_requests = 1;
    let runtime = BudgetRuntime::new(
        &fixture,
        &provider,
        BudgetLimits {
            context_window: 100_000,
            maximum_input_tokens: 0,
            maximum_output_tokens: 0,
        },
        selected_profile,
        BudgetTools::new(&[]),
    )
    .await;
    let sent = runtime
        .send("first prompt", protocol::Delivery::DELIVERY_STEER)
        .await;
    first_gate.entered().await;
    let queued = runtime
        .send("queued prompt", protocol::Delivery::DELIVERY_QUEUE)
        .await;
    let scheduled = Arc::new(AtomicBool::new(false));
    let observed = scheduled.clone();
    let gate = scheduled_gate.clone();
    let work = runtime
        .session
        .schedule_work(
            move |_, cancellation| {
                Box::pin(async move {
                    observed.store(true, Ordering::SeqCst);
                    gate.wait(cancellation.as_ref()).await.map_err(|_| {
                        parrot_runtime::agent_turn::lifecycle::LifecycleError::Cancelled
                    })
                })
            },
            Arc::new(NeverCancelled),
        )
        .unwrap();
    assert!(!scheduled.load(Ordering::SeqCst));
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    first_gate.open();
    scheduled_gate.entered().await;
    assert!(scheduled.load(Ordering::SeqCst));
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    let pending = fixture.history.peek_pending_input().await.unwrap().unwrap();
    assert_eq!(pending.id, queued.admission.input.id);
    assert_eq!(pending.message_id, queued.admission.input.message_id);
    assert_eq!(pending.delivery, protocol::Delivery::DELIVERY_QUEUE);
    scheduled_gate.open();
    bounded(work.join()).await.unwrap();
    second_gate.entered().await;
    {
        let requests = provider.requests.lock().unwrap();
        assert!(
            requests[1]
                .messages
                .iter()
                .any(|message| message.role == sessions::Role::User
                    && message.content().contains("queued prompt"))
        );
        assert_eq!(
            requests
                .iter()
                .map(|request| request
                    .messages
                    .iter()
                    .filter(|message| message.role == sessions::Role::System
                        && message.content().contains("final provider request"))
                    .count())
                .collect::<Vec<_>>(),
            [1, 2]
        );
    }
    let events = fixture.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_turn_started())
            .count(),
        2
    );
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
    let mut final_step = tool_step("survivor");
    final_step.gate = Some(final_gate.clone());
    let mut next = tool_step("new-tool");
    next.gate = Some(next_gate.clone());
    let provider = Scripted::new(vec![
        first,
        final_step,
        Step::complete("first answer"),
        next,
        Step::complete("queued answer"),
    ]);
    let mut original = profile();
    original.maximum_requests = 2;
    original.allowed_tools = Some(vec!["worker".into(), "survivor".into()]);
    let tools = BudgetTools::new(&["worker", "survivor", "new-tool"]);
    let runtime = BudgetRuntime::new(
        &fixture,
        &provider,
        BudgetLimits {
            context_window: 100_000,
            maximum_input_tokens: 0,
            maximum_output_tokens: 0,
        },
        original,
        tools.clone(),
    )
    .await;
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
        assert_eq!(requests.len(), 4);
        assert_eq!(tool_names(&requests[3]), ["new-tool"]);
        let restored: Vec<_> = requests[3]
            .messages
            .iter()
            .enumerate()
            .filter(|(_, message)| {
                message.role == sessions::Role::System
                    && message.content().contains("Tool access is restored")
            })
            .map(|(index, _)| index)
            .collect();
        assert_eq!(restored.len(), 1);
        let queued = requests[3]
            .messages
            .iter()
            .position(|message| {
                message.role == sessions::Role::User && message.content().contains("queued prompt")
            })
            .unwrap();
        assert!(restored[0] < queued);
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
    assert_eq!(
        *tools.executions.lock().unwrap(),
        [
            ("original".into(), vec!["worker".into(), "survivor".into()]),
            ("original".into(), vec!["survivor".into()]),
            ("updated".into(), vec!["new-tool".into()]),
        ]
    );
    runtime.close().await;
}

#[tokio::test]
async fn core_context_preparation_persists_one_reminder_across_later_native_invocations() {
    let fixture = Fixture::new();
    let provider = Scripted::new(vec![
        Step::complete("first answer"),
        Step::complete("second answer"),
        Step::complete("third answer"),
    ]);
    let runtime = BudgetRuntime::compose(
        &fixture,
        &provider,
        BudgetLimits {
            context_window: 10_000,
            maximum_input_tokens: 0,
            maximum_output_tokens: 2_048,
        },
        profile(),
        BudgetTools::new(&[]),
        |_| RealReminderStatus::new(),
    )
    .await;
    let first = runtime
        .send("first prompt", protocol::Delivery::DELIVERY_STEER)
        .await;
    assert_eq!(
        bounded(first.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(
            !requests[0]
                .messages
                .iter()
                .any(|message| message.role == sessions::Role::System
                    && message.content().contains("notification band"))
        );
    }
    assert!(
        fixture
            .history
            .standing_statuses()
            .await
            .unwrap()
            .unwrap_or_default()
            .contains_key(&parrot_core::StatusKey::new("runtime:context"))
    );

    let second = runtime
        .send(&"x ".repeat(5_500), protocol::Delivery::DELIVERY_STEER)
        .await;
    assert_eq!(
        bounded(second.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    let standing = fixture
        .history
        .standing_statuses()
        .await
        .unwrap()
        .unwrap_or_default();
    assert!(standing.contains_key(&parrot_core::StatusKey::new("runtime:context")));
    let reminder = {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let message = requests[1].messages.last().unwrap();
        assert_eq!(message.role, sessions::Role::System);
        assert!(
            message
                .content()
                .contains("Context usage entered the 50% notification band")
        );
        message.content().to_owned()
    };

    let third = runtime
        .send("third prompt", protocol::Delivery::DELIVERY_STEER)
        .await;
    assert_eq!(
        bounded(third.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[2]
                .messages
                .iter()
                .filter(|message| message.role == sessions::Role::System
                    && message.content() == reminder)
                .count(),
            1
        );
        assert!(
            !requests[2]
                .messages
                .iter()
                .any(|message| message.role == sessions::Role::System
                    && message.content().contains("60% notification band"))
        );
    }
    assert!(
        fixture
            .history
            .standing_statuses()
            .await
            .unwrap()
            .unwrap_or_default()
            .contains_key(&parrot_core::StatusKey::new("runtime:context"))
    );
    assert_eq!(provider.turn_begins.load(Ordering::SeqCst), 3);
    assert!(
        !fixture
            .events()
            .await
            .iter()
            .any(|event| event.has_compaction_started() || event.has_turn_failed())
    );
    runtime.close().await;
}
