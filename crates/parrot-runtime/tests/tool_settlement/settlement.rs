use super::*;

fn terminal_ids(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| {
            if event.has_tool_finished() {
                Some(event.tool_finished().tool_call_id.clone())
            } else if event.has_tool_error() {
                Some(event.tool_error().tool_call_id.clone())
            } else if event.has_tool_cancelled() {
                Some(event.tool_cancelled().tool_call_id.clone())
            } else {
                None
            }
        })
        .collect()
}
async fn terminals(fixture: &Fixture, ids: &[&str]) -> Vec<ToolExecutionTerminal> {
    fixture
        .history
        .tool_terminals_of_calls(ids.iter().map(|id| (*id).into()).collect())
        .await
        .unwrap()
}
fn gated(
    fixture: &Fixture,
    name: &str,
    safety: ParallelSafety,
    completed: &Arc<Mutex<Vec<String>>>,
) -> (Arc<RecordingTool>, Arc<Gate>) {
    let gate = Gate::new();
    let mut tool = RecordingTool::new(fixture, name, safety);
    tool.gate = Some(gate.clone());
    tool.completed = completed.clone();
    (Arc::new(tool), gate)
}
#[tokio::test]
async fn contiguous_parallel_starts_in_call_order_joins_all_before_settlement_and_respects_barriers()
 {
    let fixture = Fixture::new();
    let completed = Arc::new(Mutex::new(vec![]));
    let (a, ga) = gated(&fixture, "a", ParallelSafety::ParallelSafe, &completed);
    let (b, gb) = gated(&fixture, "b", ParallelSafety::ParallelSafe, &completed);
    let (c, gc) = gated(&fixture, "c", ParallelSafety::ParallelSafe, &completed);
    let (serial, gs) = gated(&fixture, "serial", ParallelSafety::Sequential, &completed);
    let (d, gd) = gated(&fixture, "d", ParallelSafety::ParallelSafe, &completed);
    let (e, ge) = gated(&fixture, "e", ParallelSafety::ParallelSafe, &completed);
    fixture
        .batch(vec![
            call("1", "a"),
            call("2", "b"),
            call("3", "c"),
            call("4", "serial"),
            call("5", "d"),
            call("6", "unknown"),
            call("7", "e"),
        ])
        .await;
    let mut harness = Harness::new(
        &fixture,
        vec![
            a.clone().handle(),
            b.clone().handle(),
            c.clone().handle(),
            serial.clone().handle(),
            d.clone().handle(),
            e.clone().handle(),
        ],
    )
    .await;
    for tool in [&a, &b, &c, &serial, &d, &e] {
        *tool.activity.lock().unwrap() = Some(harness.activity.clone());
    }
    let activity = harness.activity.clone();
    let control = async {
        ga.entered().await;
        gb.entered().await;
        gc.entered().await;
        let starts = fixture
            .events()
            .await
            .into_iter()
            .filter(|event| event.has_tool_started())
            .map(|event| event.tool_started().tool_call_id.clone())
            .collect::<Vec<_>>();
        assert_eq!(starts, vec!["1", "2", "3"]);
        assert_eq!(activity.capture().current_tool.as_deref(), Some("c"));
        gc.open();
        bounded(async {
            while !completed.lock().unwrap().contains(&"3".into()) {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            terminal_ids(&fixture.events().await).is_empty(),
            "no partial run settlement before all joined"
        );
        assert_eq!(activity.capture().current_tool.as_deref(), Some("b"));
        gb.open();
        bounded(async {
            while completed.lock().unwrap().len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(terminal_ids(&fixture.events().await).is_empty());
        ga.open();
        gs.entered().await;
        assert!(d.calls.lock().unwrap().is_empty());
        assert_eq!(terminal_ids(&fixture.events().await), vec!["1", "2", "3"]);
        gs.open();
        gd.entered().await;
        assert!(e.calls.lock().unwrap().is_empty());
        gd.open();
        ge.entered().await;
        assert_eq!(
            terminal_ids(&fixture.events().await),
            vec!["1", "2", "3", "4", "5", "6"]
        );
        ge.open();
    };
    let (result, ()) = tokio::join!(harness.run(&fixture, &NeverCancelled), control);
    result.unwrap();
    assert_eq!(&completed.lock().unwrap()[..3], &["3", "2", "1"]);
    assert_eq!(
        terminal_ids(&fixture.events().await),
        vec!["1", "2", "3", "4", "5", "6", "7"]
    );
    let results = fixture
        .history
        .conversation()
        .await
        .unwrap()
        .into_iter()
        .filter(|item| item.message.role == MessageRole::Tool)
        .map(|item| item.message.tool_call_id)
        .collect::<Vec<_>>();
    assert_eq!(results, vec!["1", "2", "3", "4", "5", "6", "7"]);
    assert!(activity.capture().current_tool.is_none());
    for tool in [&a, &b, &c, &serial, &d, &e] {
        let calls = tool.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].starts, 1);
        assert_eq!(calls[0].usages, 1);
    }
}

#[tokio::test]
async fn cancellation_joins_started_peers_keeps_successes_and_cancels_unadmitted_tail() {
    let fixture = Fixture::new();
    let completed = Arc::new(Mutex::new(vec![]));
    let (a, ga) = gated(&fixture, "a", ParallelSafety::ParallelSafe, &completed);
    let mut peer = RecordingTool::new(&fixture, "peer", ParallelSafety::ParallelSafe);
    let gp = Gate::new();
    peer.gate = Some(gp.clone());
    peer.ignore_cancel = true;
    let peer = Arc::new(peer);
    let tail = Arc::new(RecordingTool::new(
        &fixture,
        "tail",
        ParallelSafety::Sequential,
    ));
    fixture
        .batch(vec![
            call("cancelled", "a"),
            call("successful-peer", "peer"),
            call("tail", "tail"),
        ])
        .await;
    let mut harness = Harness::new(
        &fixture,
        vec![a.handle(), peer.clone().handle(), tail.clone().handle()],
    )
    .await;
    let cancellation = Signal::new();
    let control = async {
        ga.entered().await;
        gp.entered().await;
        cancellation.cancel();
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(
            terminal_ids(&fixture.events().await).is_empty(),
            "cancelled call cannot drop running peer"
        );
        gp.open();
    };
    let (result, ()) = tokio::join!(harness.run(&fixture, &cancellation), control);
    result.unwrap();
    let statuses = terminals(&fixture, &["cancelled", "successful-peer", "tail"])
        .await
        .into_iter()
        .map(|terminal| (terminal.tool_call_id, terminal.status))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(statuses["cancelled"], ToolExecutionStatus::Cancelled);
    assert_eq!(
        statuses["successful-peer"],
        ToolExecutionStatus::Finished,
        "do not blanket-normalize joined success"
    );
    assert_eq!(statuses["tail"], ToolExecutionStatus::Cancelled);
    assert!(tail.calls.lock().unwrap().is_empty());
    assert!(harness.activity.capture().current_tool.is_none());
    assert_eq!(
        fixture
            .events()
            .await
            .iter()
            .filter(|event| event.has_tool_started())
            .count(),
        2
    );
}

#[tokio::test]
async fn immutable_per_start_authority_refresh_preserves_original_turn_model_and_profile() {
    let fixture = Fixture::new();
    let completed = Arc::new(Mutex::new(vec![]));
    let (first, gate) = gated(&fixture, "first", ParallelSafety::Sequential, &completed);
    let second = Arc::new(RecordingTool::new(
        &fixture,
        "second",
        ParallelSafety::Sequential,
    ));
    fixture
        .batch(vec![call("first", "first"), call("second", "second")])
        .await;
    let mut harness = Harness::new(
        &fixture,
        vec![first.clone().handle(), second.clone().handle()],
    )
    .await;
    let control = async {
        gate.entered().await;
        let mut newer = harness.capture.capture().unwrap();
        newer.profile.id = "new-profile".into();
        newer.model.requested_selector.value = "new-model".into();
        newer.security = Arc::new(Security { version: 2 });
        harness.capture.update(newer).unwrap();
        gate.open();
    };
    // Borrow disjoint fields explicitly: the actual TurnTools adapter receives the live capture source.
    let resources = TurnResources {
        history: &fixture.history,
        conversation: &mut harness.conversation,
        context: &mut harness.context,
        prompt: &harness.prompt,
        sessions: &harness.sessions,
        status: &Status,
        events: &harness.events,
        usage: &harness.usage,
    };
    let (result, ()) = tokio::join!(
        harness.tools.reconcile(
            resources,
            &harness.selection,
            &harness.snapshot,
            &harness.capture,
            &NeverCancelled
        ),
        control
    );
    result.unwrap();
    assert_eq!(first.calls.lock().unwrap()[0].security, 1);
    assert_eq!(second.calls.lock().unwrap()[0].security, 2);
    for tool in [&first, &second] {
        let observation = tool.calls.lock().unwrap()[0].clone();
        assert_eq!(observation.profile, "original-profile");
        assert_eq!(observation.requested, "scripted/model");
    }
}

#[tokio::test]
async fn unknown_tool_has_durable_start_and_error_without_capture_usage_or_activity() {
    struct NoCapture;
    impl SelectionSource<Security> for NoCapture {
        fn capture(&self) -> Result<TurnSelection<Security>, TurnError> {
            panic!("unknown tool captured authority")
        }
        fn update(&self, _: TurnSelection<Security>) -> Result<(), TurnError> {
            panic!("unknown update")
        }
    }
    let fixture = Fixture::new();
    fixture.batch(vec![call("missing", "not_registered")]).await;
    let mut harness = Harness::new(&fixture, vec![]).await;
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
            &NoCapture,
            &NeverCancelled,
        )
        .await
        .unwrap();
    let events = fixture.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_tool_started())
            .count(),
        1
    );
    assert_eq!(
        events.iter().filter(|event| event.has_tool_error()).count(),
        1
    );
    assert!(
        !events
            .iter()
            .any(|event| event.has_tool_execution_started())
    );
    assert!(harness.activity.capture().current_tool.is_none());
    assert_eq!(
        terminals(&fixture, &["missing"]).await[0].status,
        ToolExecutionStatus::Error
    );
    let diagnostics = harness.diagnostics.0.lock().unwrap();
    assert!(diagnostics.iter().all(|event| event.tool_name.is_none()));
    assert_eq!(
        diagnostics.last().unwrap().outcome.as_deref(),
        Some("unknown_tool")
    );
}

#[tokio::test]
async fn durable_terminal_survives_publisher_failure_retry_without_reinvoke_or_republication() {
    let fixture = Fixture::new();
    fixture.batch(vec![call("once", "read")]).await;
    let tool = Arc::new(RecordingTool::new(
        &fixture,
        "read",
        ParallelSafety::Sequential,
    ));
    let mut harness = Harness::new(&fixture, vec![tool.clone().handle()]).await;
    harness.publisher.fail_terminal.store(1, Ordering::SeqCst);
    assert!(harness.run(&fixture, &NeverCancelled).await.is_err());
    assert_eq!(harness.tools.settled_conversation_sequence(), 0);
    assert_eq!(terminals(&fixture, &["once"]).await.len(), 1);
    harness.run(&fixture, &NeverCancelled).await.unwrap();
    harness.run(&fixture, &NeverCancelled).await.unwrap();
    assert_eq!(tool.calls.lock().unwrap().len(), 1);
    assert_eq!(harness.factory.creates.load(Ordering::SeqCst), 1);
    assert_eq!(
        terminal_ids(&harness.publisher.events.lock().unwrap()),
        vec!["once"]
    );
    assert!(harness.tools.settled_conversation_sequence() > 0);
    assert_eq!(
        fixture
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
async fn failed_started_publication_is_outside_tool_catch_but_joins_all_admitted_peers() {
    let fixture = Fixture::new();
    fixture
        .batch(vec![call("start-fails", "a"), call("peer", "peer")])
        .await;
    let a = Arc::new(RecordingTool::new(
        &fixture,
        "a",
        ParallelSafety::ParallelSafe,
    ));
    let completed = Arc::new(Mutex::new(vec![]));
    let (peer, gate) = gated(&fixture, "peer", ParallelSafety::ParallelSafe, &completed);
    let mut harness = Harness::new(&fixture, vec![a.clone().handle(), peer.clone().handle()]).await;
    harness.publisher.fail_started.store(1, Ordering::SeqCst);
    let control = async {
        gate.entered().await;
        assert!(terminal_ids(&fixture.events().await).is_empty());
        gate.open();
    };
    let (result, ()) = tokio::join!(harness.run(&fixture, &NeverCancelled), control);
    assert!(result.is_err());
    assert!(a.calls.lock().unwrap().is_empty());
    assert_eq!(peer.calls.lock().unwrap().len(), 1);
    assert!(terminal_ids(&fixture.events().await).is_empty());
    assert_eq!(harness.tools.settled_conversation_sequence(), 0);
    assert!(harness.activity.capture().current_tool.is_none());
}

#[tokio::test]
async fn intentional_tool_errors_are_persisted_but_diagnostics_contain_only_bounded_classification()
{
    let fixture = Fixture::new();
    let mut tool = RecordingTool::new(&fixture, "canonical", ParallelSafety::Sequential);
    tool.metadata.aliases = vec!["alias".into()];
    tool.fail = true;
    let tool = Arc::new(tool);
    fixture.batch(vec![call("failure", "alias")]).await;
    let mut harness = Harness::new(&fixture, vec![tool.handle()]).await;
    harness.run(&fixture, &NeverCancelled).await.unwrap();
    let events = fixture.events().await;
    let error = events
        .iter()
        .find(|event| event.has_tool_error())
        .unwrap()
        .tool_error();
    assert_eq!(error.message, "RAW_SECRET_TOOL_FAILURE");
    assert_eq!(error.tool_name, "alias");
    assert_eq!(
        events
            .iter()
            .find(|event| event.has_tool_execution_started())
            .unwrap()
            .tool_execution_started()
            .tool_name,
        "alias"
    );
    let diagnostics = harness.diagnostics.0.lock().unwrap();
    assert!(
        diagnostics
            .iter()
            .all(|event| event.tool_name.as_deref() == Some("canonical"))
    );
    assert_eq!(
        diagnostics.last().unwrap().error_code.as_deref(),
        Some("io")
    );
    assert!(!format!("{diagnostics:?}").contains("RAW_SECRET_TOOL_FAILURE"));
    assert!(harness.activity.capture().current_tool.is_none());
}

#[tokio::test]
async fn utf8_output_spills_only_above_64kib_and_preserves_actual_blob_bytes() {
    for (text, spilled) in [("é".repeat(32768), false), ("é".repeat(32769), true)] {
        let fixture = Fixture::new();
        let mut tool = RecordingTool::new(&fixture, "large", ParallelSafety::Sequential);
        tool.output = ToolOutput::text(text.clone());
        fixture.batch(vec![call("large", "large")]).await;
        let mut harness = Harness::new(&fixture, vec![Arc::new(tool).handle()]).await;
        harness.run(&fixture, &NeverCancelled).await.unwrap();
        let terminal = terminals(&fixture, &["large"]).await.remove(0);
        if spilled {
            assert!(
                terminal
                    .message
                    .starts_with("Tool output exceeded 64 KiB and was saved to ")
            );
            let path = terminal
                .message
                .strip_prefix("Tool output exceeded 64 KiB and was saved to ")
                .unwrap()
                .strip_suffix('.')
                .unwrap();
            assert_eq!(std::fs::read_to_string(path).unwrap(), text);
            assert!(!fixture.jsonl().contains(&text));
        } else {
            assert_eq!(terminal.message, text);
        }
    }
}

#[tokio::test]
async fn close_reopen_partial_batch_skips_settled_call_and_idempotently_projects_pending_call() {
    let mut fixture = Fixture::new();
    let sequence = fixture
        .batch(vec![call("settled", "read"), call("pending", "read")])
        .await;
    let terminal = ToolExecutionTerminal {
        tool_call_id: "settled".into(),
        tool_name: "read".into(),
        status: ToolExecutionStatus::Finished,
        result_parts: vec![ConversationPart::text("durable first")],
        message: "durable first".into(),
    };
    let mut event = Event::new();
    event.id = id();
    event.agent_session_id = "agent".into();
    fixture
        .history
        .append_tool_settlement(event, sequence, terminal.clone())
        .await
        .unwrap();
    let path = fixture.path.clone();
    fixture
        .repository
        .events()
        .database()
        .close()
        .await
        .unwrap();
    fixture.path = std::env::current_dir().unwrap().join(".tmp").join(id());
    drop(fixture);
    let reopened = Fixture::open(&path);
    let history = Fixture::history_for(&path, &reopened);
    let reopened_fixture = Fixture {
        path,
        repository: reopened,
        history,
    };
    let tool = Arc::new(RecordingTool::new(
        &reopened_fixture,
        "read",
        ParallelSafety::Sequential,
    ));
    let mut harness = Harness::new(&reopened_fixture, vec![tool.clone().handle()]).await;
    harness
        .run(&reopened_fixture, &NeverCancelled)
        .await
        .unwrap();
    harness
        .run(&reopened_fixture, &NeverCancelled)
        .await
        .unwrap();
    assert_eq!(
        tool.calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| call.id.as_str())
            .collect::<Vec<_>>(),
        vec!["pending"]
    );
    assert_eq!(
        terminal_ids(&harness.publisher.events.lock().unwrap()),
        vec!["pending"]
    );
    assert_eq!(
        terminals(&reopened_fixture, &["settled", "pending"])
            .await
            .len(),
        2
    );
    let results = reopened_fixture
        .history
        .conversation()
        .await
        .unwrap()
        .into_iter()
        .filter(|item| item.message.role == MessageRole::Tool)
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].message.tool_call_id, "settled");
    assert_eq!(results[1].message.tool_call_id, "pending");
    let mut conflict = terminal;
    conflict.message = "different".into();
    let mut event = Event::new();
    event.id = id();
    event.agent_session_id = "agent".into();
    let error = reopened_fixture
        .history
        .append_tool_settlement(event, sequence, conflict)
        .await
        .unwrap_err();
    assert!(
        matches!(error, parrot_storage::events::EventError::Conflict(ref message) if message == "tool call settled was already settled differently"),
        "durable identity conflict was masked: {error}"
    );
}
