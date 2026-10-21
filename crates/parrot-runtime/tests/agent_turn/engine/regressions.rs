use super::*;

struct CancelBeforeClose(Arc<Signal>);
impl TurnCompletion<Security> for CancelBeforeClose {
    fn complete<'a>(
        &'a self,
        _: &'a CompletionCandidate<Security>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<CompletionDecision, TurnError>> {
        Box::pin(async move {
            self.0.cancel();
            Ok(CompletionDecision::Complete)
        })
    }
}
#[tokio::test]
async fn cancellation_before_terminal_admission_closes_same_turn_as_interrupted_using_cleanup_token()
 {
    let fixture = Fixture::new();
    let lifetime = Arc::new(Signal::new());
    let provider = Scripted::new(vec![Step::complete("must not be committed")]);
    let runtime = fixture
        .runtime(
            provider,
            Arc::new(Tools::default()),
            vec![Arc::new(CancelBeforeClose(lifetime.clone()))],
            lifetime,
            0,
        )
        .await;
    let sent = runtime
        .send("question", protocol::Delivery::DELIVERY_STEER)
        .await;
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Canceled
    );
    let events = fixture.events().await;
    let boundaries: Vec<_> = events
        .iter()
        .filter(|event| event.has_turn_started() || event.has_turn_ended())
        .collect();
    assert_eq!(boundaries.len(), 2);
    assert!(boundaries[0].has_turn_started());
    assert!(boundaries[1].has_turn_ended());
    assert_eq!(boundaries[1].turn_ended().finish_reason, "interrupted");
    assert_eq!(runtime.publisher.terminals.load(Ordering::SeqCst), 1);
    let conversation = fixture.conversation().await;
    let assistants: Vec<_> = conversation
        .messages()
        .into_iter()
        .filter(|message| message.role == MessageRole::Assistant)
        .collect();
    assert_eq!(assistants.len(), 1);
    assert!(assistants[0].content().contains("interrupted"));
    assert!(!fixture.jsonl().contains("must not be committed"));
    runtime.close().await;
}
fn http_error(body: &str) -> Error {
    Error::Http(parrot_provider::http_stream::HttpError::Response(
        parrot_provider::http_stream::HttpFailure {
            status: 503,
            error_type: "busy".into(),
            error_code: "retry".into(),
            detail: "safe classification".into(),
            response_body: body.into(),
        },
    ))
}
#[tokio::test]
async fn actual_turn_failed_response_body_is_top_level_only_and_never_activity_metadata() {
    let secret = "RAW-PROVIDER-BODY-secret-\"unredacted\"";
    for (error, expected) in [
        (http_error(secret), secret),
        (
            Error::Response(
                parrot_provider::responses::ResponsesError::ProviderResponse {
                    error_type: "busy".into(),
                    error_code: "retry".into(),
                    detail: "safe classification".into(),
                    response_body: secret.into(),
                },
            ),
            secret,
        ),
        (Error::Io("ordinary IO".into()), ""),
        (
            Error::ProviderWithCause {
                message: "outer provider wrapper".into(),
                cause: Box::new(http_error(secret)),
            },
            "",
        ),
    ] {
        let fixture = Fixture::new();
        let provider = Scripted::new(vec![Step {
            completion: Err(error),
            gate: None,
            close_gate: None,
            events: VecDeque::new(),
        }]);
        let runtime = fixture
            .runtime(
                provider.clone(),
                Arc::new(Tools::default()),
                vec![],
                Arc::new(Signal::new()),
                0,
            )
            .await;
        let sent = runtime
            .send("question", protocol::Delivery::DELIVERY_STEER)
            .await;
        assert_eq!(
            bounded(sent.execution.join()).await.unwrap().status,
            ExecutionStatus::Failed
        );
        let events = fixture.events().await;
        let failed: Vec<_> = events
            .iter()
            .filter(|event| event.has_turn_failed())
            .collect();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].turn_failed().provider_response_body, expected);
        assert!(
            !runtime
                .activity
                .diagnostic
                .lock()
                .unwrap()
                .join("\n")
                .contains(secret)
        );
        assert_eq!(provider.stream_closes.load(Ordering::SeqCst), 1);
        runtime.close().await;
    }
}
#[tokio::test]
async fn completion_waits_for_true_stream_close_and_shutdown_joins_pool_and_output_close() {
    let fixture = Fixture::new();
    let stream_close = Gate::new();
    let pool_close = Gate::new();
    let output_close = Gate::new();
    let mut step = Step::complete("joined answer");
    step.close_gate = Some(stream_close.clone());
    let provider = Scripted::new(vec![step]);
    *provider.session_close_gate.lock().unwrap() = Some(pool_close.clone());
    let runtime = fixture
        .runtime(
            provider.clone(),
            Arc::new(Tools::default()),
            vec![],
            Arc::new(Signal::new()),
            0,
        )
        .await;
    *runtime.output.close_gate.lock().unwrap() = Some(output_close.clone());
    let sent = runtime
        .send("question", protocol::Delivery::DELIVERY_STEER)
        .await;
    stream_close.entered().await;
    assert!(!sent.execution.is_finished());
    assert_eq!(
        fixture
            .events()
            .await
            .iter()
            .filter(|event| event.has_turn_ended())
            .count(),
        0
    );
    stream_close.open();
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    let closing = runtime.session.close_handle();
    pool_close.entered().await;
    assert!(!closing.is_finished());
    assert_eq!(runtime.output.closes.load(Ordering::SeqCst), 0);
    pool_close.open();
    output_close.entered().await;
    assert!(!closing.is_finished());
    output_close.open();
    bounded(closing.join()).await.unwrap();
    assert_eq!(provider.stream_closes.load(Ordering::SeqCst), 1);
    assert_eq!(provider.session_closes.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.output.closes.load(Ordering::SeqCst), 1);
}
struct CheckReminder {
    history: AgentHistoryView,
    calls: AtomicUsize,
}
impl TurnCompletion<Security> for CheckReminder {
    fn complete<'a>(
        &'a self,
        _: &'a CompletionCandidate<Security>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<CompletionDecision, TurnError>> {
        Box::pin(async move {
            assert_eq!(
                self.history
                    .notices("exit_reminder_changed".into())
                    .await
                    .unwrap()
                    .len(),
                1
            );
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(CompletionDecision::Complete)
        })
    }
}
#[tokio::test]
async fn dropped_owned_real_reminder_observer_during_admitted_sqlite_is_joined_before_callback_and_terminal()
 {
    use std::{future::Future, task::Poll};
    let fixture = Fixture::new();
    let provider_gate = Gate::new();
    let mut step = Step::complete("answer after mutation");
    step.gate = Some(provider_gate.clone());
    let callback = Arc::new(CheckReminder {
        history: fixture.history.clone(),
        calls: AtomicUsize::new(0),
    });
    let runtime = fixture
        .runtime(
            Scripted::new(vec![step]),
            Arc::new(Tools::default()),
            vec![callback.clone()],
            Arc::new(Signal::new()),
            0,
        )
        .await;
    let sent = runtime
        .send("question", protocol::Delivery::DELIVERY_STEER)
        .await;
    provider_gate.entered().await;
    let (release, blocked) = std::sync::mpsc::channel();
    let barrier = fixture
        .repository
        .events()
        .database()
        .admit("blocked-before-reminder", &NeverCancelled, move |_| {
            blocked.recv_timeout(Duration::from_secs(10)).unwrap();
            Ok(())
        })
        .unwrap();
    let admitted = Arc::new(Semaphore::new(0));
    let admitted_worker = admitted.clone();
    let observer = runtime
        .session
        .owned_mutation(
            move |backend, _| {
                Box::pin(async move {
                    let mut mutation = Box::pin(backend.reminders().set(
                        "owned",
                        "must settle",
                        &NeverCancelled,
                    ));
                    // With the uncontended reminder lock, the first poll admits the real
                    // append to the blocked SQLite actor before yielding its receipt wait.
                    std::future::poll_fn(|cx| {
                        assert!(matches!(mutation.as_mut().poll(cx), Poll::Pending));
                        Poll::Ready(())
                    })
                    .await;
                    admitted_worker.add_permits(1);
                    mutation.await.map_err(|error| {
                        parrot_runtime::agent_turn::lifecycle::LifecycleError::Failed(
                            error.to_string(),
                        )
                    })
                })
            },
            Arc::new(NeverCancelled),
        )
        .unwrap();
    bounded(admitted.acquire()).await.unwrap().forget();
    drop(observer);
    provider_gate.open();
    tokio::task::yield_now().await;
    assert!(!sent.execution.is_finished());
    assert_eq!(callback.calls.load(Ordering::SeqCst), 0);
    release.send(()).unwrap();
    barrier.into_completion().await.unwrap();
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    assert_eq!(callback.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        runtime.session.backend().reminders().titles().await,
        ["owned"]
    );
    let rehydrated = ExitReminders::new(
        fixture.history.clone(),
        runtime.publisher.clone(),
        templates(),
    )
    .await
    .unwrap();
    assert_eq!(rehydrated.titles().await, ["owned"]);
    let events = fixture.events().await;
    let changed = events
        .iter()
        .position(|event| {
            event.has_agent_notice() && event.agent_notice().kind == "exit_reminder_changed"
        })
        .unwrap();
    let ended = events
        .iter()
        .position(|event| event.has_turn_ended())
        .unwrap();
    assert!(changed < ended);
    runtime.close().await;
}
fn tool_step(arguments: &str, finish: &str) -> Step {
    let mut step = Step::complete("tool candidate");
    let completion = step.completion.as_mut().unwrap();
    completion.finish_reason = finish.into();
    completion.tool_calls = vec![sessions::ToolCall {
        id: id(),
        name: "fake_tool".into(),
        arguments_json: arguments.into(),
    }];
    step
}
#[tokio::test]
async fn completed_truncated_and_invalid_tool_arguments_retry_before_request_received() {
    let fixture = Fixture::new();
    let provider = Scripted::new(vec![
        tool_step("{}", "length"),
        tool_step("{bad", "tool_calls"),
        Step::complete("valid final"),
    ]);
    let runtime = fixture
        .runtime(
            provider.clone(),
            Arc::new(Tools::default()),
            vec![],
            Arc::new(Signal::new()),
            0,
        )
        .await;
    let sent = runtime
        .send("question", protocol::Delivery::DELIVERY_STEER)
        .await;
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    let events = fixture.events().await;
    let retries: Vec<_> = events
        .iter()
        .filter(|event| event.has_retry_notice())
        .map(|event| event.retry_notice())
        .collect();
    assert_eq!(retries.len(), 2);
    assert_eq!(
        retries[0].reason,
        templates()
            .render("agent-session.truncated-tool-call", &[], &NeverCancelled)
            .unwrap()
    );
    assert_eq!(retries[0].attempt, 1);
    assert_eq!(
        retries[1].reason,
        templates()
            .render("agent-session.invalid-tool-call", &[], &NeverCancelled)
            .unwrap()
    );
    assert_eq!(retries[1].attempt, 2);
    assert!(!events.iter().any(|event| event.has_tool_request_received()));
    assert_eq!(provider.requests.lock().unwrap().len(), 3);
    assert_eq!(
        fixture
            .conversation()
            .await
            .messages()
            .iter()
            .filter(|message| message.role == MessageRole::Assistant)
            .count(),
        1
    );
    runtime.close().await;
}
#[tokio::test]
async fn any_valid_json_tool_arguments_are_accepted_not_just_objects() {
    for arguments in ["[]", "42", "null", "true", "\"scalar\""] {
        let fixture = Fixture::new();
        let provider = Scripted::new(vec![
            tool_step(arguments, "tool_calls"),
            Step::complete("settled"),
        ]);
        let runtime = fixture
            .runtime(
                provider.clone(),
                Arc::new(Tools::default()),
                vec![],
                Arc::new(Signal::new()),
                0,
            )
            .await;
        let sent = runtime
            .send("question", protocol::Delivery::DELIVERY_STEER)
            .await;
        assert_eq!(
            bounded(sent.execution.join()).await.unwrap().status,
            ExecutionStatus::Succeeded
        );
        let events = fixture.events().await;
        assert!(!events.iter().any(|event| event.has_retry_notice()));
        let requests: Vec<_> = events
            .iter()
            .filter(|event| event.has_tool_request_received())
            .collect();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].tool_request_received().tool_call_count, 1);
        assert_eq!(provider.requests.lock().unwrap().len(), 2);
        runtime.close().await;
    }
}
#[tokio::test]
async fn opaque_provider_content_survives_terminal_projection_and_repository_reopen() {
    let fixture = Fixture::new();
    let opaque = r#"[{"type":"reasoning","encrypted_content":"opaque==","summary":[{"type":"summary_text","text":"private continuity"}]},{"type":"message","content":[{"type":"output_text","text":"visible"}]}]"#;
    let mut step = Step::complete("visible");
    let completed = step.completion.as_mut().unwrap();
    completed.provider_id = "responses".into();
    completed.provider_content = opaque.into();
    let runtime = fixture
        .runtime(
            Scripted::new(vec![step]),
            Arc::new(Tools::default()),
            vec![],
            Arc::new(Signal::new()),
            0,
        )
        .await;
    let sent = runtime
        .send("question", protocol::Delivery::DELIVERY_STEER)
        .await;
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    runtime.close().await;
    let before = fixture.history.conversation().await.unwrap();
    let assistant = before
        .iter()
        .find(|item| item.message.role == MessageRole::Assistant)
        .unwrap();
    assert_eq!(assistant.message.provider_content.provider_id, "responses");
    assert_eq!(assistant.message.provider_content.content, opaque);
    fixture
        .repository
        .events()
        .database()
        .close()
        .await
        .unwrap();
    let reopened = Fixture::open(&fixture.path);
    let history = Fixture::history_for(&fixture.path, &reopened);
    let after = history.conversation().await.unwrap();
    assert_eq!(before, after);
    let conversation = AgentConversation::new(history).await.unwrap();
    let message = conversation
        .messages()
        .into_iter()
        .find(|message| message.role == MessageRole::Assistant)
        .unwrap();
    assert_eq!(message.provider_content.content, opaque);
    let provider = parrot_runtime::compaction::provider_message(&message);
    assert_eq!(provider.provider_content, opaque);
    let projected: Vec<serde_json::Value> = fixture
        .jsonl()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let assistant = projected
        .iter()
        .find(|entry| entry["type"] == "message" && entry["role"] == "assistant")
        .unwrap();
    assert_eq!(assistant["parts"][0]["text"], "visible");
    reopened.events().database().close().await.unwrap();
}
