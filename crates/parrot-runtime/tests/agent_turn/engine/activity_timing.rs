use super::*;
use parrot_runtime::agent_turn::lifecycle::DrainBackend;

#[tokio::test]
async fn real_activity_finishes_each_started_turn_once_for_normal_publication_cancel_provider_failure_and_runaway()
 {
    for case in [
        "normal",
        "publication-cancel",
        "provider-failure",
        "runaway",
    ] {
        let fixture = Fixture::new();
        let mut step = Step::complete("answer");
        if case == "provider-failure" {
            step.completion = Err(Error::Io("safe failure".into()));
        }
        let provider = Scripted::new(if case == "runaway" {
            vec![]
        } else {
            vec![step]
        });
        let runtime = fixture
            .runtime(
                provider.clone(),
                Arc::new(Tools::default()),
                vec![],
                Arc::new(Signal::new()),
                usize::from(case == "publication-cancel"),
            )
            .await;
        if case == "runaway" {
            runtime
                .session
                .backend()
                .update_selection(TurnSelection {
                    model: provider.selection(),
                    profile: TurnProfile {
                        id: "mode".into(),
                        maximum_requests: 0,
                        allowed_tools: None,
                        disabled_tools: vec![],
                    },
                    security: Arc::new(Security),
                })
                .unwrap();
        }
        let sent = runtime
            .send("question", protocol::Delivery::DELIVERY_STEER)
            .await;
        let result = bounded(sent.execution.join()).await.unwrap();
        assert_eq!(
            result.status,
            match case {
                "normal" => ExecutionStatus::Succeeded,
                "publication-cancel" => ExecutionStatus::Canceled,
                _ => ExecutionStatus::Failed,
            }
        );
        let diagnostic = runtime.activity.diagnostic.lock().unwrap().clone();
        let starts: Vec<_> = diagnostic
            .iter()
            .filter(|entry| entry.starts_with("started:"))
            .collect();
        let ends: Vec<_> = diagnostic
            .iter()
            .filter(|entry| entry.starts_with("finished:"))
            .collect();
        assert_eq!(starts.len(), 1, "{case}: {diagnostic:?}");
        assert_eq!(ends.len(), 1, "{case}: {diagnostic:?}");
        let turn = starts[0].strip_prefix("started:").unwrap();
        assert!(ends[0].starts_with(&format!("finished:{turn}:")));
        drop(diagnostic);
        runtime.close().await;
    }
}
#[tokio::test]
async fn actual_live_activity_clock_records_nonzero_success_elapsed_and_failure_duration_before_parent_delivery()
 {
    for fail in [false, true] {
        let fixture = Fixture::new();
        let gate = Gate::new();
        let mut step = Step::complete("timed answer");
        step.gate = Some(gate.clone());
        if fail {
            step.completion = Err(Error::Io("timed provider failure".into()));
        }
        let tools = Arc::new(Tools::default());
        let trace = tools.trace.clone();
        let runtime = fixture
            .runtime(
                Scripted::new(vec![step]),
                tools,
                vec![],
                Arc::new(Signal::new()),
                0,
            )
            .await;
        // Unlike a low-level wake, the public source-shaped execution API owns
        // AgentStarted/AgentFinished and the domain request-session timer.
        let sent = bounded(
            runtime
                .session
                .send_text_message("timed question".into(), Arc::new(NeverCancelled)),
        )
        .await
        .unwrap();
        gate.entered().await;
        assert!(
            runtime
                .activity
                .live
                .tracker()
                .capture()
                .current_provider_request_duration
                .is_some()
        );
        runtime.activity.clock.advance(1234);
        gate.open();
        let execution = bounded(sent.execution.join()).await.unwrap();
        assert_eq!(
            execution.status,
            if fail {
                ExecutionStatus::Failed
            } else {
                ExecutionStatus::Succeeded
            }
        );
        let activity = runtime.activity.live.tracker().capture();
        assert_eq!(
            activity.request_session_duration,
            Some(Duration::from_millis(1234))
        );
        assert_eq!(
            activity.last_provider_request_duration,
            Some(Duration::from_millis(1234))
        );
        assert_eq!(activity.terminal_outcome, Some(execution.clone()));
        let events = fixture.events().await;
        if fail {
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.has_agent_failed())
                    .count(),
                1
            );
            assert!(!events.iter().any(|event| event.has_agent_finished()));
        } else {
            let finished: Vec<_> = events
                .iter()
                .filter(|event| event.has_agent_finished())
                .collect();
            assert_eq!(finished.len(), 1);
            assert_eq!(finished[0].agent_finished().elapsed_ms, Some(1234));
        }
        let trace = trace.values();
        let published = trace
            .iter()
            .position(|entry| entry == "execution-terminal-published")
            .unwrap();
        let finished = trace
            .iter()
            .position(|entry| entry.starts_with("execution-finished:"))
            .unwrap();
        let delivered = trace
            .iter()
            .position(|entry| entry.starts_with("parent-delivered:"))
            .unwrap();
        assert!(published < finished && finished < delivered, "{trace:?}");
        runtime.close().await;
    }
}
