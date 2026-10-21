use super::*;

#[tokio::test]
async fn live_context_completes_while_actual_borrowed_tool_holds_inline_summary() {
    let fixture = Fixture::new();
    fixture.seed().await;
    let compact = Arc::new(CompactTool::new());
    let gate = Gate::new();
    let mut summary = Step::complete("live tool summary");
    summary.gate = Some(gate.clone());
    let provider = Scripted::new(vec![compact_step(), summary, Step::complete("answer")]);
    let (session, _) = runtime(
        &fixture,
        &provider,
        vec![ToolHandle::Borrowed(compact.clone())],
    )
    .await;
    let sent = send(&session).await;
    gate.entered().await;
    assert_eq!(compact.calls.load(Ordering::SeqCst), 1);
    assert!(!sent.execution.is_finished());
    let observed = bounded(session.backend().observe_context(&NeverCancelled))
        .await
        .unwrap();
    assert!(observed.context.estimated_tokens > 0);
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    gate.open();
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    let after = bounded(session.backend().observe_context(&NeverCancelled))
        .await
        .unwrap();
    assert!(after.context.estimated_tokens < observed.context.estimated_tokens);
    bounded(session.close()).await.unwrap();
}
