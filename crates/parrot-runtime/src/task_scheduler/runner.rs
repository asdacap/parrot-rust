use super::{
    TaskConfiguration, TaskRunProgress, TaskSession, ports::render, values::serialize_tasks,
};
use crate::{
    agent_turn::lifecycle::ExecutionStatus as SessionStatus,
    children::{ChildError, ChildResult},
};
use parrot_domain::{
    contracts::{agent_tasks::TaskArtifactParser, async_ops::Cancellation},
    values::{
        agent_state::{TaskRunnerCheckpoint, TaskRunnerPhase},
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
    pub fn begin(
        &self,
        task: Task,
        siblings: Vec<Task>,
        dependencies: Vec<Task>,
    ) -> ChildResult<TaskRunnerCheckpoint> {
        let assignment_header = self.header(&task, &siblings, &dependencies)?;
        let prompt = self.build_assignment(&task, assignment_header.clone(), &[], None)?;
        Ok(TaskRunnerCheckpoint {
            original_task: task.clone(),
            current_task: task,
            feedback: Vec::new(),
            carried_result: None,
            attempt: 1,
            repair_attempt: 0,
            phase: TaskRunnerPhase::PromptIntent,
            generation_id: Identifier::event_id().map_err(identifier_error)?,
            prompt_message_id: Identifier::message_id().map_err(identifier_error)?,
            prompt,
            maximum_attempts: self.configuration.maximum_attempts as u64,
            maximum_response_repairs: self.configuration.maximum_response_repairs as u64,
            assignment_header,
            outcome: None,
            response_reference: None,
        })
    }
    pub async fn run(
        &self,
        mut checkpoint: TaskRunnerCheckpoint,
        session: &dyn TaskSession,
        progress: &dyn TaskRunProgress,
        cancellation: Arc<dyn Cancellation>,
    ) -> ChildResult<Task> {
        if let Some(outcome) = checkpoint.outcome.clone() {
            return Ok(outcome);
        }
        loop {
            let response = match self
                .run_and_parse(session, &mut checkpoint, progress, cancellation.clone())
                .await?
            {
                Ok(response) => response,
                Err(failure) => {
                    let task = outcome(
                        checkpoint.original_task.clone(),
                        ExecutionStatus::Failed,
                        checkpoint.carried_result.clone(),
                        Some(failure),
                    );
                    return self.finish(checkpoint, task, progress).await;
                }
            };
            let result = self.bound(&response.result, MAX_CONTEXT_CHARACTERS)?;
            let (retry_feedback, replacement_instruction, replacement_result) =
                match response.verdict {
                    AcceptanceVerdict::Accept => {
                        let task = outcome(
                            checkpoint.original_task.clone(),
                            ExecutionStatus::Succeeded,
                            Some(result),
                            None,
                        );
                        return self.finish(checkpoint, task, progress).await;
                    }
                    AcceptanceVerdict::RejectAndHalt { feedback } => {
                        let task = outcome(
                            checkpoint.original_task.clone(),
                            ExecutionStatus::Failed,
                            Some(result),
                            Some(self.bound(&feedback, MAX_SUMMARY_CHARACTERS)?),
                        );
                        return self.finish(checkpoint, task, progress).await;
                    }
                    AcceptanceVerdict::RejectAndRetry {
                        feedback,
                        payload,
                        replacement_result,
                    } => (feedback, payload, replacement_result),
                };
            let retry_feedback = self.bound(&retry_feedback, MAX_SUMMARY_CHARACTERS)?;
            checkpoint.carried_result = Some(self.bound(
                replacement_result.as_deref().unwrap_or(&response.result),
                MAX_CONTEXT_CHARACTERS,
            )?);
            if checkpoint.attempt >= checkpoint.maximum_attempts {
                let failure = render(
                    &self.configuration,
                    "agent-task.attempts-exhausted",
                    &[
                        ("attempts", &checkpoint.attempt.to_string()),
                        ("feedback", &retry_feedback),
                    ],
                )?;
                let task = outcome(
                    checkpoint.original_task.clone(),
                    ExecutionStatus::Failed,
                    checkpoint.carried_result.clone(),
                    Some(failure),
                );
                return self.finish(checkpoint, task, progress).await;
            }
            checkpoint.feedback.push(retry_feedback);
            if let (Some(instruction), TaskPayload::Instruction(_)) =
                (replacement_instruction, &checkpoint.current_task.payload)
            {
                checkpoint.current_task.payload = TaskPayload::Instruction(instruction);
            }
            checkpoint.attempt += 1;
            checkpoint.repair_attempt = 0;
            checkpoint.prompt = self.build_assignment(
                &checkpoint.current_task,
                checkpoint.assignment_header.clone(),
                &checkpoint.feedback,
                checkpoint.carried_result.as_deref(),
            )?;
            checkpoint.prompt_message_id = Identifier::message_id().map_err(identifier_error)?;
            checkpoint.phase = TaskRunnerPhase::RetryIntent;
            checkpoint.response_reference = None;
            progress.save(checkpoint.clone()).await?;
        }
    }
    async fn finish(
        &self,
        mut checkpoint: TaskRunnerCheckpoint,
        task: Task,
        progress: &dyn TaskRunProgress,
    ) -> ChildResult<Task> {
        checkpoint.phase = TaskRunnerPhase::Completed;
        checkpoint.outcome = Some(task.clone());
        progress.save(checkpoint).await?;
        Ok(task)
    }
    async fn run_and_parse(
        &self,
        session: &dyn TaskSession,
        checkpoint: &mut TaskRunnerCheckpoint,
        progress: &dyn TaskRunProgress,
        cancellation: Arc<dyn Cancellation>,
    ) -> ChildResult<Result<LeafResponse, String>> {
        loop {
            if cancellation.is_cancelled() {
                return Err(ChildError::Cancelled);
            }
            let claim = progress.claim(&checkpoint.prompt_message_id);
            let execution = if let Some(reference) = &checkpoint.response_reference {
                let execution = session.restore_response(reference).await?;
                session.acknowledge_result(reference, &claim).await?;
                progress.reserve();
                execution
            } else {
                let lease = progress.acquire(checkpoint, session).await?;
                let handles = self
                    .reserve_prompt(session, checkpoint, progress, cancellation.clone())
                    .await?;
                handles.reservation.join().await?;
                progress.reserve();
                checkpoint.phase = TaskRunnerPhase::AwaitingResponse;
                progress.save(checkpoint.clone()).await?;
                let execution = handles.execution.join().await?;
                if cancellation.is_cancelled() {
                    return Err(ChildError::Cancelled);
                }
                let reference = session
                    .capture_result(&checkpoint.prompt_message_id, &claim)
                    .await?;
                checkpoint.phase = TaskRunnerPhase::AssessingResponse;
                checkpoint.response_reference = Some(reference.clone());
                progress.save(checkpoint.clone()).await?;
                session.acknowledge_result(&reference, &claim).await?;
                drop(lease);
                execution
            };
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
                Err(error) if checkpoint.repair_attempt < checkpoint.maximum_response_repairs => {
                    checkpoint.repair_attempt += 1;
                    checkpoint.prompt = render(
                        &self.configuration,
                        "agent-task.response-repair",
                        &[("error", &error.to_string())],
                    )?;
                    checkpoint.prompt_message_id =
                        Identifier::message_id().map_err(identifier_error)?;
                    checkpoint.phase = TaskRunnerPhase::RepairingResponse;
                    checkpoint.response_reference = None;
                    progress.save(checkpoint.clone()).await?;
                }
                Err(error) => return Ok(Err(format!("leaf response invalid: {error}"))),
            }
        }
    }
    async fn reserve_prompt(
        &self,
        session: &dyn TaskSession,
        checkpoint: &TaskRunnerCheckpoint,
        progress: &dyn TaskRunProgress,
        cancellation: Arc<dyn Cancellation>,
    ) -> ChildResult<super::TaskExecution> {
        loop {
            if cancellation.is_cancelled() {
                return Err(ChildError::Cancelled);
            }
            progress.save(checkpoint.clone()).await?;
            let claim = progress.claim(&checkpoint.prompt_message_id);
            let saved = session.capture_execution().await?;
            if let Some(saved) = saved {
                if saved.initial_message_id == checkpoint.prompt_message_id {
                    if saved.result_claim.as_ref() != Some(&claim) {
                        return Err(ChildError::registry(
                            "task prompt lost its protected result identity",
                        ));
                    }
                    return session.enqueue_claimed(
                        vec![ConversationPart::text(checkpoint.prompt.clone())],
                        checkpoint.prompt_message_id.clone(),
                        claim,
                        cancellation,
                    );
                }
                if matches!(
                    checkpoint.phase,
                    TaskRunnerPhase::AwaitingResponse | TaskRunnerPhase::AssessingResponse
                ) {
                    return Err(ChildError::registry(
                        "admitted task prompt no longer matches worker execution",
                    ));
                }
                if saved.terminal.is_none() {
                    let predecessor = session.recover()?;
                    predecessor.reservation.wait(cancellation.as_ref()).await?;
                    progress.reserve();
                    predecessor.execution.wait(cancellation.as_ref()).await?;
                    continue;
                }
            } else if matches!(
                checkpoint.phase,
                TaskRunnerPhase::AwaitingResponse | TaskRunnerPhase::AssessingResponse
            ) {
                return Err(ChildError::registry(
                    "admitted task prompt has no worker execution",
                ));
            }
            return session.enqueue_claimed(
                vec![ConversationPart::text(checkpoint.prompt.clone())],
                checkpoint.prompt_message_id.clone(),
                claim,
                cancellation,
            );
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
        self.build_assignment(
            task,
            self.header(task, siblings, dependencies)?,
            feedback,
            carried_result,
        )
    }
    fn build_assignment(
        &self,
        task: &Task,
        mut prompt: String,
        feedback: &[String],
        carried_result: Option<&str>,
    ) -> ChildResult<String> {
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
        let dependencies = dependencies
            .iter()
            .map(|dependency| {
                Ok(serde_json::json!({
                    "name": dependency.name,
                    "description": dependency.description,
                    "result": self.bound(
                        dependency.result.as_deref().unwrap_or(""),
                        MAX_SUMMARY_CHARACTERS,
                    )?,
                }))
            })
            .collect::<ChildResult<Vec<_>>>()?;
        let siblings = siblings
            .iter()
            .map(|sibling| {
                Ok(serde_json::json!({
                    "name": sibling.name,
                    "description": self.bound(&sibling.description, MAX_SUMMARY_CHARACTERS)?,
                }))
            })
            .collect::<ChildResult<Vec<_>>>()?;
        self.configuration
            .templates
            .render(
                "agent-task.assignment-context",
                &[
                    ("task_name".into(), task.name.clone().into()),
                    ("description".into(), task.description.clone().into()),
                    (
                        "acceptance_criteria".into(),
                        task.acceptance_criteria.clone().into(),
                    ),
                    ("dependencies".into(), dependencies.into()),
                    ("siblings".into(), siblings.into()),
                ],
                &parrot_storage::events::NeverCancelled,
            )
            .map_err(|error| ChildError::failure("argument", error.to_string()))
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

fn identifier_error(error: std::io::Error) -> ChildError {
    ChildError::failure("unexpected", error.to_string())
}
