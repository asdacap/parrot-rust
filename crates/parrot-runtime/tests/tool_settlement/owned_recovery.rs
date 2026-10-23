use super::*;
use parrot_domain::{contracts::agent_state::AgentStateStore, values::agent_state::*};
use parrot_foundation::paths::AgentScratchDirectory;
use parrot_runtime::agent_turn::lifecycle::{AgentSession, Execution};
use parrot_storage::agent_state::FileAgentStateStore;

struct CounterTool {
    recording: RecordingTool,
    effects: Arc<AtomicUsize>,
}
impl Tool for CounterTool {
    type Definition = ToolMetadata;
    type Invocation = ToolInvocation;
    type Selection = TurnSelection<Security>;
    type Security = SharedSecurity<Security>;
    type Output = ToolOutput;
    type Error = ToolFailure;
    fn definition(&self) -> &ToolMetadata {
        self.recording.definition()
    }
    fn parallel_safety(&self, invocation: &ToolInvocation) -> ParallelSafety {
        self.recording.parallel_safety(invocation)
    }
    fn enabled_after_interruption(&self) -> bool {
        self.recording.enabled_after_interruption()
    }
    fn invoke<'a>(
        &'a self,
        invocation: ToolInvocation,
        context: InvocationContext<'a, SharedSecurity<Security>, TurnSelection<Security>>,
    ) -> BoxFuture<'a, Result<ToolOutput, OperationError<ToolFailure>>> {
        Box::pin(async move {
            self.effects.fetch_add(1, Ordering::SeqCst);
            self.recording.invoke(invocation, context).await
        })
    }
}

#[tokio::test]
async fn uncommitted_tool_effect_can_repeat_but_committed_terminal_survives_reopening() {
    let fixture = Arc::new(Fixture::new());
    fixture.batch(vec![call("effect", "counter")]).await;
    let effects = Arc::new(AtomicUsize::new(0));
    let gate = Gate::new();
    let mut recording = RecordingTool::new(&fixture, "counter", ParallelSafety::Sequential);
    recording.gate = Some(gate.clone());
    let tool = Arc::new(CounterTool {
        recording,
        effects: effects.clone(),
    });
    let mut harness = Harness::new(&fixture, vec![ToolHandle::Ordinary(tool.clone())]).await;
    let task_fixture = fixture.clone();
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(async move { harness.run(&task_fixture, &NeverCancelled).await });
    gate.entered().await;
    tasks.abort_all();
    assert!(
        bounded(tasks.join_next())
            .await
            .unwrap()
            .unwrap_err()
            .is_cancelled()
    );
    assert!(tasks.is_empty());
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert!(tool.recording.completed.lock().unwrap().is_empty());
    assert!(
        fixture
            .history
            .tool_terminals_of_calls(vec!["effect".into()])
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        fixture
            .events()
            .await
            .iter()
            .filter(|event| event.has_tool_started())
            .count(),
        1
    );
    drop(tool);
    fixture
        .repository
        .events()
        .database()
        .close()
        .await
        .unwrap();

    let repository = Fixture::open(&fixture.path);
    let restarted = Fixture {
        path: fixture.path.clone(),
        history: Fixture::history_for(&fixture.path, &repository),
        repository,
    };
    let tool = Arc::new(CounterTool {
        recording: RecordingTool::new(&restarted, "counter", ParallelSafety::Sequential),
        effects: effects.clone(),
    });
    let mut harness = Harness::new(&restarted, vec![ToolHandle::Ordinary(tool.clone())]).await;
    bounded(harness.run(&restarted, &NeverCancelled))
        .await
        .unwrap();
    assert_eq!(effects.load(Ordering::SeqCst), 2);
    assert_eq!(*tool.recording.completed.lock().unwrap(), ["effect"]);
    let terminals = restarted
        .history
        .tool_terminals_of_calls(vec!["effect".into()])
        .await
        .unwrap();
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0].status, ToolExecutionStatus::Finished);
    assert_eq!(terminals[0].message, "result:counter");
    drop(harness);
    drop(tool);
    restarted
        .repository
        .events()
        .database()
        .close()
        .await
        .unwrap();

    let repository = Fixture::open(&fixture.path);
    let recovered = Fixture {
        path: fixture.path.clone(),
        history: Fixture::history_for(&fixture.path, &repository),
        repository,
    };
    let tool = Arc::new(CounterTool {
        recording: RecordingTool::new(&recovered, "counter", ParallelSafety::Sequential),
        effects: effects.clone(),
    });
    let mut harness = Harness::new(&recovered, vec![ToolHandle::Ordinary(tool.clone())]).await;
    bounded(harness.run(&recovered, &NeverCancelled))
        .await
        .unwrap();
    assert_eq!(effects.load(Ordering::SeqCst), 2);
    assert!(tool.recording.calls.lock().unwrap().is_empty());
    assert_eq!(
        recovered
            .history
            .conversation()
            .await
            .unwrap()
            .iter()
            .filter(|item| item.message.role == MessageRole::Tool)
            .count(),
        1
    );
}

#[tokio::test]
async fn owned_recovery_restores_settled_tool_and_invokes_only_unresolved_call() {
    let fixture = Fixture::new();
    let (seed, _) = runtime::runtime(&fixture, &Scripted::default(), vec![]).await;
    seed.record(
        vec![ConversationPart::text("original question")],
        "original-input".into(),
        protocol::Delivery::DELIVERY_STEER,
        Arc::new(NeverCancelled),
    )
    .await
    .unwrap();
    fixture
        .history
        .promote_steers(|input| {
            let mut event = Event {
                id: id(),
                agent_session_id: "agent".into(),
                ..Event::new()
            };
            event.set_input_promoted(protocol::InputPromoted {
                input_id: input.id.clone(),
                message_id: input.message_id.clone(),
                ..Default::default()
            });
            Ok(event)
        })
        .await
        .unwrap();
    seed.close().await.unwrap();
    fixture
        .batch(vec![
            call("settled-owned", "read"),
            call("pending-owned", "read"),
        ])
        .await;
    let mut event = Event {
        id: id(),
        agent_session_id: "agent".into(),
        ..Event::new()
    };
    event.set_tool_finished(protocol::ToolFinished {
        tool_call_id: "settled-owned".into(),
        tool_name: "read".into(),
        result: Some("saved first result".into()),
        ..Default::default()
    });
    fixture
        .repository
        .events()
        .append_tool_terminal(
            event,
            ToolExecutionTerminal {
                tool_call_id: "settled-owned".into(),
                tool_name: "read".into(),
                status: ToolExecutionStatus::Finished,
                message: "saved first result".into(),
                result_parts: vec![ConversationPart::text("saved first result")],
            },
        )
        .await
        .unwrap();
    let agent_directory = fixture.path.join("agent-state");
    AgentScratchDirectory::new(&agent_directory, &fixture.path).unwrap();
    let store = Arc::new(
        FileAgentStateStore::open(&agent_directory, "agent".into())
            .await
            .unwrap(),
    );
    let mut snapshot = AgentStateSnapshot::new(
        "agent".into(),
        AgentDescriptor {
            parent_session_id: None,
            name: "test".into(),
            local_scope: String::new(),
            selection: SelectionSnapshot {
                profile_id: "mode".into(),
                model_selector: "scripted/model".into(),
            },
            completion_delivery: CompletionDeliverySnapshot::RetainedOnly,
        },
    );
    snapshot.execution = Some(ExecutionCheckpoint {
        result_claim: None,
        execution_id: "owned-tool-execution".into(),
        initial_message_id: "original-input".into(),
        initial_parts: vec![ExecutionInputPart::Text {
            text: "original question".into(),
        }],
        history_watermark: 0,
        phase: ExecutionPhase::Active,
        terminal: None,
        parent_notification: Some(NotificationCheckpoint {
            message_id: "owned-tool-execution.parent".into(),
            admitted: false,
            delivery_error: None,
        }),
    });
    store.initialize(snapshot).await.unwrap();
    fixture
        .repository
        .events()
        .database()
        .close()
        .await
        .unwrap();
    let repository = Fixture::open(&fixture.path);
    let history = Fixture::history_for(&fixture.path, &repository);
    let restarted = Fixture {
        path: fixture.path.clone(),
        repository,
        history,
    };
    let tool = Arc::new(RecordingTool::new(
        &restarted,
        "read",
        ParallelSafety::Sequential,
    ));
    let provider = Scripted::new(vec![Step::complete("answer using recovered tools")]);
    let (legacy, _) = runtime::runtime(&restarted, &provider, vec![tool.clone().handle()]).await;
    let session =
        AgentSession::with_state(legacy.backend().clone(), Arc::new(Signal::new()), store);
    assert_eq!(
        bounded(session.recover_owned().unwrap().join())
            .await
            .unwrap(),
        Execution::succeeded("answer using recovered tools")
    );
    assert_eq!(
        bounded(session.recover_owned().unwrap().join())
            .await
            .unwrap(),
        Execution::succeeded("answer using recovered tools")
    );
    assert_eq!(
        tool.calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| call.id.as_str())
            .collect::<Vec<_>>(),
        ["pending-owned"]
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    let messages = restarted.history.conversation().await.unwrap();
    assert_eq!(
        messages
            .iter()
            .filter(|item| item.message.role == MessageRole::Tool)
            .count(),
        2
    );
    assert_eq!(
        messages
            .iter()
            .filter(|item| item.message.role == MessageRole::User)
            .count(),
        1
    );
    assert_eq!(
        restarted
            .history
            .tool_terminals_of_calls(vec!["settled-owned".into(), "pending-owned".into()])
            .await
            .unwrap()
            .len(),
        2
    );
    session.close().await.unwrap();
}
