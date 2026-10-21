use super::*;

struct BlockCompletion(Arc<Gate>);
impl TurnCompletion<Security> for BlockCompletion {
    fn complete<'a>(
        &'a self,
        _: &'a CompletionCandidate<Security>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<CompletionDecision, TurnError>> {
        Box::pin(async move {
            self.0
                .wait(cancellation)
                .await
                .map_err(TurnError::Provider)?;
            Ok(CompletionDecision::Complete)
        })
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
struct OrderedCallback {
    name: &'static str,
    trace: Arc<Trace>,
    retries: AtomicUsize,
    retain: bool,
    pending: Option<bool>,
    candidates: Arc<Mutex<Vec<(String, String)>>>,
}
impl TurnCompletion<Security> for OrderedCallback {
    fn complete<'a>(
        &'a self,
        candidate: &'a CompletionCandidate<Security>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<CompletionDecision, TurnError>> {
        Box::pin(async move {
            self.trace
                .push(format!("{}:{}", self.name, candidate.assistant.content()));
            self.candidates
                .lock()
                .unwrap()
                .push((self.name.into(), candidate.message_id.clone()));
            if self
                .retries
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                    value.checked_sub(1)
                })
                .is_ok()
            {
                Ok(CompletionDecision::Retry {
                    system_message: "source completion retry".into(),
                    retain_candidate_assistant: self.retain,
                    completion_retry_pending: self.pending,
                })
            } else {
                Ok(CompletionDecision::Complete)
            }
        })
    }
}
#[tokio::test]
async fn completion_callbacks_short_circuit_in_order_and_retry_restarts_original_request_budget() {
    use parrot_runtime::agent_turn::lifecycle::DrainBackend;
    let retain = false;
    let fixture = Fixture::new();
    let trace = Arc::new(Trace::default());
    let provider = Scripted::new(vec![Step::complete("candidate"), Step::complete("final")]);
    let candidates = Arc::new(Mutex::new(vec![]));
    let callbacks: Vec<Arc<dyn TurnCompletion<Security>>> =
        [("first", 0), ("retry", 1), ("last", 0)]
            .into_iter()
            .map(|(name, retries)| {
                Arc::new(OrderedCallback {
                    name,
                    trace: trace.clone(),
                    retries: AtomicUsize::new(retries),
                    retain,
                    pending: None,
                    candidates: candidates.clone(),
                }) as Arc<dyn TurnCompletion<Security>>
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
            "first:candidate",
            "retry:candidate",
            "first:final",
            "retry:final",
            "last:final"
        ]
    );
    let candidates = candidates.lock().unwrap().clone();
    assert_eq!(candidates.len(), 5);
    assert_eq!(candidates[0].1, candidates[1].1);
    assert_eq!(candidates[2].1, candidates[3].1);
    assert_eq!(candidates[3].1, candidates[4].1);
    assert_ne!(candidates[0].1, candidates[2].1);
    drop(candidates);
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
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.content() == "source completion retry")
    );
    assert_eq!(
        requests[1]
            .messages
            .iter()
            .filter(|message| message.role == sessions::Role::Assistant)
            .count(),
        if retain { 2 } else { 0 }
    );
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
