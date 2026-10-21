use super::*;
use parrot_runtime::agent_turn::lifecycle::DrainBackend;
struct RetryOnce(AtomicBool);
impl TurnCompletion<Security> for RetryOnce {
    fn complete<'a>(
        &'a self,
        _: &'a CompletionCandidate<Security>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<CompletionDecision, TurnError>> {
        Box::pin(async move {
            Ok(if self.0.swap(false, Ordering::SeqCst) {
                CompletionDecision::Retry {
                    system_message: "source retry".into(),
                    retain_candidate_assistant: false,
                    completion_retry_pending: None,
                }
            } else {
                CompletionDecision::Complete
            })
        })
    }
}
#[tokio::test]
async fn source_one_completion_retry_with_budget_two_restarts_before_second_final_phase_admission()
{
    let fixture = Fixture::new();
    let provider = Scripted::new(vec![
        Step::complete("first candidate"),
        Step::complete("settled"),
    ]);
    let runtime = fixture
        .runtime(
            provider.clone(),
            Arc::new(Tools::default()),
            vec![Arc::new(RetryOnce(AtomicBool::new(true)))],
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
    {
        let requests = provider.requests.lock().unwrap();
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
    }
    let events = fixture.events().await;
    let ended: Vec<_> = events
        .iter()
        .filter(|event| event.has_turn_ended())
        .collect();
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].turn_ended().finish_reason, "stop");
    assert!(!events.iter().any(|event| event.has_turn_failed()));
    runtime.close().await;
}
struct BlockOnce {
    entered: Arc<Gate>,
    first: AtomicBool,
}
impl TurnCompletion<Security> for BlockOnce {
    fn complete<'a>(
        &'a self,
        _: &'a CompletionCandidate<Security>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<CompletionDecision, TurnError>> {
        Box::pin(async move {
            if self.first.swap(false, Ordering::SeqCst) {
                self.entered
                    .wait(cancellation)
                    .await
                    .map_err(TurnError::Provider)?;
            }
            Ok(CompletionDecision::Complete)
        })
    }
}
#[tokio::test]
async fn ordinary_interrupt_automatically_resumes_durable_queued_input_without_process_restart() {
    let fixture = Fixture::new();
    let completion = Gate::new();
    let next = Gate::new();
    let mut second = Step::complete("queued answer");
    second.gate = Some(next.clone());
    let provider = Scripted::new(vec![Step::complete("interrupted candidate"), second]);
    let runtime = fixture
        .runtime(
            provider.clone(),
            Arc::new(Tools::default()),
            vec![Arc::new(BlockOnce {
                entered: completion.clone(),
                first: AtomicBool::new(true),
            })],
            Arc::new(Signal::new()),
            0,
        )
        .await;
    let first = runtime
        .send("first", protocol::Delivery::DELIVERY_STEER)
        .await;
    completion.entered().await;
    bounded(runtime.session.record(
        vec![ConversationPart::text("queued")],
        "ordinary-queued".into(),
        protocol::Delivery::DELIVERY_QUEUE,
        Arc::new(NeverCancelled),
    ))
    .await
    .unwrap();
    bounded(runtime.session.interrupt(&NeverCancelled))
        .await
        .unwrap();
    assert_eq!(
        bounded(first.execution.join()).await.unwrap().status,
        ExecutionStatus::Canceled
    );
    next.entered().await;
    let (_, resumed) = runtime.session.wake(None);
    next.open();
    assert_eq!(
        bounded(resumed.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    let events = fixture.events().await;
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
        .collect();
    assert_eq!(ended.len(), 2);
    assert_eq!(ended[0].turn_ended().finish_reason, "interrupted");
    assert_eq!(ended[1].turn_ended().finish_reason, "stop");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_input_promoted())
            .count(),
        2
    );
    assert!(!fixture.history.has_pending_inputs().await.unwrap());
    runtime.close().await;
}
