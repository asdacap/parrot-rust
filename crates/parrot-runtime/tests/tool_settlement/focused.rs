use super::*;

struct DynamicTool {
    inner: RecordingTool,
    checks: Mutex<Vec<(String, String, i64)>>,
    gates: BTreeMap<String, Arc<Gate>>,
}
impl Tool for DynamicTool {
    type Definition = ToolMetadata;
    type Invocation = ToolInvocation;
    type Selection = TurnSelection<Security>;
    type Security = SharedSecurity<Security>;
    type Output = ToolOutput;
    type Error = ToolFailure;
    fn definition(&self) -> &ToolMetadata {
        self.inner.definition()
    }
    fn enabled_after_interruption(&self) -> bool {
        true
    }
    fn parallel_safety(&self, invocation: &ToolInvocation) -> ParallelSafety {
        self.checks.lock().unwrap().push((
            invocation.call_id.clone(),
            invocation.arguments_json.clone(),
            invocation.assistant_sequence,
        ));
        if invocation.arguments_json == "{\"serial\":true}" {
            ParallelSafety::Sequential
        } else {
            ParallelSafety::ParallelSafe
        }
    }
    fn invoke<'a>(
        &'a self,
        invocation: ToolInvocation,
        context: InvocationContext<'a, SharedSecurity<Security>, TurnSelection<Security>>,
    ) -> BoxFuture<'a, Result<ToolOutput, OperationError<ToolFailure>>> {
        Box::pin(async move {
            self.gates[&invocation.call_id]
                .wait(context.cancellation())
                .await
                .map_err(|_| OperationError::Cancelled)?;
            self.inner.invoke(invocation, context).await
        })
    }
}
#[tokio::test]
async fn invocation_dependent_safety_receives_original_call_arguments_id_and_assistant_sequence() {
    let fixture = Fixture::new();
    let first = Gate::new();
    let serial = Gate::new();
    let last = Gate::new();
    let tool = Arc::new(DynamicTool {
        inner: RecordingTool::new(&fixture, "dynamic", ParallelSafety::ParallelSafe),
        checks: Mutex::new(vec![]),
        gates: BTreeMap::from([
            ("first".into(), first.clone()),
            ("serial".into(), serial.clone()),
            ("last".into(), last.clone()),
        ]),
    });
    let mut serial_call = call("serial", "dynamic");
    serial_call.arguments_json = "{\"serial\":true}".into();
    let sequence = fixture
        .batch(vec![
            call("first", "dynamic"),
            serial_call,
            call("last", "dynamic"),
        ])
        .await;
    let erased: Arc<RuntimeTool<Security>> = tool.clone();
    let mut harness = Harness::new(&fixture, vec![ToolHandle::Ordinary(erased)]).await;
    let control = async {
        first.entered().await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(serial.entered.available_permits(), 0);
        assert_eq!(last.entered.available_permits(), 0);
        first.open();
        serial.entered().await;
        assert_eq!(last.entered.available_permits(), 0);
        assert_eq!(
            fixture
                .events()
                .await
                .iter()
                .filter(|event| event.has_tool_finished())
                .count(),
            1
        );
        serial.open();
        last.entered().await;
        assert_eq!(
            fixture
                .events()
                .await
                .iter()
                .filter(|event| event.has_tool_finished())
                .count(),
            2
        );
        last.open();
    };
    let (result, ()) = tokio::join!(harness.run(&fixture, &NeverCancelled), control);
    result.unwrap();
    let checks = tool.checks.lock().unwrap();
    assert!(
        checks
            .iter()
            .all(|(_, _, assistant)| *assistant == sequence)
    );
    for (id, args) in [
        ("first", "{}"),
        ("serial", "{\"serial\":true}"),
        ("last", "{}"),
    ] {
        assert!(
            checks
                .iter()
                .any(|(actual_id, actual_args, _)| actual_id == id && actual_args == args)
        );
    }
}

#[tokio::test]
async fn selection_capture_reads_tool_started_from_committed_sqlite_before_usage_or_invoke() {
    struct DurableCapture {
        database: SessionDatabase,
        selection: TurnSelection<Security>,
        captures: AtomicUsize,
    }
    impl SelectionSource<Security> for DurableCapture {
        fn capture(&self) -> Result<TurnSelection<Security>, TurnError> {
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            // This read-only worker transaction observes the already-committed start.
            // No mutation is left owned by the synchronous selection boundary.
            let ticket = self
                .database
                .admit("capture-observation", &NeverCancelled, move |transaction| {
                    let payload: Vec<u8> = transaction.query_row(
                        "SELECT payload FROM event ORDER BY sequence DESC LIMIT 1",
                        [],
                        |row| row.get(0),
                    )?;
                    tx.send(payload).unwrap();
                    Ok(())
                })
                .unwrap();
            let payload = rx.recv_timeout(Duration::from_secs(2)).unwrap();
            drop(ticket);
            let event = <Event as protobuf::Message>::parse_from_bytes(&payload).unwrap();
            assert!(
                event.has_tool_started(),
                "capture ran before durable start or after usage"
            );
            assert_eq!(event.tool_started().tool_call_id, "capture");
            self.captures.fetch_add(1, Ordering::SeqCst);
            Ok(self.selection.clone())
        }
        fn update(&self, _: TurnSelection<Security>) -> Result<(), TurnError> {
            panic!("not used")
        }
    }
    let fixture = Fixture::new();
    fixture.batch(vec![call("capture", "known")]).await;
    let tool = Arc::new(RecordingTool::new(
        &fixture,
        "known",
        ParallelSafety::Sequential,
    ));
    let mut harness = Harness::new(&fixture, vec![tool.clone().handle()]).await;
    let capture = DurableCapture {
        database: fixture.repository.events().database().clone(),
        selection: harness.selection.clone(),
        captures: AtomicUsize::new(0),
    };
    harness
        .tools
        .reconcile(
            TurnResources {
                history: &fixture.history,
                conversation: &mut harness.conversation,
                context: &mut harness.context,
                prompt: &harness.prompt,
                sessions: &harness.sessions,
                status: &Status,
                events: &harness.events,
                usage: &harness.usage,
            },
            &harness.selection,
            &harness.snapshot,
            &capture,
            &NeverCancelled,
        )
        .await
        .unwrap();
    assert_eq!(capture.captures.load(Ordering::SeqCst), 1);
    let calls = tool.calls.lock().unwrap();
    assert_eq!(calls[0].starts, 1);
    assert_eq!(calls[0].usages, 1);
}

#[tokio::test]
async fn restored_cancelled_terminal_stops_pending_tail_without_reinvocation_or_start() {
    let fixture = Fixture::new();
    let sequence = fixture
        .batch(vec![call("cancelled", "safe"), call("tail", "safe")])
        .await;
    let terminal = ToolExecutionTerminal {
        tool_call_id: "cancelled".into(),
        tool_name: "safe".into(),
        status: ToolExecutionStatus::Cancelled,
        result_parts: vec![ConversationPart::text("old interrupted")],
        message: "old interrupted".into(),
    };
    let mut event = Event::new();
    event.id = id();
    event.agent_session_id = "agent".into();
    fixture
        .repository
        .events()
        .append_tool_terminal(event, terminal)
        .await
        .unwrap();
    let tool = Arc::new(RecordingTool::new(
        &fixture,
        "safe",
        ParallelSafety::ParallelSafe,
    ));
    let mut harness = Harness::new(&fixture, vec![tool.clone().handle()]).await;
    harness.run(&fixture, &NeverCancelled).await.unwrap();
    assert!(tool.calls.lock().unwrap().is_empty());
    assert!(
        !fixture
            .events()
            .await
            .iter()
            .any(|event| event.has_tool_started())
    );
    let terminals = fixture
        .history
        .tool_terminals_of_calls(vec!["cancelled".into(), "tail".into()])
        .await
        .unwrap();
    assert!(
        terminals
            .iter()
            .all(|terminal| terminal.status == ToolExecutionStatus::Cancelled)
    );
    assert_eq!(
        fixture
            .history
            .conversation()
            .await
            .unwrap()
            .iter()
            .filter(|item| item.message.role == MessageRole::Tool)
            .count(),
        2
    );
    assert!(harness.tools.settled_conversation_sequence() >= sequence);
}

#[tokio::test]
async fn configured_actual_catalog_renders_success_error_and_interrupted_result() {
    let fixture = Fixture::new();
    let success = Arc::new(RecordingTool::new(
        &fixture,
        "success",
        ParallelSafety::Sequential,
    ));
    let mut failure = RecordingTool::new(&fixture, "failure", ParallelSafety::Sequential);
    failure.fail = true;
    let mut cancelled = RecordingTool::new(&fixture, "cancelled", ParallelSafety::Sequential);
    cancelled.cancel = true;
    fixture
        .batch(vec![
            call("success", "success"),
            call("failure", "failure"),
            call("cancelled", "cancelled"),
        ])
        .await;
    let environment = BTreeMap::from([
        ("HOME".into(), "/tmp/t47/home".into()),
        ("WORKDIR".into(), "/tmp/t47/work".into()),
    ]);
    let(_,catalog)=PromptTemplateCatalog::from_content("prompt_templates:\n  tool-result.text:\n    template: 'SUCCESS [{value}]'\n  tool-result.error:\n    template: 'ERROR [{message}]'\n  agent-session.interrupted-result:\n    template: 'INTERRUPTED configured'\n",&environment).unwrap();
    let mut harness = Harness::configured(
        &fixture,
        vec![
            success.handle(),
            Arc::new(failure).handle(),
            Arc::new(cancelled).handle(),
        ],
        1024,
        Arc::new(catalog),
    )
    .await;
    harness.run(&fixture, &NeverCancelled).await.unwrap();
    let terminals = fixture
        .history
        .tool_terminals_of_calls(vec!["success".into(), "failure".into(), "cancelled".into()])
        .await
        .unwrap();
    assert_eq!(
        terminals
            .iter()
            .map(|terminal| terminal.message.as_str())
            .collect::<Vec<_>>(),
        vec![
            "SUCCESS [result:success]",
            "ERROR [RAW_SECRET_TOOL_FAILURE]",
            "INTERRUPTED configured"
        ]
    );
}
