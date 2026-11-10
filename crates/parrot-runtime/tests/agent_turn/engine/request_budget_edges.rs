//! Remaining R06/R08 predicates through the actual durable drain.
//! Registered leaf tools exercise actual runtime tool settlement.
use super::*;

const RUNAWAY: &str = "the turn exceeded its provider-request limit";

struct EdgeTools {
    inner: Tools,
    name: &'static str,
}
impl ToolFactory<Security> for EdgeTools {
    fn materialize(&self) -> Result<ToolInventory<Security>, TurnError> {
        let definitions = vec![sessions::Tool {
            name: self.name.into(),
            description: String::new(),
            parameters_json: "{}".into(),
        }];
        Ok(inventory(
            vec![ToolHandle::Borrowed(Arc::new(FixtureTool {
                metadata: ToolMetadata {
                    name: self.name.into(),
                    aliases: vec![],
                },
                tools: self.inner.clone(),
                final_phase: self.name == "survivor",
                text: "fake tool result",
                observations: None,
            }))],
            &definitions,
        ))
    }
}
struct EdgeRuntime {
    session: AgentSession<RuntimeBackend<Security>>,
}
impl EdgeRuntime {
    async fn new(
        fixture: &Fixture,
        provider: &Scripted,
        maximum_requests: i32,
        tool: &'static str,
    ) -> Self {
        Self::with_limits(
            fixture,
            provider,
            maximum_requests,
            tool,
            parrot_foundation::config::runtime::AgentLimitsConfig {
                disable_max_turns: false,
                ..Default::default()
            },
        )
        .await
    }
    async fn with_limits(
        fixture: &Fixture,
        provider: &Scripted,
        maximum_requests: i32,
        tool: &'static str,
        agent_limits: parrot_foundation::config::runtime::AgentLimitsConfig,
    ) -> Self {
        let tools = Arc::new(EdgeTools {
            inner: Tools::default(),
            name: tool,
        });
        let trace = tools.inner.trace.clone();
        let broker = Arc::new(EventBroker::new());
        let publisher: Arc<RuntimePublisher> = Arc::new(Publisher {
            history: fixture.history.clone(),
            broker: broker.clone(),
            terminal_failure: 0,
            cancel_retry: AtomicBool::new(false),
            terminals: AtomicUsize::new(0),
            terminal_observed: AtomicBool::new(false),
            reminder_gate: Mutex::new(None),
            trace: trace.clone(),
        });
        let templates = templates();
        let reminders = Arc::new(
            ExitReminders::new(
                fixture.history.clone(),
                publisher.clone(),
                templates.clone(),
            )
            .await
            .unwrap(),
        );
        let backend = RuntimeBackend::new(RuntimeConfig {
            tool_execution: tool_execution_config(
                &fixture.history,
                templates.clone(),
                publisher.clone(),
            ),
            agent_limits,
            identity: SessionIdentity {
                session_id: "agent".into(),
                name: "request-budget-edges".into(),
                parent_session_id: String::new(),
                depth: 0,
            },
            history: fixture.history.clone(),
            broker,
            publisher,
            selection: Arc::new(Selection(Mutex::new(TurnSelection {
                model: provider.selection(),
                profile: TurnProfile {
                    id: "budget-edge".into(),
                    maximum_requests,
                    allowed_tools: None,
                    disabled_tools: vec![],
                },
                security: Arc::new(Security),
            }))),
            tools: tools.clone(),
            prompt: Box::new(Prompt {
                trace: trace.clone(),
                sampled: false,
                cancel_sample: false,
            }),
            skills: Arc::new(Skills(trace.clone())),
            context: fixture.controller(),
            status: Arc::new(Status(trace.clone())),
            completion_sources: vec![],
            templates,
            reminders,
            output: Arc::new(Output::default()),
            activity: Arc::new(Activity::new(trace.clone())),
            parent_completion: Arc::new(Parent(trace)),
            ancestors: vec![],
            event_id: Arc::new(id),
        })
        .await
        .unwrap();
        Self {
            session: AgentSession::new(backend, Arc::new(Signal::new())),
        }
    }
    async fn execute(&self) -> Execution {
        let sent = bounded(self.session.send(
            vec![ConversationPart::text("prompt")],
            id(),
            protocol::Delivery::DELIVERY_STEER,
            IncomingActivity {
                name: "request-budget-edges".into(),
                cause: None,
                source: protocol::InputSource::default(),
            },
            Arc::new(NeverCancelled),
        ))
        .await
        .unwrap();
        bounded(sent.execution.join()).await.unwrap()
    }
    async fn close(&self) {
        bounded(self.session.close()).await.unwrap();
    }
}
fn tool_response(call_id: &str, name: &str, finish_reason: &str, arguments: &str) -> Step {
    let mut step = Step::complete("");
    let completion = step.completion.as_mut().unwrap();
    completion.finish_reason = finish_reason.into();
    completion.input_tokens = 1;
    completion.output_tokens = 1;
    completion.tool_calls = vec![sessions::ToolCall {
        id: call_id.into(),
        name: name.into(),
        arguments_json: arguments.into(),
    }];
    step
}
fn assert_runaway(events: &[Event]) {
    let failures: Vec<_> = events
        .iter()
        .filter(|event| event.has_turn_failed())
        .collect();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].turn_failed().message, RUNAWAY);
    assert!(failures[0].turn_failed().provider_response_body.is_empty());
    assert!(!events.iter().any(|event| event.has_turn_ended()));
}

#[tokio::test]
async fn global_default_disables_profile_request_budget_but_explicit_false_restores_it() {
    for disable_max_turns in [true, false] {
        let fixture = Fixture::new();
        let provider = Scripted::new(vec![
            tool_response("truncated-1", "settled", "length", "{"),
            tool_response("truncated-2", "settled", "length", "{"),
            Step::complete("done beyond profile budget"),
        ]);
        let mut limits = parrot_foundation::config::runtime::AgentLimitsConfig::default();
        if !disable_max_turns {
            limits.disable_max_turns = false;
        }
        let runtime = EdgeRuntime::with_limits(&fixture, &provider, 2, "settled", limits).await;
        let execution = runtime.execute().await;
        let events = fixture.events().await;
        {
            let requests = provider.requests.lock().unwrap();
            if disable_max_turns {
                assert_eq!(execution.status, ExecutionStatus::Succeeded);
                assert_eq!(execution.output, "done beyond profile budget");
                assert_eq!(requests.len(), 3);
                assert!(
                    requests
                        .iter()
                        .all(|request| request.tools.iter().any(|tool| tool.name == "settled"))
                );
                assert!(!events.iter().any(|event| event.has_turn_failed()));
                assert!(events.iter().any(|event| event.has_turn_ended()));
            } else {
                assert_eq!(execution.status, ExecutionStatus::Failed);
                assert_eq!(execution.error, RUNAWAY);
                assert_eq!(requests.len(), 2);
                assert!(requests[1].tools.is_empty());
                assert_runaway(&events);
            }
        }
        runtime.close().await;
    }
}

#[tokio::test]
async fn exhausted_budget_reports_native_failure_after_promotion_without_a_provider_request() {
    let fixture = Fixture::new();
    let provider = Scripted::default();
    let runtime = EdgeRuntime::new(&fixture, &provider, 0, "settled").await;
    let execution = runtime.execute().await;
    assert_eq!(execution.status, ExecutionStatus::Failed);
    assert_eq!(execution.error, RUNAWAY);
    assert!(provider.requests.lock().unwrap().is_empty());
    assert_eq!(provider.stream_closes.load(Ordering::SeqCst), 0);
    let events = fixture.events().await;
    assert_runaway(&events);
    let started = events
        .iter()
        .position(|event| event.has_turn_started())
        .unwrap();
    let promoted = events
        .iter()
        .position(|event| event.has_input_promoted())
        .unwrap();
    let failed = events
        .iter()
        .position(|event| event.has_turn_failed())
        .unwrap();
    assert!(started < promoted && promoted < failed);
    assert!(!fixture.history.has_pending_inputs().await.unwrap());
    runtime.close().await;
}

#[tokio::test]
async fn repeated_truncated_tool_responses_consume_exact_two_requests_without_tool_history_or_lifecycle()
 {
    let fixture = Fixture::new();
    let provider = Scripted::new(vec![
        tool_response("truncated", "settled", "length", "{"),
        tool_response("truncated", "settled", "length", "{"),
    ]);
    let runtime = EdgeRuntime::new(&fixture, &provider, 2, "settled").await;
    let execution = runtime.execute().await;
    assert_eq!(execution.status, ExecutionStatus::Failed);
    assert_eq!(execution.error, RUNAWAY);
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    assert_eq!(provider.stream_closes.load(Ordering::SeqCst), 2);
    let events = fixture.events().await;
    assert_runaway(&events);
    assert!(!events.iter().any(|event| event.has_tool_started()
        || event.has_tool_finished()
        || event.has_tool_cancelled()));
    assert!(!events.iter().any(|event| event.has_tool_request_received()));
    assert!(
        fixture
            .conversation()
            .await
            .messages()
            .iter()
            .all(|message| message.tool_calls.is_empty())
    );
    assert!(
        fixture
            .history
            .tool_terminals_of_calls(vec!["truncated".into()])
            .await
            .unwrap()
            .is_empty()
    );
    let retries: Vec<_> = events
        .iter()
        .filter(|event| event.has_retry_notice())
        .collect();
    assert_eq!(retries.len(), 2);
    assert_eq!(
        retries
            .iter()
            .map(|event| event.retry_notice().attempt)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    runtime.close().await;
}

#[tokio::test]
async fn length_completion_without_tools_preserves_exact_partial_assistant_history_and_no_retry() {
    let fixture = Fixture::new();
    let mut partial = Step::complete("partial answer");
    let completion = partial.completion.as_mut().unwrap();
    completion.finish_reason = "length".into();
    completion.input_tokens = 1;
    completion.output_tokens = 32_768;
    let provider = Scripted::new(vec![partial]);
    let runtime = EdgeRuntime::new(&fixture, &provider, 8, "settled").await;
    let execution = runtime.execute().await;
    assert_eq!(execution.status, ExecutionStatus::Succeeded);
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    let events = fixture.events().await;
    let ended: Vec<_> = events
        .iter()
        .filter(|event| event.has_turn_ended())
        .collect();
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].turn_ended().finish_reason, "length");
    assert!(
        !events
            .iter()
            .any(|event| event.has_retry_notice() || event.has_turn_failed())
    );
    let conversation = fixture.conversation().await;
    assert_eq!(
        conversation
            .messages()
            .iter()
            .map(|message| (message.role, message.content()))
            .collect::<Vec<_>>(),
        [
            (MessageRole::User, "prompt".to_owned()),
            (MessageRole::Assistant, "partial answer".to_owned())
        ]
    );
    assert_eq!(fixture.jsonl().matches("partial answer").count(), 1);
    runtime.close().await;
}

#[tokio::test]
async fn returned_tool_after_definitions_are_omitted_is_settled_and_grants_one_more_final_request()
{
    let fixture = Fixture::new();
    let provider = Scripted::new(vec![
        tool_response("call-1", "settled", "stop", "{}"),
        Step::complete("final answer"),
    ]);
    let runtime = EdgeRuntime::new(&fixture, &provider, 1, "settled").await;
    let execution = runtime.execute().await;
    assert_eq!(execution.status, ExecutionStatus::Succeeded);
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].tools.is_empty());
        assert!(requests[1].tools.is_empty());
        assert!(requests[1].messages.iter().any(
            |message| message.role == sessions::Role::Tool && message.tool_call_id == "call-1"
        ));
        assert!(
            requests[0]
                .messages
                .iter()
                .any(|message| message.role == sessions::Role::System
                    && message.content().contains("final provider request"))
        );
    }
    let terminals = fixture
        .history
        .tool_terminals_of_calls(vec!["call-1".into()])
        .await
        .unwrap();
    assert_eq!(terminals.len(), 1);
    let events = fixture.events().await;
    assert!(!events.iter().any(|event| event.has_turn_failed()));
    let ended: Vec<_> = events
        .iter()
        .filter(|event| event.has_turn_ended())
        .collect();
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].turn_ended().finish_reason, "stop");
    runtime.close().await;
}

#[tokio::test]
async fn four_settlement_extensions_allow_exact_six_provider_requests_then_fail_as_runaway() {
    let fixture = Fixture::new();
    let provider = Scripted::new(
        (1..=6)
            .map(|index| tool_response(&format!("call-{index}"), "survivor", "stop", "{}"))
            .chain([Step::complete("never reached")])
            .collect(),
    );
    let runtime = EdgeRuntime::new(&fixture, &provider, 2, "survivor").await;
    let execution = runtime.execute().await;
    assert_eq!(execution.status, ExecutionStatus::Failed);
    assert_eq!(execution.error, RUNAWAY);
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 6);
        for request in requests.iter() {
            assert_eq!(
                request
                    .tools
                    .iter()
                    .map(|tool| tool.name.as_str())
                    .collect::<Vec<_>>(),
                ["survivor"]
            );
        }
    }
    assert_eq!(provider.stream_closes.load(Ordering::SeqCst), 6);
    assert_eq!(provider.steps.lock().unwrap().len(), 1);
    let events = fixture.events().await;
    assert_runaway(&events);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_tool_request_received())
            .count(),
        6
    );
    assert!(!events.iter().any(|event| event.has_retry_notice()));
    for index in 1..=6 {
        assert_eq!(
            fixture
                .history
                .tool_terminals_of_calls(vec![format!("call-{index}")])
                .await
                .unwrap()
                .len(),
            1
        );
    }
    let conversation = fixture.conversation().await;
    assert_eq!(
        conversation
            .messages()
            .iter()
            .filter(|message| !message.tool_calls.is_empty())
            .count(),
        6
    );
    assert_eq!(
        conversation
            .messages()
            .iter()
            .filter(|message| message.role == MessageRole::Tool)
            .count(),
        6
    );
    assert!(!fixture.jsonl().contains("never reached"));
    runtime.close().await;
}
