//! The pre-existing T47 fixtures construct actual SQLite history, ContextController,
//! ToolSettlement and RuntimeBackend. Their fake provider has no network/credentials.
#[allow(dead_code, unused_imports)]
#[path = "../../../parrot-runtime/tests/tool_settlement/fixture.rs"]
mod fixture;
use fixture::*;
#[allow(dead_code, unused_imports)]
#[path = "../../../parrot-runtime/tests/tool_settlement/harness.rs"]
mod harness;
use harness::*;
#[path = "../../../parrot-runtime/tests/tool_settlement/runtime.rs"]
mod runtime;
use parrot_domain::contracts::runtime::SessionControl;
use parrot_host::composition::context::{
    CompactRequest, ContextControl, ContextSelection, ContextServices, RetainedCompactTool,
};
use parrot_runtime::{agent_turn::lifecycle::ExecutionStatus, prompt_context::SessionPrompt};
use parrot_tools::session::{self, Failure, SessionServices};

struct Ancillary;
impl SessionServices for Ancillary {
    type Security = SharedSecurity<Security>;
    type Selection = ContextSelection<Security>;
    fn checkpoint<'a>(
        &'a self,
        _: &'a str,
        _: i64,
        _: &'a str,
    ) -> BoxFuture<'a, Result<(), Failure>> {
        panic!("not exercised by compact")
    }
    fn set_reminder<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<Failure>>> {
        panic!("not exercised by compact")
    }
    fn clear_reminder<'a>(
        &'a self,
        _: &'a str,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, OperationError<Failure>>> {
        panic!("not exercised by compact")
    }
    fn reminder_titles(&self) -> BoxFuture<'_, Vec<String>> {
        panic!("not exercised by compact")
    }
    fn runtime_status<'a>(
        &'a self,
        _: &'a Self::Selection,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<String, OperationError<Failure>>> {
        panic!("not exercised by compact")
    }
    fn statistics_status<'a>(
        &'a self,
        _: &'a Self::Selection,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<String, OperationError<Failure>>> {
        panic!("not exercised by compact")
    }
    fn compact<'a>(
        &'a self,
        _: &'a Self::Selection,
        _: Option<ContextSize>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<session::Compaction, OperationError<Failure>>> {
        panic!("must route through frozen SessionControl")
    }
    fn estimate_after_tool_result(
        &self,
        _: &Self::Selection,
        _: &str,
        _: &str,
    ) -> Result<ContextSnapshot, Failure> {
        panic!("must estimate actual postcompact history")
    }
}
impl SessionControl for Ancillary {
    type CheckpointRequest = String;
    type CheckpointResult = String;
    type ReminderRequest = String;
    type ReminderResult = String;
    type ClearReminderRequest = String;
    type ClearReminderResult = String;
    type CompactRequest = ();
    type CompactResult = ();
    type Status = &'static str;
    type Error = Failure;
    fn checkpoint<'a>(
        &'a self,
        value: String,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<String, OperationError<Failure>>> {
        Box::pin(async move { Ok(value) })
    }
    fn compact<'a>(
        &'a self,
        _: (),
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<Failure>>> {
        panic!("must use actual context route")
    }
    fn set_reminder(&self, value: String) -> Result<String, Failure> {
        Ok(value)
    }
    fn clear_reminder(&self, value: String) -> Result<String, Failure> {
        Ok(value)
    }
    fn status(&self) -> Result<&'static str, Failure> {
        Ok("ancillary status")
    }
}

type Bridge = (
    Arc<RetainedCompactTool<Security>>,
    Arc<ContextControl<Ancillary>>,
    Arc<session::SessionTool<ContextServices<Security, Ancillary, Ancillary>>>,
);
fn bridge() -> Bridge {
    let ancillary = Arc::new(Ancillary);
    let control = Arc::new(ContextControl::new(ancillary.clone()));
    let services = Arc::new(ContextServices::new(ancillary, control.clone()));
    let tool = Arc::new(session::SessionTool::new(
        session::Kind::CompactContext,
        ConfiguredToolDefinition::new("Compact current context", r#"{"type":"object"}"#),
        services,
        templates(),
    ));
    let planner = Arc::new(CompactionPlanner::new(
        CompactionSettings {
            trigger_percent: 95,
            target_percent: 30,
            maximum_input_tokens: 60000,
            summary_output_tokens: 1024,
            summary_instructions: String::new(),
            summary_prefix: String::new(),
        },
        ModelProfiles::new(&BTreeMap::new()),
    ));
    let adapter =
        Arc::new(RetainedCompactTool::from_session(tool.clone(), planner, templates()).unwrap());
    (adapter, control, tool)
}
fn compact_step() -> Step {
    let mut step = Step::complete("request retained compact");
    step.completion.as_mut().unwrap().tool_calls =
        vec![call("retained-compact", "compact_context")];
    step
}

#[tokio::test]
async fn retained_session_tool_reaches_real_inline_context_and_reuses_identity() {
    let fixture = Fixture::new();
    fixture.seed().await;
    let (adapter, control, retained) = bridge();
    assert_eq!(
        Arc::strong_count(&retained),
        2,
        "adapter must retain the same Tool"
    );
    let provider = Scripted::new(vec![
        compact_step(),
        Step::complete("retained durable summary"),
        Step::complete("answer after compaction"),
        Step::complete("next answer"),
    ]);
    let (session, factory) = runtime::runtime(
        &fixture,
        &provider,
        vec![ToolHandle::Borrowed(adapter.clone())],
    )
    .await;
    let session = Arc::new(session);
    control.bind_session(Arc::downgrade(&session)).unwrap();
    for _ in 0..2 {
        let sent = runtime::send(&session).await;
        assert_eq!(
            bounded(sent.execution.join()).await.unwrap().status,
            ExecutionStatus::Succeeded
        );
    }
    assert_eq!(factory.creates.load(Ordering::SeqCst), 1);
    assert_eq!(
        Arc::strong_count(&retained),
        2,
        "no per-invocation Tool recreated"
    );
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 4);
    assert!(
        requests[2]
            .messages
            .iter()
            .any(|message| message.content().contains("retained durable summary"))
    );
    assert!(
        !requests[2]
            .messages
            .iter()
            .any(|message| message.content().starts_with("seed-0-"))
    );
    let result = requests[2]
        .messages
        .iter()
        .find(|message| message.tool_call_id == "retained-compact")
        .unwrap();
    assert_eq!(result.role, sessions::Role::Tool);
    assert!(!result.content().contains("error:"));
    assert!(result.content().contains("Context"));
    drop(requests);
    let events = fixture.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_compaction_finished())
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
    assert_eq!(
        events.iter().filter(|event| event.has_turn_ended()).count(),
        2
    );
    assert!(
        fixture
            .history
            .compaction_history()
            .await
            .unwrap()
            .is_some()
    );
    bounded(session.close()).await.unwrap();
}

#[tokio::test]
async fn frozen_control_external_compaction_waits_behind_active_drain_and_joins() {
    let fixture = Fixture::new();
    fixture.seed().await;
    let (adapter, control, _) = bridge();
    let gate = Gate::new();
    let mut first = Step::complete("first answer");
    first.gate = Some(gate.clone());
    let provider = Scripted::new(vec![first, Step::complete("external durable summary")]);
    let (session, _) =
        runtime::runtime(&fixture, &provider, vec![ToolHandle::Borrowed(adapter)]).await;
    let session = Arc::new(session);
    control.bind_session(Arc::downgrade(&session)).unwrap();
    assert!(control.bind_session(Arc::downgrade(&session)).is_err());
    let sent = runtime::send(&session).await;
    gate.entered().await;
    let request = CompactRequest::external(None, Arc::new(NeverCancelled));
    let compact = SessionControl::compact(control.as_ref(), request, &NeverCancelled);
    tokio::pin!(compact);
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut compact)
            .await
            .is_err()
    );
    assert_eq!(
        provider.requests.lock().unwrap().len(),
        1,
        "external compaction must not overlap active provider/drain"
    );
    gate.open();
    let compacted = bounded(compact).await.unwrap();
    assert!(compacted.reduced);
    bounded(sent.execution.join()).await.unwrap();
    bounded(session.close()).await.unwrap();
    let summary_instructions = templates()
        .render("compaction.summary-instructions", &[], &NeverCancelled)
        .unwrap();
    assert!(
        provider.requests.lock().unwrap()[1]
            .messages
            .iter()
            .any(|message| message.role == sessions::Role::System
                && message.content() == summary_instructions)
    );
    assert!(
        fixture
            .history
            .compaction_history()
            .await
            .unwrap()
            .unwrap()
            .snapshot
            .summary
            .contains("external durable summary")
    );
    assert!(
        SessionControl::compact(
            control.as_ref(),
            CompactRequest::external(None, Arc::new(NeverCancelled)),
            &NeverCancelled
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn retained_inline_cancellation_shutdown_joins_real_provider_work() {
    let fixture = Fixture::new();
    fixture.seed().await;
    let (adapter, control, _) = bridge();
    let gate = Gate::new();
    let mut summary = Step::complete("must cancel");
    summary.gate = Some(gate.clone());
    let provider = Scripted::new(vec![compact_step(), summary]);
    let (session, _) =
        runtime::runtime(&fixture, &provider, vec![ToolHandle::Borrowed(adapter)]).await;
    let session = Arc::new(session);
    control.bind_session(Arc::downgrade(&session)).unwrap();
    let sent = runtime::send(&session).await;
    gate.entered().await;
    bounded(session.close()).await.unwrap();
    let _ = bounded(sent.execution.join()).await;
    let events = fixture.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_compaction_failed())
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_tool_cancelled())
            .count(),
        1
    );
    assert!(provider.closes.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn frozen_control_precancel_is_nonmutating_and_ancillary_methods_delegate() {
    let fixture = Fixture::new();
    fixture.seed().await;
    let (adapter, control, _) = bridge();
    let provider = Scripted::default();
    let (session, _) =
        runtime::runtime(&fixture, &provider, vec![ToolHandle::Borrowed(adapter)]).await;
    let session = Arc::new(session);
    control.bind_session(Arc::downgrade(&session)).unwrap();
    let cancelled = Arc::new(Signal::new());
    cancelled.cancel();
    assert!(matches!(
        SessionControl::compact(
            control.as_ref(),
            CompactRequest::external(None, cancelled.clone()),
            cancelled.as_ref()
        )
        .await,
        Err(OperationError::Cancelled)
    ));
    assert!(provider.requests.lock().unwrap().is_empty());
    assert_eq!(
        SessionControl::checkpoint(control.as_ref(), "checkpoint".into(), &NeverCancelled)
            .await
            .unwrap(),
        "checkpoint"
    );
    assert_eq!(
        SessionControl::set_reminder(control.as_ref(), "set".into()).unwrap(),
        "set"
    );
    assert_eq!(
        SessionControl::clear_reminder(control.as_ref(), "clear".into()).unwrap(),
        "clear"
    );
    assert_eq!(
        SessionControl::status(control.as_ref()).unwrap(),
        "ancillary status"
    );
    bounded(session.close()).await.unwrap();
}

#[tokio::test]
async fn retained_result_fixedpoint_matches_actual_controller_history_estimate() {
    let fixture = Fixture::new();
    fixture.seed().await;
    let sequence = fixture
        .batch(vec![call("fixedpoint", "compact_context")])
        .await;
    let (adapter, _, _) = bridge();
    let mut harness = Harness::new(&fixture, vec![ToolHandle::Borrowed(adapter.clone())]).await;
    let provider = Scripted::new(vec![Step::complete("fixedpoint summary")]);
    harness.selection = selection(&provider);
    let definitions = harness.snapshot.definitions().to_vec();
    let output = bounded(adapter.invoke(
        ToolInvocation::new("fixedpoint", "{}", sequence),
        BorrowedToolContext {
            selection: &harness.selection,
            definitions: &definitions,
            resources: TurnResources {
                history: &fixture.history,
                conversation: &mut harness.conversation,
                context: &mut harness.context,
                prompt: &harness.prompt,
                sessions: &harness.sessions,
                status: &Status,
                events: &harness.events,
                usage: &harness.usage,
            },
        },
        &NeverCancelled,
    ))
    .await
    .unwrap();
    let estimated = harness
        .context
        .estimate_context_after_tool_result(
            EpochInput {
                selection: &harness.selection.model,
                tools: &definitions,
                conversation: &mut harness.conversation,
                prompt: &harness.prompt,
                sessions: &harness.sessions,
                status: &Status,
                events: &harness.events,
            },
            "fixedpoint",
            &output.text,
        )
        .await
        .unwrap();
    assert!(estimated.is_available());
    let arguments = [
        (
            "estimated_tokens".into(),
            estimated.estimated_tokens.to_string().into(),
        ),
        (
            "context_limit".into(),
            estimated.context_limit.to_string().into(),
        ),
        (
            "notification_interval".into(),
            ContextCadence::NOTIFICATION_INTERVAL.to_string().into(),
        ),
        (
            "trigger".into(),
            estimated.trigger_percent.to_string().into(),
        ),
        (
            "usage".into(),
            estimated.usage_percent.unwrap().to_string().into(),
        ),
    ];
    let expected = templates()
        .render(
            "compact-context-tool.compacted",
            &arguments,
            &NeverCancelled,
        )
        .unwrap();
    assert_eq!(
        output.text, expected,
        "retained Tool result must reach the real controller's token fixed point"
    );
}

// The retained status tool must use the activity+context registry, not the full
// observation (generated time, selection and profile) used for prompt injection.
use parrot_domain::values::accounting::AgentSessionStatistics;
use parrot_runtime::active_work::status::{RuntimeStatus, StatusError, StatusQuery};
struct StatusAncillary {
    other: Ancillary,
    runtime: Arc<RuntimeStatus>,
    statistics: AgentSessionStatistics,
    observed: Mutex<Vec<ContextSnapshot>>,
    prompt_probe: Option<Arc<SessionPrompt>>,
}
fn status_query(selected: &ContextSelection<Security>) -> StatusQuery {
    StatusQuery {
        session_id: "agent".into(),
        parent_session_id: String::new(),
        parent_session_name: String::new(),
        profile: selected.turn.profile.id.clone(),
        requested_model: selected.turn.model.requested_selector.value.clone(),
    }
}
fn status_error(error: OperationError<StatusError>) -> OperationError<Failure> {
    match error {
        OperationError::Cancelled => OperationError::Cancelled,
        OperationError::Failed(error) => OperationError::Failed(Failure::new(
            session::FailureKind::InvalidOperation,
            error.to_string(),
        )),
    }
}
impl SessionServices for StatusAncillary {
    type Security = SharedSecurity<Security>;
    type Selection = ContextSelection<Security>;
    fn checkpoint<'a>(
        &'a self,
        title: &'a str,
        sequence: i64,
        call: &'a str,
    ) -> BoxFuture<'a, Result<(), Failure>> {
        SessionServices::checkpoint(&self.other, title, sequence, call)
    }
    fn set_reminder<'a>(
        &'a self,
        title: &'a str,
        description: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<Failure>>> {
        SessionServices::set_reminder(&self.other, title, description, cancellation)
    }
    fn clear_reminder<'a>(
        &'a self,
        title: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, OperationError<Failure>>> {
        SessionServices::clear_reminder(&self.other, title, cancellation)
    }
    fn reminder_titles(&self) -> BoxFuture<'_, Vec<String>> {
        self.other.reminder_titles()
    }
    fn runtime_status<'a>(
        &'a self,
        selection: &'a Self::Selection,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<String, OperationError<Failure>>> {
        Box::pin(async move {
            let snapshot = selection.estimate_context(cancellation).await?;
            if let Some(prompt) = &self.prompt_probe {
                assert_eq!(
                    bounded(prompt.build(&selection.turn.model, cancellation))
                        .await
                        .unwrap(),
                    "actual status epoch instructions",
                    "inline estimation must release the shared prompt gate before status observation"
                );
            }
            self.observed.lock().unwrap().push(snapshot.clone());
            self.runtime
                .observe_runtime(&status_query(selection), snapshot, cancellation)
                .await
                .map_err(status_error)
        })
    }
    fn statistics_status<'a>(
        &'a self,
        selection: &'a Self::Selection,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<String, OperationError<Failure>>> {
        Box::pin(async move {
            self.runtime
                .observe_statistics(
                    &status_query(selection),
                    self.statistics.capture(),
                    cancellation,
                )
                .await
                .map_err(status_error)
        })
    }
    fn compact<'a>(
        &'a self,
        selection: &'a Self::Selection,
        target: Option<ContextSize>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<session::Compaction, OperationError<Failure>>> {
        SessionServices::compact(&self.other, selection, target, cancellation)
    }
    fn estimate_after_tool_result(
        &self,
        selection: &Self::Selection,
        call: &str,
        result: &str,
    ) -> Result<ContextSnapshot, Failure> {
        self.other
            .estimate_after_tool_result(selection, call, result)
    }
}
type StatusBridge = (
    Arc<RetainedCompactTool<Security>>,
    Arc<StatusAncillary>,
    Arc<session::SessionTool<ContextServices<Security, StatusAncillary, Ancillary>>>,
);
fn status_bridge() -> StatusBridge {
    status_bridge_with_prompt(None)
}
fn status_bridge_with_prompt(prompt_probe: Option<Arc<SessionPrompt>>) -> StatusBridge {
    let ancillary = Arc::new(StatusAncillary {
        other: Ancillary,
        runtime: Arc::new(RuntimeStatus::new(templates(), Arc::new(TestClock), vec![]).unwrap()),
        statistics: AgentSessionStatistics::default(),
        observed: Mutex::new(vec![]),
        prompt_probe,
    });
    let control = Arc::new(ContextControl::new(Arc::new(Ancillary)));
    let services = Arc::new(ContextServices::new(ancillary.clone(), control));
    let tool = Arc::new(session::SessionTool::new(
        session::Kind::Status,
        ConfiguredToolDefinition::new("Observe runtime status", r#"{"type":"object"}"#),
        services,
        templates(),
    ));
    let planner = Arc::new(CompactionPlanner::new(
        CompactionSettings {
            trigger_percent: 95,
            target_percent: 30,
            maximum_input_tokens: 60000,
            summary_output_tokens: 1024,
            summary_instructions: String::new(),
            summary_prefix: String::new(),
        },
        ModelProfiles::new(&BTreeMap::new()),
    ));
    let adapter = Arc::new(
        RetainedCompactTool::from_status_session(tool.clone(), planner, templates()).unwrap(),
    );
    (adapter, ancillary, tool)
}
fn status_step(call_id: &str) -> Step {
    let mut step = Step::complete("request retained status");
    step.completion.as_mut().unwrap().tool_calls = vec![call(call_id, "status")];
    step
}

#[tokio::test]
async fn retained_status_tool_uses_actual_activity_context_statistics_and_same_identity() {
    let fixture = Fixture::new();
    fixture.seed().await;
    let (adapter, ancillary, retained) = status_bridge();
    let provider = Scripted::new(vec![
        status_step("retained-status-1"),
        Step::complete("status answer"),
        status_step("retained-status-2"),
        Step::complete("second status answer"),
    ]);
    let (session, factory) =
        runtime::runtime(&fixture, &provider, vec![ToolHandle::Borrowed(adapter)]).await;
    for _ in 0..2 {
        let sent = runtime::send(&session).await;
        assert_eq!(
            bounded(sent.execution.join()).await.unwrap().status,
            ExecutionStatus::Succeeded
        );
    }
    assert_eq!(factory.creates.load(Ordering::SeqCst), 1);
    assert_eq!(Arc::strong_count(&retained), 2);
    let observations = ancillary.observed.lock().unwrap().clone();
    assert_eq!(observations.len(), 2);
    assert!(observations[1].estimated_tokens > observations[0].estimated_tokens);
    let selection = ContextSelection::external(selection(&provider));
    let query = status_query(&selection);
    let statistics = ancillary
        .runtime
        .observe_statistics(&query, ancillary.statistics.capture(), &NeverCancelled)
        .await
        .unwrap();
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 4);
    for (request, snapshot, call_id) in [
        (&requests[1], &observations[0], "retained-status-1"),
        (&requests[3], &observations[1], "retained-status-2"),
    ] {
        let runtime = ancillary
            .runtime
            .observe_runtime(&query, snapshot.clone(), &NeverCancelled)
            .await
            .unwrap();
        let expected = templates()
            .render(
                "tool-result.text",
                &[("value".into(), format!("{runtime}\n\n{statistics}").into())],
                &NeverCancelled,
            )
            .unwrap();
        let result = request
            .messages
            .iter()
            .find(|message| message.role == sessions::Role::Tool && message.tool_call_id == call_id)
            .unwrap();
        assert_eq!(result.content(), expected);
        let full = ancillary
            .runtime
            .observe_with_context(
                &query,
                "PROFILE MUST NOT APPEAR IN STATUS TOOL",
                snapshot.clone(),
                &NeverCancelled,
            )
            .await
            .unwrap();
        assert_ne!(
            runtime, full,
            "status Tool is activity+context, not full prompt observation"
        );
        assert!(
            !result
                .content()
                .contains("PROFILE MUST NOT APPEAR IN STATUS TOOL")
        );
    }
    assert!(
        !fixture
            .events()
            .await
            .iter()
            .any(|event| event.has_compaction_started())
    );
    bounded(session.close()).await.unwrap();
}

struct CountingPrompt(Arc<AtomicUsize>);
impl SystemPrompt for CountingPrompt {
    fn renew_epoch(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
    fn build(
        &mut self,
        _: &parrot_provider::routing::aliases::ResolvedModelSelection,
    ) -> PromptResult<String> {
        assert_eq!(
            self.0.load(Ordering::SeqCst),
            1,
            "status must observe the current sampled epoch"
        );
        Ok("actual status epoch instructions".into())
    }
}

#[tokio::test]
async fn retained_status_observes_current_borrowed_history_tools_and_epoch_without_resampling() {
    let fixture = Fixture::new();
    fixture.seed().await;
    let samples = Arc::new(AtomicUsize::new(0));
    let prompt = Arc::new(SessionPrompt::new(Box::new(CountingPrompt(
        samples.clone(),
    ))));
    let (adapter, ancillary, _) = status_bridge_with_prompt(Some(prompt.clone()));
    let mut harness = Harness::new(&fixture, vec![ToolHandle::Borrowed(adapter.clone())]).await;
    let definitions = harness.snapshot.definitions().to_vec();
    for index in 0..2 {
        let output = bounded(adapter.invoke(
            ToolInvocation::new("direct-status", "{}", index),
            BorrowedToolContext {
                selection: &harness.selection,
                definitions: &definitions,
                resources: TurnResources {
                    history: &fixture.history,
                    conversation: &mut harness.conversation,
                    context: &mut harness.context,
                    prompt: &prompt,
                    sessions: &harness.sessions,
                    status: &Status,
                    events: &harness.events,
                    usage: &harness.usage,
                },
            },
            &NeverCancelled,
        ))
        .await
        .unwrap();
        let actual = harness
            .context
            .estimate_context(
                EpochInput {
                    selection: &harness.selection.model,
                    tools: &definitions,
                    conversation: &mut harness.conversation,
                    prompt: &prompt,
                    sessions: &harness.sessions,
                    status: &Status,
                    events: &harness.events,
                },
                &NeverCancelled,
            )
            .await
            .unwrap();
        assert_eq!(ancillary.observed.lock().unwrap()[index as usize], actual);
        assert!(output.text.contains("Context"));
        let mut event = Event::new();
        event.id = id();
        event.agent_session_id = "agent".into();
        fixture
            .history
            .append_conversation(
                event,
                ConversationMessage::text(
                    MessageRole::User,
                    "new live conversation content ".repeat(50),
                ),
            )
            .await
            .unwrap();
        harness.conversation.reload().await.unwrap();
    }
    assert_eq!(samples.load(Ordering::SeqCst), 1);
    let observed = ancillary.observed.lock().unwrap();
    assert!(observed[1].estimated_tokens > observed[0].estimated_tokens);
}

#[tokio::test]
async fn retained_status_precancel_does_not_estimate_and_external_selection_has_no_inline_route() {
    let fixture = Fixture::new();
    let (adapter, ancillary, _) = status_bridge();
    let mut harness = Harness::new(&fixture, vec![ToolHandle::Borrowed(adapter.clone())]).await;
    let definitions = harness.snapshot.definitions().to_vec();
    let cancelled = Signal::new();
    cancelled.cancel();
    let result = adapter
        .invoke(
            ToolInvocation::new("cancel-status", "{}", 0),
            BorrowedToolContext {
                selection: &harness.selection,
                definitions: &definitions,
                resources: TurnResources {
                    history: &fixture.history,
                    conversation: &mut harness.conversation,
                    context: &mut harness.context,
                    prompt: &harness.prompt,
                    sessions: &harness.sessions,
                    status: &Status,
                    events: &harness.events,
                    usage: &harness.usage,
                },
            },
            &cancelled,
        )
        .await;
    assert!(matches!(result, Err(OperationError::Cancelled)));
    assert!(ancillary.observed.lock().unwrap().is_empty());
    let external = ContextSelection::external(harness.selection.clone());
    let error = external
        .estimate_context(&NeverCancelled)
        .await
        .unwrap_err();
    assert!(matches!(error, OperationError::Failed(_)));
}

struct CancelAfterCompactionPrompt {
    history: AgentHistoryView,
    cancelled_builds: Arc<AtomicUsize>,
}
impl SystemPrompt for CancelAfterCompactionPrompt {
    fn renew_epoch(&mut self) {}
    fn build(
        &mut self,
        _: &parrot_provider::routing::aliases::ResolvedModelSelection,
    ) -> PromptResult<String> {
        Ok("actual post-compaction cancellation instructions".into())
    }
    fn build_live<'a>(
        &'a mut self,
        selection: &'a parrot_provider::routing::aliases::ResolvedModelSelection,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<String>> {
        Box::pin(async move {
            let events = self.history.repository().events().replay().await.unwrap();
            if events.iter().any(|event| event.has_compaction_finished()) {
                self.cancelled_builds.fetch_add(1, Ordering::SeqCst);
                return Err(parrot_runtime::prompt_context::PromptError::Cancelled);
            }
            self.build(selection)
        })
    }
}

#[tokio::test]
async fn retained_compact_preserves_live_prompt_cancellation_after_successful_compaction() {
    let fixture = Fixture::new();
    fixture.seed().await;
    let sequence = fixture
        .batch(vec![call("cancel-build", "compact_context")])
        .await;
    let (adapter, _, _) = bridge();
    let mut harness = Harness::new(&fixture, vec![ToolHandle::Borrowed(adapter.clone())]).await;
    let provider = Scripted::new(vec![Step::complete("actual completed summary")]);
    harness.selection = selection(&provider);
    let definitions = harness.snapshot.definitions().to_vec();
    let cancelled_builds = Arc::new(AtomicUsize::new(0));
    let prompt = SessionPrompt::new(Box::new(CancelAfterCompactionPrompt {
        history: fixture.history.clone(),
        cancelled_builds: cancelled_builds.clone(),
    }));
    let result = bounded(adapter.invoke(
        ToolInvocation::new("cancel-build", "{}", sequence),
        BorrowedToolContext {
            selection: &harness.selection,
            definitions: &definitions,
            resources: TurnResources {
                history: &fixture.history,
                conversation: &mut harness.conversation,
                context: &mut harness.context,
                prompt: &prompt,
                sessions: &harness.sessions,
                status: &Status,
                events: &harness.events,
                usage: &harness.usage,
            },
        },
        &NeverCancelled,
    ))
    .await;
    assert!(matches!(result, Err(OperationError::Cancelled)));
    assert_eq!(cancelled_builds.load(Ordering::SeqCst), 1);
    let events = fixture.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_compaction_finished())
            .count(),
        1
    );
    assert!(!events.iter().any(|event| event.has_compaction_failed()));
}
