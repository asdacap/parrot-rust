use super::*;

struct BlockCompletion(Arc<Gate>);
impl CompletionStateSource for BlockCompletion {
    fn get_pending_ask_reminder<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, TurnError>> {
        Box::pin(async move { Ok(None) })
    }
    fn get_active_work_reminder<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, TurnError>> {
        Box::pin(async move { Ok(None) })
    }
    fn get_exit_reminder<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, TurnError>> {
        Box::pin(async move {
            self.0
                .wait(cancellation)
                .await
                .map_err(TurnError::Provider)?;
            Ok(None)
        })
    }
    fn record_completion_feedback<'a>(
        &'a self,
        feedback: &'a CompletionFeedback,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), TurnError>> {
        Box::pin(async move { panic!("this source has no reminder to record: {feedback:?}") })
    }
}
#[tokio::test]
async fn durable_queued_input_survives_controlled_shutdown_and_repository_restart_with_exact_turns_usage_and_reminder()
 {
    let fixture = Fixture::new();
    let callback_gate = Gate::new();
    let runtime = fixture
        .runtime(
            Scripted::new(vec![Step::complete("cancelled candidate")]),
            Arc::new(Tools::default()),
            vec![Arc::new(BlockCompletion(callback_gate.clone()))],
            Arc::new(Signal::new()),
            0,
        )
        .await;
    let first = runtime
        .send("first question", protocol::Delivery::DELIVERY_STEER)
        .await;
    callback_gate.entered().await;
    let queued = runtime
        .session
        .record(
            vec![ConversationPart::text("durable queued question")],
            "queued-message".into(),
            protocol::Delivery::DELIVERY_QUEUE,
            Arc::new(NeverCancelled),
        )
        .await
        .unwrap();
    assert!(fixture.history.has_pending_inputs().await.unwrap());
    bounded(
        runtime
            .session
            .set_exit_reminder(
                "restart".into(),
                "survives interruption".into(),
                Arc::new(NeverCancelled),
            )
            .unwrap()
            .join(),
    )
    .await
    .unwrap();
    // Ordinary Interrupt auto-wakes pending queued work in the source. Close
    // instead fixes the process-restart boundary before that follow-up may run.
    runtime.close().await;
    assert_eq!(
        bounded(first.execution.join()).await.unwrap().status,
        ExecutionStatus::Canceled
    );
    assert!(fixture.history.has_pending_inputs().await.unwrap());
    let before = fixture.events().await;
    assert_eq!(
        before
            .iter()
            .filter(|event| event.has_input_promoted())
            .count(),
        1
    );
    runtime.close().await;
    fixture
        .repository
        .events()
        .database()
        .close()
        .await
        .unwrap();
    let repository = Fixture::open(&fixture.path);
    let history = Fixture::history_for(&fixture.path, &repository);
    let restarted = Fixture {
        path: fixture.path.clone(),
        repository,
        history,
    };
    assert!(restarted.history.has_pending_inputs().await.unwrap());
    let mut final_step = Step::complete("answer after restart");
    let completion = final_step.completion.as_mut().unwrap();
    completion.input_tokens = 6;
    completion.output_tokens = 2;
    let provider = Scripted::new(vec![final_step]);
    let resumed = restarted
        .runtime(
            provider.clone(),
            Arc::new(Tools::default()),
            vec![],
            Arc::new(Signal::new()),
            0,
        )
        .await;
    assert_eq!(
        resumed.session.backend().reminders().titles().await,
        ["restart"]
    );
    assert_eq!(
        bounded(resumed.session.recover().join())
            .await
            .unwrap()
            .status,
        ExecutionStatus::Succeeded
    );
    let events = restarted.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_turn_started())
            .count(),
        2
    );
    let ended: Vec<_> = events
        .iter()
        .filter(|event| event.has_turn_ended())
        .map(|event| event.turn_ended())
        .collect();
    assert_eq!(ended.len(), 2);
    assert_eq!(ended[0].finish_reason, "interrupted");
    assert_eq!(ended[1].finish_reason, "stop");
    assert_eq!((ended[0].input_tokens, ended[0].output_tokens), (17, 9));
    assert_eq!((ended[1].input_tokens, ended[1].output_tokens), (23, 11));
    let promotions: Vec<_> = events
        .iter()
        .filter(|event| event.has_input_promoted())
        .map(|event| event.input_promoted())
        .collect();
    assert_eq!(promotions.len(), 2);
    assert_eq!(promotions[1].input_id, queued.input.id);
    assert_eq!(promotions[1].message_id, "queued-message");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_agent_notice()
                && event.agent_notice().kind == "exit_reminder_changed")
            .count(),
        1
    );
    assert!(!restarted.history.has_pending_inputs().await.unwrap());
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0]
            .messages
            .iter()
            .any(|message| message.content() == "durable queued question")
    );
    drop(requests);
    let raw: Vec<serde_json::Value> = restarted
        .jsonl()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        raw.iter()
            .filter(|entry| entry["type"] == "request")
            .count(),
        2
    );
    assert_eq!(
        raw.iter()
            .filter(|entry| entry["type"] == "message" && entry["role"] == "assistant")
            .count(),
        2
    );
    resumed.close().await;
}
struct OrderedSource {
    name: &'static str,
    trace: Arc<Trace>,
    retries: AtomicUsize,
    retains_candidate: bool,
    feedback: Arc<Mutex<Vec<(String, CompletionFeedback)>>>,
}
impl CompletionStateSource for OrderedSource {
    fn get_pending_ask_reminder<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, TurnError>> {
        Box::pin(async move {
            self.trace.push(format!("{}:asks", self.name));
            Ok(None)
        })
    }
    fn get_active_work_reminder<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, TurnError>> {
        Box::pin(async move {
            self.trace.push(format!("{}:work", self.name));
            Ok((self.retries.load(Ordering::SeqCst) > 0)
                .then(|| format!("{} completion retry", self.name)))
        })
    }
    fn get_exit_reminder<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, TurnError>> {
        Box::pin(async move {
            self.trace.push(format!("{}:exit", self.name));
            Ok(None)
        })
    }
    fn record_completion_feedback<'a>(
        &'a self,
        feedback: &'a CompletionFeedback,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), TurnError>> {
        Box::pin(async move {
            assert_eq!(feedback.kind, CompletionFeedbackKind::ActiveWork);
            assert_eq!(feedback.candidate.is_some(), self.retains_candidate);
            assert_eq!(feedback.message, format!("{} completion retry", self.name));
            assert!(self.retries.fetch_sub(1, Ordering::SeqCst) > 0);
            self.trace.push(format!("{}:feedback", self.name));
            self.feedback
                .lock()
                .unwrap()
                .push((self.name.into(), feedback.clone()));
            Ok(())
        })
    }
}
#[tokio::test]
async fn completion_observations_collect_all_sources_in_order_and_retain_once_before_budget_restart()
 {
    use parrot_runtime::agent_turn::lifecycle::DrainBackend;
    let fixture = Fixture::new();
    let trace = Arc::new(Trace::default());
    let provider = Scripted::new(vec![Step::complete("candidate"), Step::complete("final")]);
    let feedback = Arc::new(Mutex::new(vec![]));
    let callbacks: Vec<Arc<dyn CompletionStateSource>> =
        [("first", true), ("retry", false), ("last", false)]
            .into_iter()
            .map(|(name, retains_candidate)| {
                Arc::new(OrderedSource {
                    name,
                    trace: trace.clone(),
                    retries: AtomicUsize::new(1),
                    retains_candidate,
                    feedback: feedback.clone(),
                }) as Arc<dyn CompletionStateSource>
            })
            .collect();
    let runtime = fixture
        .runtime(
            provider.clone(),
            Arc::new(Tools::default()),
            callbacks,
            Arc::new(Signal::new()),
            0,
        )
        .await;
    runtime
        .session
        .backend()
        .update_selection(TurnSelection {
            model: provider.selection(),
            profile: TurnProfile {
                id: "mode".into(),
                maximum_requests: 2,
                allowed_tools: None,
                disabled_tools: vec![],
            },
            security: Arc::new(Security),
        })
        .unwrap();
    let sent = runtime
        .send("question", protocol::Delivery::DELIVERY_STEER)
        .await;
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    assert_eq!(
        trace.values(),
        [
            "first:asks",
            "retry:asks",
            "last:asks",
            "first:work",
            "retry:work",
            "last:work",
            "first:exit",
            "retry:exit",
            "last:exit",
            "first:feedback",
            "retry:feedback",
            "last:feedback",
            "first:asks",
            "retry:asks",
            "last:asks",
            "first:work",
            "retry:work",
            "last:work",
            "first:exit",
            "retry:exit",
            "last:exit",
        ]
    );
    let feedback = feedback.lock().unwrap().clone();
    assert_eq!(feedback.len(), 3);
    assert_eq!(
        feedback
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["first", "retry", "last"]
    );
    assert!(
        feedback
            .iter()
            .all(|(_, feedback)| feedback.kind == CompletionFeedbackKind::ActiveWork)
    );
    assert_eq!(
        feedback
            .iter()
            .map(|(_, feedback)| feedback.message.as_str())
            .collect::<Vec<_>>(),
        [
            "first completion retry",
            "retry completion retry",
            "last completion retry"
        ]
    );
    assert_eq!(
        feedback[0].1.candidate.as_ref().unwrap().assistant_text,
        "candidate"
    );
    assert!(
        feedback[1..]
            .iter()
            .all(|(_, feedback)| feedback.candidate.is_none())
    );
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    let final_prompt = templates()
        .render("agent-session.final-request", &[], &NeverCancelled)
        .unwrap();
    assert!(
        !requests[1]
            .messages
            .iter()
            .any(|message| message.content() == final_prompt)
    );
    assert_eq!(
        requests[1]
            .messages
            .iter()
            .filter(|message| message.role == sessions::Role::System
                && message.content().ends_with("completion retry"))
            .map(|message| message.content())
            .collect::<Vec<_>>(),
        [
            "first completion retry",
            "retry completion retry",
            "last completion retry"
        ]
    );
    assert_eq!(
        requests[1]
            .messages
            .iter()
            .filter(|message| message.role == sessions::Role::Assistant)
            .count(),
        1
    );
    assert!(requests[1].messages.iter().any(
        |message| message.role == sessions::Role::Assistant && message.content() == "candidate"
    ));
    drop(requests);
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
    assert!(!events.iter().any(|event| event.has_turn_failed()));
    let durable = fixture.conversation().await;
    let assistants: Vec<_> = durable
        .messages()
        .into_iter()
        .filter(|message| message.role == MessageRole::Assistant)
        .collect();
    assert_eq!(assistants.len(), 1);
    assert_eq!(assistants[0].content(), "final");
    runtime.close().await;
}
