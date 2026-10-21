use super::{TaskConfiguration, TaskSession, ports::render, values::serialize_tasks};
use crate::{
    agent_turn::lifecycle::ExecutionStatus as SessionStatus,
    children::{ChildError, ChildResult},
};
use parrot_domain::{
    contracts::{agent_tasks::TaskArtifactParser, async_ops::Cancellation},
    values::{
        agent_tasks::{AcceptanceVerdict, ExecutionStatus, LeafResponse, Task, TaskPayload},
        identifiers::Identifier,
    },
};
use parrot_storage::events::ConversationPart;
use std::sync::Arc;

const MAX_CONTEXT_CHARACTERS: usize = 16 * 1024;
const MAX_SUMMARY_CHARACTERS: usize = 16 * 1024;
const MAX_PROMPT_CHARACTERS: usize = 256 * 1024;

/// Runs one retained worker's attempts; a composite's inner list is prompt data only.
pub struct TaskRunner<P: TaskArtifactParser + 'static> {
    configuration: TaskConfiguration,
    parser: Arc<P>,
}
impl<P: TaskArtifactParser + 'static> TaskRunner<P> {
    pub fn new(configuration: TaskConfiguration, parser: Arc<P>) -> Self {
        Self {
            configuration,
            parser,
        }
    }
    pub async fn run(
        &self,
        task: Task,
        siblings: &[Task],
        dependencies: &[Task],
        session: &dyn TaskSession,
        cancellation: Arc<dyn Cancellation>,
    ) -> ChildResult<Task> {
        let mut feedback = Vec::new();
        let mut carried_result = None;
        let mut current = task.clone();
        for attempt in 1usize.. {
            let prompt = self.build_prompt(
                &current,
                siblings,
                dependencies,
                &feedback,
                carried_result.as_deref(),
            )?;
            let response = match self
                .run_and_parse(session, prompt, cancellation.clone())
                .await?
            {
                Ok(response) => response,
                Err(failure) => {
                    return Ok(outcome(
                        task,
                        ExecutionStatus::Failed,
                        carried_result,
                        Some(failure),
                    ));
                }
            };
            let result = self.bound(&response.result, MAX_CONTEXT_CHARACTERS)?;
            let (retry_feedback, replacement_instruction, replacement_result) =
                match response.verdict {
                    AcceptanceVerdict::Accept => {
                        return Ok(outcome(
                            task,
                            ExecutionStatus::Succeeded,
                            Some(result),
                            None,
                        ));
                    }
                    AcceptanceVerdict::RejectAndHalt { feedback } => {
                        return Ok(outcome(
                            task,
                            ExecutionStatus::Failed,
                            Some(result),
                            Some(self.bound(&feedback, MAX_SUMMARY_CHARACTERS)?),
                        ));
                    }
                    AcceptanceVerdict::RejectAndRetry {
                        feedback,
                        payload,
                        replacement_result,
                    } => (feedback, payload, replacement_result),
                };
            let retry_feedback = self.bound(&retry_feedback, MAX_SUMMARY_CHARACTERS)?;
            carried_result = Some(self.bound(
                replacement_result.as_deref().unwrap_or(&response.result),
                MAX_CONTEXT_CHARACTERS,
            )?);
            if attempt >= self.configuration.maximum_attempts {
                let failure = render(
                    &self.configuration,
                    "agent-task.attempts-exhausted",
                    &[
                        ("attempts", &attempt.to_string()),
                        ("feedback", &retry_feedback),
                    ],
                )?;
                return Ok(outcome(
                    task,
                    ExecutionStatus::Failed,
                    carried_result,
                    Some(failure),
                ));
            }
            feedback.push(retry_feedback);
            if let (Some(instruction), TaskPayload::Instruction(_)) =
                (replacement_instruction, &current.payload)
            {
                current.payload = TaskPayload::Instruction(instruction);
            }
        }
        unreachable!("the unbounded attempt loop always returns or retries")
    }
    async fn run_and_parse(
        &self,
        session: &dyn TaskSession,
        mut prompt: String,
        cancellation: Arc<dyn Cancellation>,
    ) -> ChildResult<Result<LeafResponse, String>> {
        let mut repairs = 0;
        loop {
            if cancellation.is_cancelled() {
                return Err(ChildError::Cancelled);
            }
            let id = Identifier::message_id()
                .map_err(|error| ChildError::failure("unexpected", error.to_string()))?;
            let execution = session
                .send_and_wait(
                    vec![ConversationPart::text(prompt)],
                    id,
                    cancellation.clone(),
                )
                .await?;
            if cancellation.is_cancelled() {
                return Err(ChildError::Cancelled);
            }
            if execution.status != SessionStatus::Succeeded {
                return Ok(Err(render(
                    &self.configuration,
                    "agent-task.role-failure",
                    &[
                        ("role", "execute"),
                        (
                            "status",
                            match execution.status {
                                SessionStatus::Failed => "failed",
                                SessionStatus::Canceled => "canceled",
                                SessionStatus::Succeeded => "succeeded",
                            },
                        ),
                        ("error", &execution.error),
                    ],
                )?));
            }
            match self.parser.parse_leaf_response(&execution.output) {
                Ok(response) => return Ok(Ok(response)),
                Err(error) if repairs < self.configuration.maximum_response_repairs => {
                    repairs += 1;
                    prompt = render(
                        &self.configuration,
                        "agent-task.response-repair",
                        &[("error", &error.to_string())],
                    )?;
                }
                Err(error) => return Ok(Err(format!("leaf response invalid: {error}"))),
            }
        }
    }
    pub fn build_prompt(
        &self,
        task: &Task,
        siblings: &[Task],
        dependencies: &[Task],
        feedback: &[String],
        carried_result: Option<&str>,
    ) -> ChildResult<String> {
        let mut prompt = self.header(task, siblings, dependencies)?;
        if let Some(result) = carried_result {
            prompt.push_str(&render(
                &self.configuration,
                "agent-task.result",
                &[("result", result)],
            )?);
        }
        if !feedback.is_empty() {
            let mut items = String::new();
            for item in feedback {
                items.push_str("\n- ");
                items.push_str(item);
            }
            prompt.push_str(&render(
                &self.configuration,
                "agent-task.feedback",
                &[("items", &items)],
            )?);
        }
        let suffix = match &task.payload {
            TaskPayload::Instruction(instruction) => render(
                &self.configuration,
                "agent-task.leaf",
                &[
                    ("header", ""),
                    ("feedback", ""),
                    ("instruction", instruction),
                ],
            )?,
            TaskPayload::Tasks(tasks) => render(
                &self.configuration,
                "agent-task.composite",
                &[
                    ("header", ""),
                    ("feedback", ""),
                    ("children", &serialize_tasks(tasks)),
                ],
            )?,
        };
        let available = MAX_PROMPT_CHARACTERS.saturating_sub(suffix.encode_utf16().count());
        if available == 0 {
            return Ok(suffix);
        }
        Ok(self.bound(&prompt, available)? + &suffix)
    }
    fn header(&self, task: &Task, siblings: &[Task], dependencies: &[Task]) -> ChildResult<String> {
        let mut dependency_text = String::new();
        if !dependencies.is_empty() {
            dependency_text.push_str(&render(
                &self.configuration,
                "agent-task.dependency-header",
                &[],
            )?);
            for dependency in dependencies {
                dependency_text.push_str(&render(
                    &self.configuration,
                    "agent-task.dependency-item",
                    &[
                        ("name", &dependency.name),
                        ("description", &dependency.description),
                        (
                            "result",
                            &self.bound(
                                dependency.result.as_deref().unwrap_or(""),
                                MAX_SUMMARY_CHARACTERS,
                            )?,
                        ),
                    ],
                )?);
            }
        }
        let mut sibling_text = String::new();
        for sibling in siblings {
            sibling_text.push_str(&render(
                &self.configuration,
                "agent-task.sibling-item",
                &[
                    ("name", &sibling.name),
                    (
                        "description",
                        &self.bound(&sibling.description, MAX_SUMMARY_CHARACTERS)?,
                    ),
                ],
            )?);
        }
        let mut header = if siblings.is_empty() {
            String::new()
        } else {
            render(
                &self.configuration,
                "agent-task.sibling-scope",
                &[("items", &sibling_text)],
            )?
        };
        header.push_str(&render(
            &self.configuration,
            "agent-task.header",
            &[
                ("task_name", &task.name),
                ("description", &task.description),
                ("acceptance_criteria", &task.acceptance_criteria),
                ("dependencies", &dependency_text),
            ],
        )?);
        Ok(header)
    }
    fn bound(&self, text: &str, limit: usize) -> ChildResult<String> {
        if text.encode_utf16().count() <= limit {
            return Ok(text.to_owned());
        }
        let prefix = String::from_utf16_lossy(&text.encode_utf16().take(limit).collect::<Vec<_>>());
        render(
            &self.configuration,
            "agent-task.truncated",
            &[("value", &prefix)],
        )
    }
}
fn outcome(
    mut task: Task,
    state: ExecutionStatus,
    result: Option<String>,
    failure: Option<String>,
) -> Task {
    task.state = state;
    task.result = result;
    task.failure = failure;
    task
}
