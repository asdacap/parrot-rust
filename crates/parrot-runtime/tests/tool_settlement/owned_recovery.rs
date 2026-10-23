use super::*;
use parrot_domain::{contracts::agent_state::AgentStateStore, values::agent_state::*};
use parrot_foundation::paths::AgentScratchDirectory;
use parrot_runtime::agent_turn::lifecycle::{AgentSession, Execution};
use parrot_storage::agent_state::FileAgentStateStore;

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
