use std::marker::PhantomData;
use std::sync::Arc;

use parrot_domain::contracts::async_ops::{BoxFuture, OperationError};
use parrot_domain::contracts::interactions::{
    ChildQuestionAnswers, QuestionDefinition, QuestionOption, QuestionReply, QuestionReplyKind,
    QuestionRequester,
};
use parrot_domain::contracts::security::{AccessSnapshot, InvocationContext};
use parrot_domain::contracts::tools::{ParallelSafety, Tool};
use parrot_domain::values::tools::ConfiguredToolDefinition;

use super::input::{parse_answers, parse_questions};
use super::{AgentSelection, AgentToolError, ResultTemplates, ToolInvocation, render};

pub struct QuestionTool<Q, S> {
    definition: ConfiguredToolDefinition,
    requester: Arc<Q>,
    templates: Option<Arc<dyn ResultTemplates>>,
    security: PhantomData<fn() -> S>,
}
impl<Q, S> QuestionTool<Q, S> {
    pub fn new(
        definition: ConfiguredToolDefinition,
        requester: Arc<Q>,
        templates: Option<Arc<dyn ResultTemplates>>,
    ) -> Self {
        Self {
            definition,
            requester,
            templates,
            security: PhantomData,
        }
    }
    pub fn name(&self) -> &'static str {
        "question"
    }
}
impl<Q, S> Tool for QuestionTool<Q, S>
where
    Q: QuestionRequester<Error = AgentToolError>,
    S: AccessSnapshot + 'static,
{
    type Definition = ConfiguredToolDefinition;
    type Invocation = ToolInvocation;
    type Selection = AgentSelection;
    type Security = S;
    type Output = String;
    type Error = AgentToolError;

    fn definition(&self) -> &Self::Definition {
        &self.definition
    }
    fn parallel_safety(&self, _: &Self::Invocation) -> ParallelSafety {
        ParallelSafety::Sequential
    }
    fn enabled_after_interruption(&self) -> bool {
        true
    }
    fn invoke<'a>(
        &'a self,
        invocation: Self::Invocation,
        context: InvocationContext<'a, S, AgentSelection>,
    ) -> BoxFuture<'a, Result<String, OperationError<AgentToolError>>> {
        Box::pin(async move {
            let wire = match parse_questions(&invocation.arguments_json) {
                Ok(wire) => wire,
                Err(error) => return error_result(self.templates.as_deref(), &error.0),
            };
            let questions: Vec<_> = wire
                .into_iter()
                .map(|question| {
                    let custom = question.custom.unwrap_or(question.options.is_none());
                    QuestionDefinition {
                        header: question.header.unwrap_or_default(),
                        prompt: question.prompt.unwrap_or_default(),
                        options: question
                            .options
                            .unwrap_or_default()
                            .into_iter()
                            .map(|option| QuestionOption {
                                label: option.label,
                                description: option.description,
                            })
                            .collect(),
                        multiple: question.multiple,
                        custom,
                    }
                })
                .collect();
            match self
                .requester
                .ask(questions.clone(), context.cancellation())
                .await
            {
                Ok(reply) if reply.kind == QuestionReplyKind::UserAway => render(
                    self.templates.as_deref(),
                    "tool-result.text",
                    &[("value", "The user is away.")],
                    || "The user is away.".into(),
                )
                .map_err(OperationError::Failed),
                Ok(reply) => questions
                    .iter()
                    .zip(&reply.answers)
                    .map(|(question, answer)| {
                        render(
                            self.templates.as_deref(),
                            "tool-result.question-answer",
                            &[("question", &question.prompt), ("answer", answer)],
                            || format!("Question: {}\nAnswer: {answer}", question.prompt),
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map(|answers| answers.join("\n\n"))
                    .map_err(OperationError::Failed),
                Err(OperationError::Failed(error)) if caught(&error) => {
                    error_result(self.templates.as_deref(), &error.to_string())
                }
                Err(error) => Err(error),
            }
        })
    }
}

pub struct AnswerTool<A, S> {
    definition: ConfiguredToolDefinition,
    answers: Arc<A>,
    templates: Option<Arc<dyn ResultTemplates>>,
    security: PhantomData<fn() -> S>,
}
impl<A, S> AnswerTool<A, S> {
    pub fn new(
        definition: ConfiguredToolDefinition,
        answers: Arc<A>,
        templates: Option<Arc<dyn ResultTemplates>>,
    ) -> Self {
        Self {
            definition,
            answers,
            templates,
            security: PhantomData,
        }
    }
    pub fn name(&self) -> &'static str {
        "answer"
    }
}
impl<A, S> Tool for AnswerTool<A, S>
where
    A: ChildQuestionAnswers<Error = AgentToolError>,
    S: AccessSnapshot + 'static,
{
    type Definition = ConfiguredToolDefinition;
    type Invocation = ToolInvocation;
    type Selection = AgentSelection;
    type Security = S;
    type Output = String;
    type Error = AgentToolError;

    fn definition(&self) -> &Self::Definition {
        &self.definition
    }
    fn parallel_safety(&self, _: &Self::Invocation) -> ParallelSafety {
        ParallelSafety::Sequential
    }
    fn enabled_after_interruption(&self) -> bool {
        true
    }
    fn invoke<'a>(
        &'a self,
        invocation: Self::Invocation,
        context: InvocationContext<'a, S, AgentSelection>,
    ) -> BoxFuture<'a, Result<String, OperationError<AgentToolError>>> {
        Box::pin(async move {
            context
                .cancellation()
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            let (agent_name, answers) = match parse_answers(&invocation.arguments_json) {
                Ok(input) => input,
                Err(error) => return error_result(self.templates.as_deref(), &error.0),
            };
            match self
                .answers
                .reply(&agent_name, QuestionReply::answered(answers))
            {
                Ok(()) => render(
                    self.templates.as_deref(),
                    "tool-result.question-replied",
                    &[("agent_name", &agent_name)],
                    || format!("Answered the pending question from child agent {agent_name}."),
                )
                .map_err(OperationError::Failed),
                Err(error) if caught(&error) => {
                    error_result(self.templates.as_deref(), &error.to_string())
                }
                Err(error) => Err(OperationError::Failed(error)),
            }
        })
    }
}

fn caught(error: &AgentToolError) -> bool {
    matches!(
        error,
        AgentToolError::Registry(_)
            | AgentToolError::Question(_)
            | AgentToolError::QuestionRejected(_)
    )
}
fn error_result(
    templates: Option<&dyn ResultTemplates>,
    message: &str,
) -> Result<String, OperationError<AgentToolError>> {
    render(
        templates,
        "tool-result.error",
        &[("message", message)],
        || format!("error: {message}"),
    )
    .map_err(OperationError::Failed)
}
