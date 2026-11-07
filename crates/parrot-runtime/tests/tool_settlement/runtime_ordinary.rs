use super::*;
use parrot_runtime::agent_turn::lifecycle::ExecutionStatus;

#[tokio::test]
async fn actual_runtime_invokes_retained_domain_tool_alias_and_next_provider_sees_settled_result() {
    let fixture = Fixture::new();
    let mut tool = RecordingTool::new(&fixture, "canonical", ParallelSafety::Sequential);
    tool.metadata.aliases = vec!["alias".into()];
    let tool = Arc::new(tool);
    let mut first = Step::complete("tool request");
    first.completion.as_mut().unwrap().tool_calls = vec![call("ordinary", "alias")];
    let provider = Scripted::new(vec![
        first,
        Step::complete("answer using result"),
        Step::complete("next turn answer"),
    ]);
    let (session, factory) =
        runtime::runtime(&fixture, &provider, vec![tool.clone().handle()]).await;
    for _ in 0..2 {
        let sent = runtime::send(&session).await;
        assert_eq!(
            bounded(sent.execution.join()).await.unwrap().status,
            ExecutionStatus::Succeeded
        );
    }
    assert_eq!(factory.creates.load(Ordering::SeqCst), 1);
    assert_eq!(tool.calls.lock().unwrap().len(), 1);
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3);
    assert!(requests[1].messages.iter().any(
        |message| message.tool_call_id == "ordinary" && message.content() == "result:canonical"
    ));
    assert_eq!(requests[0].tools[0].name, "canonical");
    let events = fixture.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_tool_finished())
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .find(|event| event.has_tool_finished())
            .unwrap()
            .tool_finished()
            .tool_name,
        "alias"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_tool_execution_started())
            .count(),
        1
    );
    bounded(session.close()).await.unwrap();
}

#[tokio::test]
async fn actual_runtime_executes_whole_mixed_batch_with_parallel_and_sequential_barriers() {
    let fixture = Fixture::new();
    let mut handles = Vec::new();
    let mut tools = Vec::new();
    let mut gates = Vec::new();
    for (name, safety) in [
        ("a", ParallelSafety::ParallelSafe),
        ("b", ParallelSafety::ParallelSafe),
        ("serial", ParallelSafety::Sequential),
        ("tail", ParallelSafety::ParallelSafe),
    ] {
        let gate = Gate::new();
        let mut tool = RecordingTool::new(&fixture, name, safety);
        tool.gate = Some(gate.clone());
        let tool = Arc::new(tool);
        handles.push(tool.clone().handle());
        tools.push(tool);
        gates.push(gate);
    }
    let mut first = Step::complete("mixed batch");
    first.completion.as_mut().unwrap().tool_calls = ["a", "b", "serial", "tail"]
        .into_iter()
        .map(|name| call(name, name))
        .collect();
    let provider = Scripted::new(vec![first, Step::complete("answer")]);
    let (session, _) = runtime::runtime(&fixture, &provider, handles).await;
    let sent = runtime::send(&session).await;
    gates[0].entered().await;
    gates[1].entered().await;
    assert!(tools[2].calls.lock().unwrap().is_empty());
    assert!(tools[3].calls.lock().unwrap().is_empty());
    assert!(settlement::terminal_ids(&fixture.events().await).is_empty());
    gates[1].open();
    gates[0].open();
    gates[2].entered().await;
    assert_eq!(
        settlement::terminal_ids(&fixture.events().await),
        ["a", "b"]
    );
    assert!(tools[3].calls.lock().unwrap().is_empty());
    gates[2].open();
    gates[3].entered().await;
    assert_eq!(
        settlement::terminal_ids(&fixture.events().await),
        ["a", "b", "serial"]
    );
    gates[3].open();
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    assert_eq!(
        settlement::terminal_ids(&fixture.events().await),
        ["a", "b", "serial", "tail"]
    );
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1]
            .messages
            .iter()
            .filter(|message| !message.tool_call_id.is_empty())
            .map(|message| message.tool_call_id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b", "serial", "tail"]
    );
    let history = fixture.history.conversation().await.unwrap();
    let sequence = history
        .iter()
        .find(|item| item.message.tool_calls.len() == 4)
        .unwrap()
        .sequence;
    let events = fixture.events().await;
    let usages: Vec<_> = events
        .iter()
        .filter(|event| event.has_tool_execution_started())
        .map(|event| event.tool_execution_started())
        .collect();
    assert_eq!(usages.len(), 4);
    assert!(
        usages
            .iter()
            .all(|usage| usage.assistant_sequence == sequence)
    );
    for tool in tools {
        assert_eq!(tool.calls.lock().unwrap().len(), 1);
    }
    bounded(session.close()).await.unwrap();
}

#[tokio::test]
async fn actual_runtime_tool_cancellation_skips_the_remaining_whole_batch() {
    let fixture = Fixture::new();
    let mut cancel = RecordingTool::new(&fixture, "cancel", ParallelSafety::Sequential);
    cancel.cancel = true;
    let cancel = Arc::new(cancel);
    let tail = Arc::new(RecordingTool::new(
        &fixture,
        "tail",
        ParallelSafety::ParallelSafe,
    ));
    let mut first = Step::complete("cancel batch");
    first.completion.as_mut().unwrap().tool_calls =
        vec![call("cancel", "cancel"), call("tail", "tail")];
    let provider = Scripted::new(vec![first, Step::complete("answer")]);
    let (session, _) = runtime::runtime(
        &fixture,
        &provider,
        vec![cancel.clone().handle(), tail.clone().handle()],
    )
    .await;
    let sent = runtime::send(&session).await;
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    assert_eq!(cancel.calls.lock().unwrap().len(), 1);
    assert!(tail.calls.lock().unwrap().is_empty());
    let events = fixture.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_tool_cancelled())
            .map(|event| event.tool_cancelled().tool_call_id.as_str())
            .collect::<Vec<_>>(),
        ["cancel", "tail"]
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    bounded(session.close()).await.unwrap();
}

#[tokio::test]
async fn actual_runtime_terminal_publication_failure_stops_tail_and_restart_never_replays() {
    let fixture = Fixture::new();
    let first = Arc::new(RecordingTool::new(
        &fixture,
        "first",
        ParallelSafety::Sequential,
    ));
    let tail = Arc::new(RecordingTool::new(
        &fixture,
        "tail",
        ParallelSafety::Sequential,
    ));
    let mut request = Step::complete("tool batch");
    request.completion.as_mut().unwrap().tool_calls =
        vec![call("first", "first"), call("tail", "tail")];
    let provider = Scripted::new(vec![request]);
    let publisher = Arc::new(Publisher::new(fixture.history.clone()));
    publisher.fail_terminal.store(1, Ordering::SeqCst);
    let handles = vec![first.clone().handle(), tail.clone().handle()];
    let (session, _) =
        runtime::runtime_with_publisher(&fixture, &provider, handles.clone(), publisher).await;
    let sent = runtime::send(&session).await;
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Failed
    );
    assert_eq!(first.calls.lock().unwrap().len(), 1);
    assert!(tail.calls.lock().unwrap().is_empty());
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    let saved = fixture
        .history
        .tool_terminals_of_calls(vec!["first".into(), "tail".into()])
        .await
        .unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].tool_call_id, "first");
    assert_eq!(saved[0].status, ToolExecutionStatus::Finished);
    assert_eq!(saved[0].message, "result:first");
    bounded(session.close()).await.unwrap();

    let repository = Fixture::open(&fixture.path);
    let reopened = Fixture {
        path: fixture.path.clone(),
        history: Fixture::history_for(&fixture.path, &repository),
        repository,
        fresh_batches: Mutex::new(VecDeque::new()),
    };
    let resumed_provider = Scripted::new(vec![Step::complete("answer after restart")]);
    let (resumed, _) = runtime::runtime(&reopened, &resumed_provider, handles).await;
    let sent = runtime::send(&resumed).await;
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    assert_eq!(first.calls.lock().unwrap().len(), 1);
    assert!(tail.calls.lock().unwrap().is_empty());
    let restored = reopened
        .history
        .tool_terminals_of_calls(vec!["first".into(), "tail".into()])
        .await
        .unwrap();
    assert_eq!(restored.len(), 2);
    assert_eq!(restored[0], saved[0]);
    assert_eq!(restored[1].tool_call_id, "tail");
    assert_eq!(restored[1].status, ToolExecutionStatus::Error);
    assert!(restored[1].message.contains(CRASH_MESSAGE));
    let requests = resumed_provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].messages.iter().any(|message| {
            message.tool_call_id == "first" && message.content() == "result:first"
        })
    );
    assert!(requests[0].messages.iter().any(|message| {
        message.tool_call_id == "tail" && message.content().contains(CRASH_MESSAGE)
    }));
    assert_eq!(
        reopened
            .events()
            .await
            .iter()
            .filter(|event| event.has_tool_started())
            .count(),
        1
    );
    bounded(resumed.close()).await.unwrap();
}
