use super::*;

struct BudgetTool {
    metadata: ToolMetadata,
    bytes: i64,
    fail: bool,
    accepted: Arc<Mutex<Vec<(String, bool)>>>,
}

impl BudgetTool {
    fn new(name: &str, bytes: i64, fail: bool, accepted: Arc<Mutex<Vec<(String, bool)>>>) -> Self {
        Self {
            metadata: ToolMetadata {
                name: name.into(),
                aliases: vec![],
            },
            bytes,
            fail,
            accepted,
        }
    }

    fn into_handle(self) -> ToolHandle<Security> {
        ToolHandle::Ordinary(Arc::new(self))
    }
}

impl Tool for BudgetTool {
    type Definition = ToolMetadata;
    type Invocation = ToolInvocation;
    type Selection = TurnSelection<Security>;
    type Security = SharedSecurity<Security>;
    type Output = ToolOutput;
    type Error = ToolFailure;

    fn definition(&self) -> &ToolMetadata {
        &self.metadata
    }
    fn parallel_safety(&self, _: &ToolInvocation) -> ParallelSafety {
        ParallelSafety::Sequential
    }
    fn enabled_after_interruption(&self) -> bool {
        false
    }

    fn invoke<'a>(
        &'a self,
        invocation: ToolInvocation,
        _: InvocationContext<'a, SharedSecurity<Security>, TurnSelection<Security>>,
    ) -> BoxFuture<'a, Result<ToolOutput, OperationError<ToolFailure>>> {
        Box::pin(async move {
            let accepted = invocation
                .image_budget
                .as_ref()
                .unwrap()
                .try_accept(self.bytes)
                .unwrap();
            self.accepted
                .lock()
                .unwrap()
                .push((invocation.call_id, accepted));
            if self.fail {
                Err(OperationError::Failed(ToolFailure::new(
                    "failure after consuming image bytes",
                    FailureKind::Io,
                )))
            } else {
                Ok(ToolOutput::text(if accepted {
                    "accepted"
                } else {
                    "rejected"
                }))
            }
        })
    }
}

#[tokio::test]
async fn live_image_budget_survives_failed_chunks_and_resets_for_the_next_batch() {
    let fixture = Fixture::new();
    fixture
        .batch(vec![call("consume", "consume"), call("probe", "probe")])
        .await;
    let accepted = Arc::new(Mutex::new(Vec::new()));
    let mut harness = Harness::with_limit(
        &fixture,
        vec![
            BudgetTool::new("consume", 6, true, accepted.clone()).into_handle(),
            BudgetTool::new("probe", 5, false, accepted.clone()).into_handle(),
        ],
        10,
    )
    .await;
    harness.run(&fixture, &NeverCancelled).await.unwrap();
    assert_eq!(
        *accepted.lock().unwrap(),
        [("consume".into(), true), ("probe".into(), false)]
    );
    let terminals = fixture
        .history
        .tool_terminals_of_calls(vec!["consume".into()])
        .await
        .unwrap();
    assert_eq!(terminals[0].status, ToolExecutionStatus::Error);
    assert!(
        terminals[0]
            .result_parts
            .iter()
            .all(|part| !matches!(part, ConversationPart::ImageArtifact { .. }))
    );
    fixture.batch(vec![call("next", "probe")]).await;
    harness.run(&fixture, &NeverCancelled).await.unwrap();
    assert_eq!(
        *accepted.lock().unwrap(),
        [
            ("consume".into(), true),
            ("probe".into(), false),
            ("next".into(), true),
        ]
    );
}

#[tokio::test]
async fn retry_after_recording_failure_closes_unfinished_calls_without_spending_their_budget() {
    let fixture = Fixture::new();
    fixture
        .batch(vec![call("consume", "consume"), call("probe", "probe")])
        .await;
    let accepted = Arc::new(Mutex::new(Vec::new()));
    let mut harness = Harness::with_limit(
        &fixture,
        vec![
            BudgetTool::new("consume", 6, true, accepted.clone()).into_handle(),
            BudgetTool::new("probe", 5, false, accepted.clone()).into_handle(),
        ],
        10,
    )
    .await;
    harness.publisher.fail_terminal.store(1, Ordering::SeqCst);
    assert!(harness.run(&fixture, &NeverCancelled).await.is_err());
    assert_eq!(harness.state.settled_conversation_sequence(), 0);
    assert_eq!(*accepted.lock().unwrap(), [("consume".into(), true)]);
    harness.run(&fixture, &NeverCancelled).await.unwrap();
    assert_eq!(*accepted.lock().unwrap(), [("consume".into(), true)]);
    let terminals = fixture
        .history
        .tool_terminals_of_calls(vec!["consume".into(), "probe".into()])
        .await
        .unwrap();
    assert_eq!(terminals[0].status, ToolExecutionStatus::Error);
    assert!(
        terminals[0]
            .content()
            .contains("failure after consuming image bytes")
    );
    assert_eq!(terminals[1].status, ToolExecutionStatus::Error);
    assert!(terminals[1].content().contains(CRASH_MESSAGE));
    let events = fixture.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_tool_started())
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_tool_execution_started())
            .count(),
        1
    );
    assert!(harness.state.settled_conversation_sequence() > 0);
    assert_eq!(
        harness
            .publisher
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.has_tool_error())
            .count(),
        2
    );
    fixture.batch(vec![call("new-request", "probe")]).await;
    harness.run(&fixture, &NeverCancelled).await.unwrap();
    assert_eq!(
        *accepted.lock().unwrap(),
        [("consume".into(), true), ("new-request".into(), true)]
    );
}

#[tokio::test]
async fn actual_runtime_whole_batch_shares_image_budget_and_next_batch_resets_it() {
    let fixture = Fixture::new();
    let accepted = Arc::new(Mutex::new(Vec::new()));
    let mut first = Step::complete("consume image budget");
    first.completion.as_mut().unwrap().tool_calls =
        vec![call("consume", "consume"), call("probe", "probe")];
    let mut next = Step::complete("new batch");
    next.completion.as_mut().unwrap().tool_calls = vec![call("new-probe", "probe")];
    let provider = Scripted::new(vec![first, next, Step::complete("answer")]);
    let (session, _) = runtime::runtime(
        &fixture,
        &provider,
        vec![
            BudgetTool::new("consume", 600_000, true, accepted.clone()).into_handle(),
            BudgetTool::new("probe", 500_000, false, accepted.clone()).into_handle(),
        ],
    )
    .await;
    let sent = runtime::send(&session).await;
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        parrot_runtime::agent_turn::lifecycle::ExecutionStatus::Succeeded
    );
    assert_eq!(
        *accepted.lock().unwrap(),
        [
            ("consume".into(), true),
            ("probe".into(), false),
            ("new-probe".into(), true),
        ]
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 3);
    bounded(session.close()).await.unwrap();
}
