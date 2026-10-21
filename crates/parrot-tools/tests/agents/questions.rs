use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use parrot_domain::contracts::async_ops::{
    BoxFuture, Cancellation, Cancelled, Clock, OperationError,
};
use parrot_domain::contracts::interactions::{
    ChildQuestionAnswers, QuestionDefinition, QuestionReply, QuestionRequester,
};
use parrot_domain::contracts::tools::{ParallelSafety, Tool};
use parrot_domain::values::tools::ConfiguredToolDefinition;
use parrot_runtime::interactions::{
    BrokerDiagnostic, ChildIdentity, ChildQuestion, ChildQuestionCoordinator,
    ChildQuestionDelivery, ChildQuestionRegistry, ChildQuestionTemplates, InteractionDiagnostics,
    InteractionError, InteractionErrorKind, QuestionBroker,
};
use parrot_tools::agents::questions::{AnswerTool, QuestionTool};
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

struct Children(Vec<Arc<ChildQuestion>>);
impl ChildQuestionRegistry for Children {
    fn find(&self, name: &str) -> Option<Arc<ChildQuestion>> {
        self.0
            .iter()
            .find(|child| child.identity.name == name)
            .cloned()
    }
    fn snapshot(&self) -> Vec<Arc<ChildQuestion>> {
        self.0.clone()
    }
}
#[derive(Default)]
struct Delivery(Mutex<Vec<String>>);
impl ChildQuestionDelivery for Delivery {
    fn steer<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        text: String,
    ) -> BoxFuture<'a, Result<(), InteractionError>> {
        Box::pin(async move {
            self.0.lock().unwrap().push(text);
            Ok(())
        })
    }
}
struct Templates;
impl ChildQuestionTemplates for Templates {
    fn question(&self, name: &str, questions: &str) -> Result<String, InteractionError> {
        Ok(format!("{name}: {questions}"))
    }
    fn reminder(&self, children: &str) -> Result<String, InteractionError> {
        Ok(children.into())
    }
}
struct ChildRequester {
    coordinator: Arc<ChildQuestionCoordinator>,
    identity: Arc<ChildIdentity>,
}
impl QuestionRequester for ChildRequester {
    type Error = AgentToolError;
    fn ask<'a>(
        &'a self,
        questions: Vec<QuestionDefinition>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<QuestionReply, OperationError<AgentToolError>>> {
        Box::pin(async move {
            self.coordinator
                .ask(self.identity.clone(), questions, cancellation)
                .await
                .map_err(|failure| match failure {
                    OperationError::Cancelled => OperationError::Cancelled,
                    OperationError::Failed(failure) => OperationError::Failed(error(failure)),
                })
        })
    }
}
struct ChildAnswers {
    coordinator: Arc<ChildQuestionCoordinator>,
    children: Arc<Children>,
    parent: String,
}
impl ChildQuestionAnswers for ChildAnswers {
    type Error = AgentToolError;
    fn reply(&self, name: &str, reply: QuestionReply) -> Result<(), AgentToolError> {
        self.coordinator
            .reply_from_parent(&self.parent, self.children.as_ref(), name, reply)
            .map_err(error)
    }
}

#[tokio::test]
async fn real_child_question_answer_routing_rejects_other_parent_and_preserves_pending_on_bad_answers()
 {
    let identity = Arc::new(ChildIdentity {
        session_id: "child-id".into(),
        name: "worker".into(),
        parent_session_id: "parent-id".into(),
    });
    let child = Arc::new(ChildQuestion::new(identity.clone()));
    let children = Arc::new(Children(vec![child]));
    let delivery = Arc::new(Delivery::default());
    let coordinator = Arc::new(ChildQuestionCoordinator::new(
        "parent-id".into(),
        children.clone(),
        delivery.clone(),
        Arc::new(Templates),
    ));
    let user_broker = broker();
    let question: Arc<QuestionTool<_, Security>> = Arc::new(QuestionTool::new(
        definition(),
        Arc::new(ChildRequester {
            coordinator: coordinator.clone(),
            identity,
        }),
        None,
    ));
    let asking = tokio::spawn(async move {
        question
            .invoke(
                invocation(r#"{"questions":[{"prompt":"Proceed?","options":["yes","no"]}]}"#),
                context(&NEVER),
            )
            .await
    });
    for _ in 0..1000 {
        if !coordinator.pending().is_empty() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(coordinator.pending().len(), 1);
    assert!(user_broker.pending().is_empty());
    assert_eq!(delivery.0.lock().unwrap().len(), 1);
    let wrong: AnswerTool<_, Security> = AnswerTool::new(
        definition(),
        Arc::new(ChildAnswers {
            coordinator: coordinator.clone(),
            children: children.clone(),
            parent: "other-parent".into(),
        }),
        None,
    );
    assert!(
        wrong
            .invoke(
                invocation(r#"{"agent_name":"worker","answers":["yes"]}"#),
                context(&NEVER)
            )
            .await
            .unwrap()
            .starts_with("error: ")
    );
    assert_eq!(coordinator.pending().len(), 1);
    let answers: AnswerTool<_, Security> = AnswerTool::new(
        definition(),
        Arc::new(ChildAnswers {
            coordinator: coordinator.clone(),
            children,
            parent: "parent-id".into(),
        }),
        None,
    );
    for arguments in [
        "not-json",
        "{}",
        r#"{"agent_name":"worker"}"#,
        r#"{"agent_name":"worker","answers":[]}"#,
        r#"{"agent_name":"worker","answers":[null]}"#,
        r#"{"agent_name":"worker","answers":[""]}"#,
        r#"{"agent_name":"worker","answers":["yes","no"]}"#,
        r#"{"agent_name":"worker","answers":[{"text":"yes"}]}"#,
        r#"{"agent_name":"worker","answers":["yes"],"extra":true}"#,
    ] {
        assert!(
            answers
                .invoke(invocation(arguments), context(&NEVER))
                .await
                .unwrap()
                .starts_with("error: "),
            "{arguments}"
        );
        assert_eq!(coordinator.pending().len(), 1);
    }
    assert_eq!(
        answers
            .invoke(
                invocation(r#"{"agent_name":"worker","answers":[""]}"#),
                context(&NEVER)
            )
            .await
            .unwrap(),
        "error: question answers cannot be empty"
    );
    assert_eq!(
        answers
            .invoke(invocation("not-json"), context(&CANCELLED))
            .await,
        Err(OperationError::Cancelled)
    );
    assert_eq!(coordinator.pending().len(), 1);
    assert_eq!(
        answers
            .invoke(
                invocation(r#"{"agent_name":"worker","answers":["yes"]}"#),
                context(&NEVER)
            )
            .await
            .unwrap(),
        "Answered the pending question from child agent worker."
    );
    assert_eq!(
        asking.await.unwrap().unwrap(),
        "Question: Proceed?\nAnswer: yes"
    );
    assert!(coordinator.pending().is_empty());
    assert!(
        answers
            .invoke(
                invocation(r#"{"agent_name":"worker","answers":["yes"]}"#),
                context(&NEVER)
            )
            .await
            .unwrap()
            .starts_with("error: ")
    );
    assert!(answers.enabled_after_interruption());
    assert_eq!(
        answers.parallel_safety(&invocation("{}")),
        ParallelSafety::Sequential
    );
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
