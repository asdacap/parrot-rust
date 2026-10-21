use super::*;
use std::{future::Future, task::Poll};

fn gated_answer(answer: &str) -> (Step, Arc<Gate>) {
    let gate = Gate::new();
    let mut step = Step::complete(answer);
    step.gate = Some(gate.clone());
    (step, gate)
}

fn prompts(request: &Request) -> String {
    request
        .messages
        .iter()
        .filter(|message| message.role == sessions::Role::User)
        .map(sessions::Message::content)
        .collect::<Vec<_>>()
        .join(" | ")
}

async fn root_runtime(
    fixture: &Fixture,
    provider: Scripted,
    callbacks: Vec<Arc<dyn TurnCompletion<Security>>>,
) -> Runtime {
    let tools = Arc::new(Tools::default());
    let trace = tools.trace.clone();
    let broker = Arc::new(EventBroker::new());
    let publisher = Arc::new(Publisher {
        history: fixture.history.clone(),
        broker: broker.clone(),
        terminal_failure: 0,
        terminals: AtomicUsize::new(0),
        terminal_observed: AtomicBool::new(false),
        reminder_gate: Mutex::new(None),
        cancel_retry: AtomicBool::new(false),
        trace: trace.clone(),
    });
    let erased: Arc<RuntimePublisher> = publisher.clone();
    let templates = templates();
    let reminders = Arc::new(
        ExitReminders::new(fixture.history.clone(), erased.clone(), templates.clone())
            .await
            .unwrap(),
    );
    let output = Arc::new(Output::default());
    let activity = Arc::new(Activity::new(trace.clone()));
    let backend = RuntimeBackend::new(RuntimeConfig {
        // The pinned DrainTests fixture uses AgentIdentity.Main, not a child
        // whose initial status is intentionally another durable system message.
        identity: SessionIdentity {
            session_id: "agent".into(),
            name: "main".into(),
            parent_session_id: String::new(),
            depth: 0,
        },
        history: fixture.history.clone(),
        broker,
        publisher: erased,
        selection: Arc::new(Selection(Mutex::new(TurnSelection {
            model: provider.selection(),
            profile: TurnProfile {
                id: "mode".into(),
                maximum_requests: 8,
                allowed_tools: None,
                disabled_tools: vec![],
            },
            security: Arc::new(Security),
        }))),
        tools,
        prompt: Box::new(Prompt {
            trace: trace.clone(),
            sampled: false,
            cancel_sample: false,
        }),
        skills: Arc::new(Skills(trace.clone())),
        context: fixture.controller(),
        status: Arc::new(Status(trace.clone())),
        completions: callbacks,
        templates,
        reminders,
        output: output.clone(),
        activity: activity.clone(),
        parent_completion: Arc::new(Parent(trace)),
        ancestors: vec![],
        event_id: Arc::new(id),
    })
    .await
    .unwrap();
    Runtime {
        session: AgentSession::new(backend, Arc::new(Signal::new())),
        publisher,
        output,
        activity,
    }
}

async fn assert_history(fixture: &Fixture, expected: &[&str]) {
    assert_eq!(fixture.history.messages().await.unwrap(), expected);
    let projected: Vec<String> = fixture
        .jsonl()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|entry| entry["type"] == "message")
        .map(|entry| {
            let text: String = entry["parts"]
                .as_array()
                .unwrap()
                .iter()
                .map(|part| part["text"].as_str().unwrap())
                .collect();
            format!("{}: {text}", entry["role"].as_str().unwrap())
        })
        .collect();
    assert_eq!(projected, expected);
}

// Pinned DrainTests.cs:64-94, Prompts_admitted_during_a_turn_are_answered_by_one_drain_in_order.
#[tokio::test]
async fn two_steers_admitted_during_one_drain_preserve_exact_source_conversation_order() {
    let fixture = Fixture::new();
    let (first_step, first_provider) = gated_answer("first answer");
    let (second_step, second_provider) = gated_answer("second answer");
    let provider = Scripted::new(vec![first_step, second_step]);
    let runtime = root_runtime(&fixture, provider.clone(), vec![]).await;

    let first = runtime
        .send("first prompt", protocol::Delivery::DELIVERY_STEER)
        .await;
    first_provider.entered().await;
    let second = runtime
        .send("second prompt", protocol::Delivery::DELIVERY_STEER)
        .await;
    first_provider.open();
    second_provider.entered().await;
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(prompts(&requests[1]), "first prompt | second prompt");
    }
    second_provider.open();
    bounded(first.execution.join()).await.unwrap();
    bounded(second.execution.join()).await.unwrap();
    runtime.close().await;

    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    assert_history(
        &fixture,
        &[
            "user: first prompt",
            "assistant: first answer",
            "user: second prompt",
            "assistant: second answer",
        ],
    )
    .await;
}

// Pinned DrainTests.cs:544-573, A_drain_continues_until_every_queued_prompt_is_answered.
#[tokio::test]
async fn all_three_queued_prompts_are_exhausted_into_exact_six_message_history() {
    let fixture = Fixture::new();
    let (first_step, first_provider) = gated_answer("first answer");
    let (second_step, second_provider) = gated_answer("second answer");
    let (third_step, third_provider) = gated_answer("third answer");
    let provider = Scripted::new(vec![first_step, second_step, third_step]);
    let runtime = root_runtime(&fixture, provider.clone(), vec![]).await;

    let first = runtime
        .send("first prompt", protocol::Delivery::DELIVERY_STEER)
        .await;
    first_provider.entered().await;
    runtime
        .send("second prompt", protocol::Delivery::DELIVERY_QUEUE)
        .await;
    runtime
        .send("third prompt", protocol::Delivery::DELIVERY_QUEUE)
        .await;
    first_provider.open();
    second_provider.entered().await;
    second_provider.open();
    third_provider.entered().await;
    third_provider.open();
    bounded(first.execution.join()).await.unwrap();
    runtime.close().await;

    assert_eq!(provider.requests.lock().unwrap().len(), 3);
    assert!(!fixture.history.has_pending_inputs().await.unwrap());
    assert_history(
        &fixture,
        &[
            "user: first prompt",
            "assistant: first answer",
            "user: second prompt",
            "assistant: second answer",
            "user: third prompt",
            "assistant: third answer",
        ],
    )
    .await;
}

struct GatedRetryCompletion {
    gate: Arc<Gate>,
    calls: AtomicUsize,
}
impl TurnCompletion<Security> for GatedRetryCompletion {
    fn complete<'a>(
        &'a self,
        _: &'a CompletionCandidate<Security>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<CompletionDecision, TurnError>> {
        Box::pin(async move {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.gate
                    .wait(cancellation)
                    .await
                    .map_err(TurnError::Provider)?;
                Ok(CompletionDecision::Retry {
                    system_message: "retry first turn".into(),
                    retain_candidate_assistant: false,
                    completion_retry_pending: None,
                })
            } else {
                Ok(CompletionDecision::Complete)
            }
        })
    }
}

// Pinned DrainTests.cs:307-347 and its GatedCompletionCallback at2731-2759.
#[tokio::test]
async fn owned_fifo_send_and_wait_reservations_wait_for_blocked_retry_callback_and_return_exact_answers()
 {
    let fixture = Fixture::new();
    let (candidate_step, candidate_provider) = gated_answer("first candidate");
    let (first_answer_step, first_answer_provider) = gated_answer("first answer");
    let (second_answer_step, second_answer_provider) = gated_answer("second answer");
    let provider = Scripted::new(vec![candidate_step, first_answer_step, second_answer_step]);
    let callback_gate = Gate::new();
    let runtime = root_runtime(
        &fixture,
        provider.clone(),
        vec![Arc::new(GatedRetryCompletion {
            gate: callback_gate.clone(),
            calls: AtomicUsize::new(0),
        })],
    )
    .await;

    let first_session = runtime.session.clone();
    let first = tokio::spawn(async move {
        first_session
            .send_and_wait(
                vec![ConversationPart::text("first prompt")],
                id(),
                None,
                None,
                Arc::new(NeverCancelled),
            )
            .await
    });
    candidate_provider.entered().await;
    let second_session = runtime.session.clone();
    let mut second = Box::pin(async move {
        second_session
            .send_and_wait(
                vec![ConversationPart::text("second prompt")],
                id(),
                None,
                None,
                Arc::new(NeverCancelled),
            )
            .await
    });
    // Poll the real public method once to reserve its owned FIFO position before
    // releasing the predecessor's provider; retaining it does not bypass its wait.
    std::future::poll_fn(|cx| {
        assert!(matches!(second.as_mut().poll(cx), Poll::Pending));
        Poll::Ready(())
    })
    .await;
    let second = tokio::spawn(second);
    candidate_provider.open();
    callback_gate.entered().await;
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert!(!first.is_finished());
    assert!(!second.is_finished());

    callback_gate.open();
    first_answer_provider.entered().await;
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1]
                .messages
                .iter()
                .rev()
                .find(|message| message.role == sessions::Role::System)
                .unwrap()
                .content(),
            "retry first turn"
        );
    }
    assert!(!second.is_finished());
    first_answer_provider.open();
    assert_eq!(bounded(first).await.unwrap().unwrap(), "first answer");
    second_answer_provider.entered().await;
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(
            requests[2]
                .messages
                .iter()
                .rev()
                .find(|message| message.role == sessions::Role::User)
                .unwrap()
                .content(),
            "second prompt"
        );
        let retry = requests[2]
            .messages
            .iter()
            .position(|message| {
                message.role == sessions::Role::System && message.content() == "retry first turn"
            })
            .unwrap();
        let second_user = requests[2]
            .messages
            .iter()
            .position(|message| {
                message.role == sessions::Role::User && message.content() == "second prompt"
            })
            .unwrap();
        assert!(retry < second_user);
    }
    second_answer_provider.open();
    assert_eq!(bounded(second).await.unwrap().unwrap(), "second answer");
    runtime.close().await;
}
