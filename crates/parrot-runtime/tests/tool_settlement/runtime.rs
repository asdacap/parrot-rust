include!("runtime_support.rs");
use parrot_runtime::agent_turn::lifecycle::ExecutionStatus;

#[tokio::test]
async fn actual_runtime_retains_tool_factory_and_inline_actual_t45_state_reaches_next_provider() {
    let fixture = Fixture::new();
    fixture.seed().await;
    let compact = Arc::new(CompactTool::new());
    let provider = Scripted::new(vec![
        compact_step(),
        Step::complete("inline durable summary"),
        Step::complete("answer using inline summary"),
        Step::complete("next turn answer"),
    ]);
    let (session, factory) = runtime(
        &fixture,
        &provider,
        vec![ToolHandle::Borrowed(compact.clone())],
    )
    .await;
    let sent = send(&session).await;
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    let next = send(&session).await;
    assert_eq!(
        bounded(next.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    assert_eq!(compact.calls.load(Ordering::SeqCst), 1);
    assert_eq!(factory.creates.load(Ordering::SeqCst), 1);
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 4);
    assert!(
        requests[2]
            .messages
            .iter()
            .any(|message| message.content().contains("inline durable summary"))
    );
    assert!(
        !requests[2]
            .messages
            .iter()
            .any(|message| message.content().starts_with("seed-0-"))
    );
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
            .filter(|event| event.has_turn_started())
            .count(),
        2
    );
    assert_eq!(
        events.iter().filter(|event| event.has_turn_ended()).count(),
        2
    );
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
            .filter(|event| event.has_tool_execution_started())
            .count(),
        1
    );
    assert!(
        fixture
            .history
            .compaction_history()
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        fixture
            .jsonl()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|entry| entry["type"] == "compaction")
            .count(),
        1
    );
    bounded(session.close()).await.unwrap();
}

#[tokio::test]
async fn actual_runtime_interrupt_joins_inline_actual_t45_without_self_queue_or_second_drain() {
    let fixture = Fixture::new();
    fixture.seed().await;
    let compact = Arc::new(CompactTool::new());
    let gate = Gate::new();
    let mut summary = Step::complete("not committed");
    summary.gate = Some(gate.clone());
    let provider = Scripted::new(vec![compact_step(), summary]);
    let (session, _) = runtime(&fixture, &provider, vec![ToolHandle::Borrowed(compact)]).await;
    let sent = send(&session).await;
    gate.entered().await;
    bounded(session.interrupt(&NeverCancelled)).await.unwrap();
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Canceled
    );
    assert!(
        fixture
            .history
            .compaction_history()
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    assert_eq!(provider.closes.load(Ordering::SeqCst), 2);
    let events = fixture.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_turn_started())
            .count(),
        1
    );
    assert_eq!(
        events.iter().filter(|event| event.has_turn_ended()).count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_tool_cancelled())
            .count(),
        1
    );
    bounded(session.close()).await.unwrap();
}

#[path = "live_observation.rs"]
mod live_observation;

impl Fixture {
    pub async fn seed(&self) {
        for index in 0..6 {
            for role in [MessageRole::User, MessageRole::Assistant] {
                let mut event = Event::new();
                event.id = id();
                event.agent_session_id = "agent".into();
                self.history
                    .append_conversation(
                        event,
                        ConversationMessage::text(
                            role,
                            format!("seed-{index}-{}", "x ".repeat(200)),
                        ),
                    )
                    .await
                    .unwrap();
            }
        }
    }
}

struct CompactTool {
    metadata: ToolMetadata,
    calls: AtomicUsize,
}
impl CompactTool {
    fn new() -> Self {
        Self {
            metadata: ToolMetadata {
                name: "compact_context".into(),
                aliases: vec![],
            },
            calls: AtomicUsize::new(0),
        }
    }
}
impl BorrowedTool<Security> for CompactTool {
    fn definition(&self) -> &ToolMetadata {
        &self.metadata
    }
    fn enabled_after_interruption(&self) -> bool {
        false
    }
    fn invoke<'a>(
        &'a self,
        _: ToolInvocation,
        context: BorrowedToolContext<'a, Security>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ToolOutput, OperationError<ToolFailure>>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let resources = context.resources;
            let result = resources
                .context
                .compact(
                    EpochInput {
                        selection: &context.selection.model,
                        tools: context.definitions,
                        conversation: resources.conversation,
                        agent_state: resources.agent_state,
                        prompt: resources.prompt,
                        sessions: resources.sessions,
                        status: resources.status,
                        events: resources.events,
                    },
                    None,
                    cancellation,
                )
                .await
                .map_err(|error| {
                    if matches!(
                        error,
                        ContextError::Cancelled
                            | ContextError::Compaction(
                                CompactionError::Cancelled
                                    | CompactionError::Provider(Error::Cancelled)
                            )
                    ) {
                        OperationError::Cancelled
                    } else {
                        OperationError::Failed(ToolFailure::new(
                            error.to_string(),
                            FailureKind::InvalidOperation,
                        ))
                    }
                })?;
            assert!(result.reduced);
            Ok(ToolOutput::text("compacted inline"))
        })
    }
}

fn compact_step() -> Step {
    let mut step = Step::complete("invoke actual compact");
    step.completion.as_mut().unwrap().tool_calls = vec![call("inline-compact", "compact_context")];
    step
}
