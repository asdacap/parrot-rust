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
async fn retry_after_recording_failure_rebuilds_the_budget_from_only_durable_results() {
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
    assert_eq!(
        *accepted.lock().unwrap(),
        [("consume".into(), true), ("probe".into(), true)]
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
        1
    );
}
