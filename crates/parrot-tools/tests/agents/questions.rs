use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use parrot_domain::contracts::async_ops::{
    BoxFuture, Cancellation, Cancelled, Clock, OperationError,
};
use parrot_domain::contracts::interactions::{
    AgentCommunication, QuestionDefinition, QuestionReply, QuestionRequester,
};
use parrot_domain::contracts::tools::{ParallelSafety, Tool};
use parrot_domain::values::tools::ConfiguredToolDefinition;
use parrot_runtime::interactions::{
    AgentAskBroker, AgentAskDelivery, AgentAskIdentity, AgentAskTemplates, AgentAsks,
    BrokerDiagnostic, InteractionDiagnostics, InteractionError, InteractionErrorKind,
    PendingAgentAsk, QuestionBroker,
};
use parrot_tools::agents::questions::{AskTool, QuestionTool, ReplyTool};
use parrot_tools::agents::{AgentToolError, ResultTemplates};

use super::{CANCELLED, NEVER, Security, context, invocation};

struct Diagnostics;
impl InteractionDiagnostics for Diagnostics {
    fn write(&self, _: BrokerDiagnostic) {}
}

fn error(error: InteractionError) -> AgentToolError {
    match error.kind {
        InteractionErrorKind::Registry => AgentToolError::Registry(error.message),
        InteractionErrorKind::QuestionRejected => AgentToolError::QuestionRejected(error.message),
        InteractionErrorKind::Question | InteractionErrorKind::QuestionNotFound => {
            AgentToolError::Question(error.message)
        }
        InteractionErrorKind::Argument => AgentToolError::Argument(error.message),
        _ => AgentToolError::Service(error.message),
    }
}
struct UserQuestions(Arc<QuestionBroker>);
impl QuestionRequester for UserQuestions {
    type Error = AgentToolError;
    fn ask<'a>(
        &'a self,
        questions: Vec<QuestionDefinition>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<QuestionReply, OperationError<AgentToolError>>> {
        Box::pin(async move {
            self.0
                .ask(questions, cancellation)
                .await
                .map_err(|failure| match failure {
                    OperationError::Cancelled => OperationError::Cancelled,
                    OperationError::Failed(failure) => OperationError::Failed(error(failure)),
                })
        })
    }
}
fn definition() -> ConfiguredToolDefinition {
    ConfiguredToolDefinition::new("configured", "{\"type\":\"object\"}")
}
fn broker() -> Arc<QuestionBroker> {
    Arc::new(QuestionBroker::new(None, Arc::new(Diagnostics)).unwrap())
}
fn question_tool(broker: Arc<QuestionBroker>) -> Arc<QuestionTool<UserQuestions, Security>> {
    Arc::new(QuestionTool::new(
        definition(),
        Arc::new(UserQuestions(broker)),
        None,
    ))
}
async fn wait_pending(broker: &QuestionBroker) {
    for _ in 0..1000 {
        if !broker.pending().is_empty() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("question was not registered");
}

#[tokio::test]
async fn question_maps_schema_input_into_actual_broker_and_formats_label_reply() {
    let broker = broker();
    let tool = question_tool(broker.clone());
    let task = tokio::spawn(async move {
        tool.invoke(invocation(r#"{"questions":[{"header":"Palette","prompt":"Pick a colour","options":["Blue",{"label":"Red","description":"warm"}],"multiple":true,"custom":true}]}"#), context(&NEVER)).await
    });
    wait_pending(&broker).await;
    let pending = broker.pending();
    let questions = &pending[0].questions;
    assert_eq!(questions[0].header, "Palette");
    assert_eq!(questions[0].prompt, "Pick a colour");
    assert_eq!(questions[0].options[0].label, "Blue");
    assert_eq!(questions[0].options[0].description, "");
    assert_eq!(questions[0].options[1].description, "warm");
    assert!(questions[0].multiple && questions[0].custom);
    broker
        .reply(&pending[0].id, QuestionReply::answered(vec!["Blue".into()]))
        .unwrap();
    assert_eq!(
        task.await.unwrap().unwrap(),
        "Question: Pick a colour\nAnswer: Blue"
    );
    assert!(broker.pending().is_empty());
}

#[tokio::test]
async fn ordered_answers_preserve_question_and_supplied_selection_order() {
    let broker = broker();
    let tool = question_tool(broker.clone());
    let task = tokio::spawn(async move {
        tool.invoke(invocation(r#"{"questions":[{"prompt":"First","options":["Red","Blue"],"multiple":true,"custom":true},{"prompt":"Second"}]}"#), context(&NEVER)).await
    });
    wait_pending(&broker).await;
    let pending = broker.pending();
    broker
        .reply(
            &pending[0].id,
            QuestionReply::answered(vec!["Blue, Red, Green".into(), "done".into()]),
        )
        .unwrap();
    assert_eq!(
        task.await.unwrap().unwrap(),
        "Question: First\nAnswer: Blue, Red, Green\n\nQuestion: Second\nAnswer: done"
    );
}

#[tokio::test]
async fn custom_default_distinguishes_absent_null_and_empty_options() {
    for (arguments, expected) in [
        (r#"{"questions":[{"prompt":"Pick"}]}"#, true),
        (r#"{"questions":[{"prompt":"Pick","options":null}]}"#, true),
        (
            r#"{"questions":[{"prompt":"Pick","options":["Blue"]}]}"#,
            false,
        ),
    ] {
        let broker = broker();
        let tool = question_tool(broker.clone());
        let arguments = arguments.to_owned();
        let task =
            tokio::spawn(async move { tool.invoke(invocation(&arguments), context(&NEVER)).await });
        wait_pending(&broker).await;
        let pending = broker.pending();
        assert_eq!(pending[0].questions[0].custom, expected);
        broker.reject(&pending[0].id).unwrap();
        assert_eq!(
            task.await.unwrap().unwrap(),
            "error: question request rejected"
        );
    }
    for arguments in [
        r#"{"questions":[{"prompt":"Pick","custom":false}]}"#,
        r#"{"questions":[{"prompt":"Pick","options":[]}]}"#,
    ] {
        assert_eq!(
            question_tool(broker())
                .invoke(invocation(arguments), context(&NEVER))
                .await
                .unwrap(),
            "error: question requires options or custom answers"
        );
    }
}

#[tokio::test]
async fn wire_rejection_never_leaves_actual_broker_pending() {
    let broker = broker();
    let tool = question_tool(broker.clone());
    for arguments in [
        r#"{"questions":[],"unexpected":true}"#,
        r#"{"questions":[{"id":"colour","prompt":"Pick","options":[{"id":"blue","label":"Blue"}],"unexpected":true}]}"#,
        r#"{"questions":[{"id":"colour","prompt":"Pick","options":[{"id":"blue","label":"Blue","unexpected":true}]}]}"#,
    ] {
        assert!(
            tool.invoke(invocation(arguments), context(&NEVER))
                .await
                .unwrap()
                .starts_with("error: ")
        );
        assert!(broker.pending().is_empty());
    }
    assert_eq!(
        tool.invoke(invocation(r#"{"questions":[]}"#), context(&NEVER))
            .await
            .unwrap(),
        "error: question requests require between 1 and 32 questions"
    );
}

struct StepClock(Mutex<Instant>);
impl Clock for StepClock {
    fn now(&self) -> Instant {
        *self.0.lock().unwrap()
    }
    fn utc_now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH
    }
    fn sleep_until<'a>(
        &'a self,
        deadline: Instant,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Cancelled>> {
        Box::pin(async move {
            cancellation.check()?;
            *self.0.lock().unwrap() = deadline;
            Ok(())
        })
    }
}
#[tokio::test]
async fn actual_broker_timeout_is_user_away_normal_text() {
    let broker = Arc::new(
        QuestionBroker::with_clock(
            Some(Duration::from_secs(20 * 60)),
            Arc::new(StepClock(Mutex::new(Instant::now()))),
            Arc::new(Diagnostics),
        )
        .unwrap(),
    );
    assert_eq!(
        question_tool(broker.clone())
            .invoke(
                invocation(r#"{"questions":[{"prompt":"Pick"}]}"#),
                context(&NEVER)
            )
            .await
            .unwrap(),
        "The user is away."
    );
    assert!(broker.pending().is_empty());
}

#[tokio::test]
async fn question_cancellation_is_not_an_error_result_and_registration_is_removed() {
    let broker = broker();
    assert_eq!(
        question_tool(broker.clone())
            .invoke(
                invocation(r#"{"questions":[{"prompt":"Pick"}]}"#),
                context(&CANCELLED)
            )
            .await,
        Err(OperationError::Cancelled)
    );
    assert!(broker.pending().is_empty());
}

#[derive(Default)]
struct Delivery(Mutex<Vec<PendingAgentAsk>>);
impl AgentAskDelivery for Delivery {
    fn steer<'a>(
        &'a self,
        request: &'a PendingAgentAsk,
        _: String,
    ) -> BoxFuture<'a, Result<(), InteractionError>> {
        Box::pin(async move {
            self.0.lock().unwrap().push(request.clone());
            Ok(())
        })
    }
}
struct Templates;
impl AgentAskTemplates for Templates {
    fn ask(&self, request: &PendingAgentAsk) -> Result<String, InteractionError> {
        Ok(format!("{}: {}", request.id, request.message))
    }
    fn reminder(&self, requests: &[PendingAgentAsk]) -> Result<String, InteractionError> {
        Ok(format!("{} pending asks", requests.len()))
    }
}
struct Communication {
    scope: Arc<AgentAsks>,
    routes: BTreeMap<&'static str, Arc<AgentAsks>>,
}
impl AgentCommunication for Communication {
    type Error = AgentToolError;
    fn ask<'a>(
        &'a self,
        name: &'a str,
        message: String,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<String, OperationError<AgentToolError>>> {
        Box::pin(async move {
            let recipient = self.routes.get(name).ok_or_else(|| {
                OperationError::Failed(AgentToolError::Registry("unknown agent".into()))
            })?;
            self.scope
                .ask(recipient, message, cancellation)
                .await
                .map_err(|failure| match failure {
                    OperationError::Cancelled => OperationError::Cancelled,
                    OperationError::Failed(failure) => OperationError::Failed(error(failure)),
                })
        })
    }
    fn reply(&self, request_id: &str, message: String) -> Result<(), AgentToolError> {
        self.scope.reply(request_id, message).map_err(error)
    }
}
fn register(broker: &Arc<AgentAskBroker>, name: &str) -> Arc<AgentAsks> {
    Arc::new(
        broker
            .register(
                AgentAskIdentity {
                    session_id: name.into(),
                    name: name.into(),
                },
                Arc::new(Delivery::default()),
                Arc::new(Templates),
            )
            .unwrap(),
    )
}
fn communication(scope: Arc<AgentAsks>) -> Arc<Communication> {
    Arc::new(Communication {
        scope,
        routes: BTreeMap::new(),
    })
}
async fn wait_pending_ask(scope: &AgentAsks) -> PendingAgentAsk {
    for _ in 0..1000 {
        if let Some(request) = scope.pending().first() {
            return request.clone();
        }
        tokio::task::yield_now().await;
    }
    panic!("ask was not registered");
}

#[tokio::test]
async fn ask_blocks_until_reply_settles_immediately_and_preserves_plain_text() {
    let broker = Arc::new(AgentAskBroker::new());
    let asker = register(&broker, "child");
    let recipient = register(&broker, "root");
    let other = register(&broker, "sibling");
    let service = Arc::new(Communication {
        scope: asker,
        routes: BTreeMap::from([("parent", recipient.clone()), ("/root", recipient.clone())]),
    });
    for name in ["parent", "/root"] {
        let ask: AskTool<_, Security> = AskTool::new(definition(), service.clone(), None);
        assert!(ask.enabled_after_interruption());
        assert_eq!(ask.name(), "ask");
        assert_eq!(ask.definition(), &definition());
        assert_eq!(
            ask.parallel_safety(&invocation("{}")),
            ParallelSafety::Sequential
        );
        let args = serde_json::json!({"name":name,"message":"  Proceed?\nDetails  "});
        let asking = tokio::spawn(async move {
            ask.invoke(invocation(&args.to_string()), context(&NEVER))
                .await
        });
        let request = wait_pending_ask(&recipient).await;
        assert!(!asking.is_finished());
        assert_eq!(request.message, "  Proceed?\nDetails  ");
        let wrong: ReplyTool<_, Security> =
            ReplyTool::new(definition(), communication(other.clone()), None);
        assert!(matches!(
            wrong
                .invoke(
                    invocation(
                        &serde_json::json!({"request_id":request.id,"message":"yes"}).to_string()
                    ),
                    context(&NEVER)
                )
                .await,
            Err(OperationError::Failed(AgentToolError::QuestionRejected(_)))
        ));
        assert_eq!(recipient.pending().len(), 1);
        let reply: ReplyTool<_, Security> =
            ReplyTool::new(definition(), communication(recipient.clone()), None);
        assert!(reply.enabled_after_interruption());
        assert_eq!(reply.name(), "reply");
        assert_eq!(
            reply.parallel_safety(&invocation("{}")),
            ParallelSafety::Sequential
        );
        let arguments =
            serde_json::json!({"request_id":request.id,"message":"  yes\nnow  "}).to_string();
        assert_eq!(
            reply
                .invoke(invocation(&arguments), context(&NEVER))
                .await
                .unwrap(),
            format!("Replied to ask {}.", request.id)
        );
        assert!(recipient.pending().is_empty());
        assert_eq!(asking.await.unwrap().unwrap(), "  yes\nnow  ");
        assert!(matches!(
            reply.invoke(invocation(&arguments), context(&NEVER)).await,
            Err(OperationError::Failed(AgentToolError::QuestionRejected(_)))
        ));
    }
}

#[tokio::test]
async fn ask_cycle_and_cancellation_are_propagated_without_fabricating_replies() {
    let broker = Arc::new(AgentAskBroker::new());
    let first = register(&broker, "first");
    let second = register(&broker, "second");
    let ask: AskTool<_, Security> = AskTool::new(
        definition(),
        Arc::new(Communication {
            scope: first.clone(),
            routes: BTreeMap::from([("second", second.clone())]),
        }),
        None,
    );
    assert_eq!(
        ask.invoke(
            invocation(r#"{"name":"second","message":"hello"}"#),
            context(&CANCELLED)
        )
        .await,
        Err(OperationError::Cancelled)
    );
    assert!(second.pending().is_empty());
    let asking = tokio::spawn(async move {
        ask.invoke(
            invocation(r#"{"name":"second","message":"hello"}"#),
            context(&NEVER),
        )
        .await
    });
    let request = wait_pending_ask(&second).await;
    let reverse: AskTool<_, Security> = AskTool::new(
        definition(),
        Arc::new(Communication {
            scope: second.clone(),
            routes: BTreeMap::from([("first", first)]),
        }),
        None,
    );
    let cycle = reverse
        .invoke(
            invocation(r#"{"name":"first","message":"cycle"}"#),
            context(&NEVER),
        )
        .await;
    assert!(
        matches!(cycle, Err(OperationError::Failed(AgentToolError::QuestionRejected(ref message))) if message.contains("cycle"))
    );
    let reply: ReplyTool<_, Security> =
        ReplyTool::new(definition(), communication(second.clone()), None);
    assert_eq!(
        reply
            .invoke(invocation("not-json"), context(&CANCELLED))
            .await,
        Err(OperationError::Cancelled)
    );
    assert_eq!(second.pending().len(), 1);
    second.reply(&request.id, "settled".into()).unwrap();
    assert_eq!(asking.await.unwrap().unwrap(), "settled");
}

#[tokio::test]
async fn invalid_ask_and_reply_inputs_do_not_reach_the_service() {
    let broker = Arc::new(AgentAskBroker::new());
    let scope = register(&broker, "agent");
    let ask: AskTool<_, Security> = AskTool::new(definition(), communication(scope.clone()), None);
    let reply: ReplyTool<_, Security> =
        ReplyTool::new(definition(), communication(scope.clone()), None);
    for arguments in [
        "not-json",
        "[]",
        "{}",
        r#"{"name":"x"}"#,
        r#"{"name":"x","message":" "}"#,
        r#"{"name":" ","message":"x"}"#,
        r#"{"name":"x","message":[]}"#,
        r#"{"name":"x","message":"x","extra":true}"#,
    ] {
        assert!(
            ask.invoke(invocation(arguments), context(&NEVER))
                .await
                .unwrap()
                .starts_with("error: "),
            "{arguments}"
        );
    }
    for arguments in [
        "not-json",
        "[]",
        "{}",
        r#"{"request_id":"x"}"#,
        r#"{"request_id":"x","message":" "}"#,
        r#"{"request_id":" ","message":"x"}"#,
        r#"{"request_id":"x","message":[]}"#,
        r#"{"request_id":"x","message":"x","extra":true}"#,
    ] {
        assert!(
            reply
                .invoke(invocation(arguments), context(&NEVER))
                .await
                .unwrap()
                .starts_with("error: "),
            "{arguments}"
        );
    }
    assert!(scope.pending().is_empty());
}

struct Formatting;
impl ResultTemplates for Formatting {
    fn render(&self, name: &str, arguments: &[(&str, &str)]) -> Result<String, AgentToolError> {
        Ok(format!(
            "{name}:{}",
            arguments
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join("|")
        ))
    }
}
#[tokio::test]
async fn question_uses_selected_result_templates() {
    let broker = broker();
    let tool: Arc<QuestionTool<_, Security>> = Arc::new(QuestionTool::new(
        definition(),
        Arc::new(UserQuestions(broker.clone())),
        Some(Arc::new(Formatting)),
    ));
    assert!(tool.enabled_after_interruption());
    assert_eq!(tool.definition(), &definition());
    let task = tokio::spawn(async move {
        tool.invoke(
            invocation(r#"{"questions":[{"prompt":"Pick"}]}"#),
            context(&NEVER),
        )
        .await
    });
    wait_pending(&broker).await;
    broker
        .reply(
            &broker.pending()[0].id,
            QuestionReply::answered(vec!["yes".into()]),
        )
        .unwrap();
    assert_eq!(
        task.await.unwrap().unwrap(),
        "tool-result.question-answer:question=Pick|answer=yes"
    );
}

#[tokio::test]
async fn ask_and_reply_use_selected_result_templates() {
    let broker = Arc::new(AgentAskBroker::new());
    let first = register(&broker, "first");
    let second = register(&broker, "second");
    let ask: AskTool<_, Security> = AskTool::new(
        definition(),
        Arc::new(Communication {
            scope: first,
            routes: BTreeMap::from([("second", second.clone())]),
        }),
        Some(Arc::new(Formatting)),
    );
    let asking = tokio::spawn(async move {
        ask.invoke(
            invocation(r#"{"name":"second","message":"hello"}"#),
            context(&NEVER),
        )
        .await
    });
    let request = wait_pending_ask(&second).await;
    let reply: ReplyTool<_, Security> = ReplyTool::new(
        definition(),
        communication(second),
        Some(Arc::new(Formatting)),
    );
    assert_eq!(
        reply
            .invoke(
                invocation(
                    &serde_json::json!({"request_id":request.id,"message":"yes"}).to_string()
                ),
                context(&NEVER)
            )
            .await
            .unwrap(),
        format!("tool-result.ask-replied:request_id={}", request.id)
    );
    assert_eq!(
        asking.await.unwrap().unwrap(),
        "tool-result.ask-reply:message=yes"
    );
}
