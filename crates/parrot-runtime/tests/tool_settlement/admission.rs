use super::*;

struct BoundaryPublisher {
    inner: Arc<Publisher>,
    cancel_on_first: Option<Arc<Signal>>,
    second_started: Option<Arc<Semaphore>>,
    first_finished: Option<Arc<Semaphore>>,
}
impl EventPublisher for BoundaryPublisher {
    type Event = Event;
    type Error = BrokerError;
    fn publish_committed<'a>(
        &'a self,
        event: &'a Committed<Event>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<BrokerError>>> {
        Box::pin(async move {
            if let Some(signal) = self.cancel_on_first.as_ref().filter(|_| {
                event.value().has_tool_started()
                    && event.value().tool_started().tool_call_id == "first"
            }) {
                signal.cancel();
            }
            self.inner.publish_committed(event, cancellation).await?;
            if let Some(second_started) = self.second_started.as_ref().filter(|_| {
                event.value().has_tool_started()
                    && event.value().tool_started().tool_call_id == "later"
            }) {
                second_started.add_permits(1);
                self.first_finished
                    .as_ref()
                    .unwrap()
                    .acquire()
                    .await
                    .unwrap()
                    .forget();
            }
            Ok(())
        })
    }
    fn publish_transient<'a>(
        &'a self,
        event: Event,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<BrokerError>>> {
        self.inner.publish_transient(event, cancellation)
    }
}
fn install_publisher(fixture: &Fixture, harness: &mut Harness, publisher: Arc<ToolPublisher>) {
    harness.tools = Arc::new(
        ToolSettlement::new(ToolSettlementConfig {
            factory: harness.factory.clone(),
            templates: templates(),
            output_blobs: Arc::new(
                ToolOutputBlobStore::for_session(
                    fixture.repository.events().database().selected_root(),
                    "scratch",
                    Arc::new(Names),
                )
                .unwrap(),
            ),
            publisher,
            activity: harness.activity.clone(),
            diagnostics: harness.diagnostics.clone(),
            image_bytes_per_cycle: 1024 * 1024,
        })
        .unwrap(),
    );
}
async fn assert_only_first_invoked(fixture: &Fixture, tool: &RecordingTool, harness: &Harness) {
    let events = fixture.events().await;
    let starts = events
        .iter()
        .filter(|event| event.has_tool_started())
        .map(|event| event.tool_started().tool_call_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(starts, vec!["first"]);
    {
        let calls = tool.calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .map(|call| call.id.as_str())
                .collect::<Vec<_>>(),
            vec!["first"]
        );
        assert_eq!(calls[0].profile, "original-profile");
        assert_eq!(calls[0].requested, "scripted/model");
        assert_eq!(calls[0].security, 1);
        assert_eq!(calls[0].starts, 1);
        assert_eq!(calls[0].usages, 1);
    }
    let terminals = fixture
        .history
        .tool_terminals_of_calls(vec!["first".into(), "later".into()])
        .await
        .unwrap();
    assert_eq!(
        terminals
            .iter()
            .map(|terminal| (terminal.tool_call_id.as_str(), terminal.status))
            .collect::<Vec<_>>(),
        vec![
            ("first", ToolExecutionStatus::Finished),
            ("later", ToolExecutionStatus::Cancelled)
        ]
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_tool_execution_started())
            .count(),
        1
    );
    assert!(harness.activity.capture().current_tool.is_none());
    let results = fixture
        .history
        .conversation()
        .await
        .unwrap()
        .into_iter()
        .filter(|item| item.message.role == MessageRole::Tool)
        .map(|item| item.message.tool_call_id)
        .collect::<Vec<_>>();
    assert_eq!(results, vec!["first", "later"]);
}

#[tokio::test]
async fn cancellation_at_first_started_still_invokes_first_but_does_not_admit_later_safe_call() {
    let fixture = Fixture::new();
    fixture
        .batch(vec![call("first", "read"), call("later", "read")])
        .await;
    let tool = Arc::new(RecordingTool::new(
        &fixture,
        "read",
        ParallelSafety::ParallelSafe,
    ));
    let mut harness = Harness::new(&fixture, vec![tool.clone().handle()]).await;
    let signal = Arc::new(Signal::new());
    let publisher = Arc::new(BoundaryPublisher {
        inner: harness.publisher.clone(),
        cancel_on_first: Some(signal.clone()),
        second_started: None,
        first_finished: None,
    });
    install_publisher(&fixture, &mut harness, publisher);
    bounded(harness.run(&fixture, signal.as_ref()))
        .await
        .unwrap();
    assert_only_first_invoked(&fixture, &tool, &harness).await;
}

struct CancelDuringInvoke {
    inner: RecordingTool,
    signal: Arc<Signal>,
}
impl Tool for CancelDuringInvoke {
    type Definition = ToolMetadata;
    type Invocation = ToolInvocation;
    type Selection = TurnSelection<Security>;
    type Security = SharedSecurity<Security>;
    type Output = ToolOutput;
    type Error = ToolFailure;
    fn definition(&self) -> &ToolMetadata {
        self.inner.definition()
    }
    fn parallel_safety(&self, invocation: &ToolInvocation) -> ParallelSafety {
        self.inner.parallel_safety(invocation)
    }
    fn enabled_after_interruption(&self) -> bool {
        true
    }
    fn invoke<'a>(
        &'a self,
        invocation: ToolInvocation,
        context: InvocationContext<'a, SharedSecurity<Security>, TurnSelection<Security>>,
    ) -> BoxFuture<'a, Result<ToolOutput, OperationError<ToolFailure>>> {
        // Cancellation here occurs synchronously, before the returned future is polled.
        self.signal.cancel();
        self.inner.invoke(invocation, context)
    }
}
#[tokio::test]
async fn synchronous_tool_invoke_cancellation_is_observed_before_later_admission() {
    let fixture = Fixture::new();
    fixture
        .batch(vec![call("first", "read"), call("later", "read")])
        .await;
    let signal = Arc::new(Signal::new());
    let tool = Arc::new(CancelDuringInvoke {
        inner: RecordingTool::new(&fixture, "read", ParallelSafety::ParallelSafe),
        signal: signal.clone(),
    });
    let mut harness = Harness::new(&fixture, vec![ToolHandle::Ordinary(tool.clone())]).await;
    bounded(harness.run(&fixture, signal.as_ref()))
        .await
        .unwrap();
    assert_only_first_invoked(&fixture, &tool.inner, &harness).await;
}

struct PreludePeer {
    inner: RecordingTool,
    second_started: Arc<Semaphore>,
    first_finished: Arc<Semaphore>,
}
impl Tool for PreludePeer {
    type Definition = ToolMetadata;
    type Invocation = ToolInvocation;
    type Selection = TurnSelection<Security>;
    type Security = SharedSecurity<Security>;
    type Output = ToolOutput;
    type Error = ToolFailure;
    fn definition(&self) -> &ToolMetadata {
        self.inner.definition()
    }
    fn parallel_safety(&self, invocation: &ToolInvocation) -> ParallelSafety {
        self.inner.parallel_safety(invocation)
    }
    fn enabled_after_interruption(&self) -> bool {
        true
    }
    fn invoke<'a>(
        &'a self,
        invocation: ToolInvocation,
        context: InvocationContext<'a, SharedSecurity<Security>, TurnSelection<Security>>,
    ) -> BoxFuture<'a, Result<ToolOutput, OperationError<ToolFailure>>> {
        Box::pin(async move {
            if invocation.call_id == "first" {
                self.second_started.acquire().await.unwrap().forget();
                let result = self.inner.invoke(invocation, context).await;
                self.first_finished.add_permits(1);
                result
            } else {
                self.inner.invoke(invocation, context).await
            }
        })
    }
}
#[tokio::test]
async fn earlier_started_peer_progresses_while_next_started_publication_waits_for_it() {
    let fixture = Fixture::new();
    fixture
        .batch(vec![call("first", "read"), call("later", "read")])
        .await;
    let second_started = Arc::new(Semaphore::new(0));
    let first_finished = Arc::new(Semaphore::new(0));
    let tool = Arc::new(PreludePeer {
        inner: RecordingTool::new(&fixture, "read", ParallelSafety::ParallelSafe),
        second_started: second_started.clone(),
        first_finished: first_finished.clone(),
    });
    let mut harness = Harness::new(&fixture, vec![ToolHandle::Ordinary(tool.clone())]).await;
    let publisher = Arc::new(BoundaryPublisher {
        inner: harness.publisher.clone(),
        cancel_on_first: None,
        second_started: Some(second_started),
        first_finished: Some(first_finished),
    });
    install_publisher(&fixture, &mut harness, publisher);
    bounded(harness.run(&fixture, &NeverCancelled))
        .await
        .unwrap();
    assert_eq!(
        tool.inner
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| call.id.as_str())
            .collect::<Vec<_>>(),
        vec!["first", "later"]
    );
    let events = fixture.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_tool_finished())
            .map(|event| event.tool_finished().tool_call_id.as_str())
            .collect::<Vec<_>>(),
        vec!["first", "later"]
    );
    assert!(
        events
            .iter()
            .filter(|event| event.has_tool_cancelled())
            .count()
            == 0
    );
}
