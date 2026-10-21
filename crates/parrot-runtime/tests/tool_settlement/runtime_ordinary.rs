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
