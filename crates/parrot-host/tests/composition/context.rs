//! The pre-existing T47 fixtures construct actual SQLite history, ContextController,
//! native tool execution and RuntimeBackend. Their fake provider has no network/credentials.
use parrot_runtime as runtime_crate;
use std::time::SystemTime;

#[allow(dead_code, unused_imports)]
#[path = "../../../parrot-runtime/tests/tool_settlement/fixture.rs"]
mod fixture;
use fixture::*;
#[allow(dead_code, unused_imports)]
#[path = "../../../parrot-runtime/tests/tool_settlement/common.rs"]
mod common;
use common::*;
#[path = "context_resources.rs"]
mod resources;
use resources::BorrowedContextResources;
#[path = "../../../parrot-runtime/tests/tool_settlement/runtime.rs"]
mod runtime;
use parrot_host::composition::context::{ContextSelection, ContextServices, RetainedCompactTool};
use parrot_runtime::{agent_turn::lifecycle::ExecutionStatus, prompt_context::SessionPrompt};
use parrot_tools::session::{self, Failure, SessionServices};

struct Ancillary;
impl SessionServices for Ancillary {
    type Security = SharedSecurity<Security>;
    type Selection = ContextSelection<Security>;
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
    fn update_progress<'a>(
        &'a self,
        _: parrot_domain::values::progress::ProgressUpdate,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, OperationError<Failure>>> {
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
}
type Bridge = (
    Arc<RetainedCompactTool<Security>>,
    Arc<session::SessionTool<ContextServices<Security, Ancillary>>>,
);
fn bridge() -> Bridge {
    let services = Arc::new(ContextServices::new(Arc::new(Ancillary)));
    let tool = Arc::new(session::SessionTool::new(
        session::Kind::CompactContext,
        ConfiguredToolDefinition::new("Compact current context", r#"{"type":"object"}"#),
        services,
    ));
    let adapter = Arc::new(RetainedCompactTool::from_session(tool.clone()).unwrap());
    (adapter, tool)
}
fn compact_step() -> Step {
    let mut step = Step::complete("request retained compact");
    step.completion.as_mut().unwrap().tool_calls =
        vec![call("retained-compact", "compact_context")];
    step
}

#[tokio::test]
async fn retained_session_tool_finishes_before_core_compaction_and_reuses_identity() {
    let fixture = Fixture::new();
    fixture.seed().await;
    let (adapter, retained) = bridge();
    let retained_tool = Arc::downgrade(&retained);
    drop(retained);
    assert!(retained_tool.upgrade().is_some());
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
    for _ in 0..2 {
        let sent = runtime::send(&session).await;
        assert_eq!(
            bounded(sent.execution.join()).await.unwrap().status,
            ExecutionStatus::Succeeded
        );
    }
    assert_eq!(factory.creates.load(Ordering::SeqCst), 1);
    assert!(retained_tool.upgrade().is_some());
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
    assert!(!result.content().is_empty());
    drop(requests);
    let events = fixture.events().await;
    assert!(
        events
            .iter()
            .position(|event| event.has_tool_finished())
            .unwrap()
            < events
                .iter()
                .position(|event| event.has_compaction_started())
                .unwrap()
    );
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
async fn core_compaction_cancellation_joins_real_provider_work_after_tool_finished() {
    let fixture = Fixture::new();
    fixture.seed().await;
    let (adapter, _) = bridge();
    let gate = Gate::new();
    let mut summary = Step::complete("must cancel");
    summary.gate = Some(gate.clone());
    let provider = Scripted::new(vec![compact_step(), summary]);
    let (session, _) =
        runtime::runtime(&fixture, &provider, vec![ToolHandle::Borrowed(adapter)]).await;
    let session = Arc::new(session);
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
            .filter(|event| event.has_tool_finished())
            .count(),
        1
    );
    assert!(provider.closes.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn retained_compact_invocation_only_acknowledges_without_mutating_context() {
    let fixture = Fixture::new();
    fixture.seed().await;
    let sequence = fixture.batch(vec![call("inert", "compact_context")]).await;
    let (adapter, _) = bridge();
    let mut harness =
        BorrowedContextResources::new(&fixture, vec![ToolHandle::Borrowed(adapter.clone())]).await;
    let provider = Scripted::default();
    harness.selection = selection(&provider);
    let before = fixture.history.conversation().await.unwrap();
    let definitions = harness.snapshot.definitions().to_vec();
    let output = bounded(adapter.invoke(
        ToolInvocation::new("inert", "{}", sequence, templates()),
        BorrowedToolContext {
            selection: &harness.selection,
            definitions: &definitions,
            resources: TurnResources {
                history: &fixture.history,
                conversation: &mut harness.conversation,
                agent_state: &mut harness.agent_state,
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
    assert!(!output.text.is_empty());
    assert!(provider.requests.lock().unwrap().is_empty());
    assert_eq!(fixture.history.conversation().await.unwrap(), before);
    assert!(
        fixture
            .history
            .compaction_history()
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !fixture
            .events()
            .await
            .iter()
            .any(|event| event.has_compaction_started())
    );
}

#[tokio::test]
async fn invalid_compact_target_returns_tool_error_without_compaction() {
    let fixture = Fixture::new();
    fixture.seed().await;
    let (adapter, _) = bridge();
    let mut requested = compact_step();
    requested.completion.as_mut().unwrap().tool_calls[0].arguments_json =
        r#"{"target_context_size":"invalid"}"#.into();
    let provider = Scripted::new(vec![requested, Step::complete("invalid target handled")]);
    let (session, _) =
        runtime::runtime(&fixture, &provider, vec![ToolHandle::Borrowed(adapter)]).await;
    let sent = runtime::send(&session).await;
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    let result = requests[1]
        .messages
        .iter()
        .find(|message| message.tool_call_id == "retained-compact")
        .unwrap();
    assert!(result.content().contains("error:"));
    drop(requests);
    assert!(
        !fixture
            .events()
            .await
            .iter()
            .any(|event| event.has_compaction_started())
    );
    bounded(session.close()).await.unwrap();
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
    fn update_progress<'a>(
        &'a self,
        update: parrot_domain::values::progress::ProgressUpdate,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, OperationError<Failure>>> {
        self.other.update_progress(update, cancellation)
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
}
type StatusBridge = (
    Arc<RetainedCompactTool<Security>>,
    Arc<StatusAncillary>,
    Arc<session::SessionTool<ContextServices<Security, StatusAncillary>>>,
);
fn status_bridge() -> StatusBridge {
    status_bridge_with_prompt(None)
}
fn status_bridge_with_prompt(prompt_probe: Option<Arc<SessionPrompt>>) -> StatusBridge {
    let ancillary = Arc::new(StatusAncillary {
        other: Ancillary,
        runtime: Arc::new(
            RuntimeStatus::new(
                templates(),
                Arc::new(TestClock),
                SystemTime::UNIX_EPOCH,
                vec![],
            )
            .unwrap(),
        ),
        statistics: AgentSessionStatistics::default(),
        observed: Mutex::new(vec![]),
        prompt_probe,
    });
    let services = Arc::new(ContextServices::new(ancillary.clone()));
    let tool = Arc::new(session::SessionTool::new(
        session::Kind::Status,
        ConfiguredToolDefinition::new("Observe runtime status", r#"{"type":"object"}"#),
        services,
    ));
    let adapter = Arc::new(RetainedCompactTool::from_status_session(tool.clone()).unwrap());
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
    let retained_tool = Arc::downgrade(&retained);
    drop(retained);
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
    assert!(retained_tool.upgrade().is_some());
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
    fn read_system_prompts(
        &mut self,
        _: &parrot_provider::routing::aliases::ResolvedModelSelection,
    ) -> PromptResult<Vec<(String, String)>> {
        assert_eq!(
            self.0.load(Ordering::SeqCst),
            1,
            "status must observe the current sampled epoch"
        );
        Ok(vec![(
            "test:instructions".into(),
            "actual status epoch instructions".into(),
        )])
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
    let mut harness =
        BorrowedContextResources::new(&fixture, vec![ToolHandle::Borrowed(adapter.clone())]).await;
    let definitions = harness.snapshot.definitions().to_vec();
    for index in 0..2 {
        let output = bounded(adapter.invoke(
            ToolInvocation::new("direct-status", "{}", index, templates()),
            BorrowedToolContext {
                selection: &harness.selection,
                definitions: &definitions,
                resources: TurnResources {
                    history: &fixture.history,
                    conversation: &mut harness.conversation,
                    agent_state: &mut harness.agent_state,
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
                    agent_state: &mut harness.agent_state,
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
    let mut harness =
        BorrowedContextResources::new(&fixture, vec![ToolHandle::Borrowed(adapter.clone())]).await;
    let definitions = harness.snapshot.definitions().to_vec();
    let cancelled = Signal::new();
    cancelled.cancel();
    let result = adapter
        .invoke(
            ToolInvocation::new("cancel-status", "{}", 0, templates()),
            BorrowedToolContext {
                selection: &harness.selection,
                definitions: &definitions,
                resources: TurnResources {
                    history: &fixture.history,
                    conversation: &mut harness.conversation,
                    agent_state: &mut harness.agent_state,
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
