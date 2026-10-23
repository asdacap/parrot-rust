use std::{marker::PhantomData, sync::Arc};

use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, OperationError, OperationResult},
        filesystem::{AuthorizedReadService, FileReadErrorKind},
        runtime::TaskController,
        security::InvocationContext,
        tools::{ParallelSafety, Tool},
    },
    values::{
        agent_tasks::{ExecutionStatus, Task, TaskCallIdentity, TaskPayload},
        tools::{ConfiguredToolDefinition, MissingToolDefinition, ToolDefinitionCatalog},
    },
};

use crate::task_artifacts::{self, ArtifactReadError, BeforeToolBatch, TaskInputError};

use super::{FailureKind, InputObject, ToolFailure, ToolInvocation, ToolOutput};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskToolKind {
    Set,
    Get,
}

impl TaskToolKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Set => "set_agent_tasks",
            Self::Get => "get_agent_tasks",
        }
    }
}

pub struct TaskUpdate<Selection> {
    pub tasks: Vec<Task>,
    pub selection: Selection,
    pub history_boundary: BeforeToolBatch,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskQuery {
    pub name: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskDetail {
    pub task: Task,
    pub agent_name: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskSnapshot {
    pub tasks: Vec<Task>,
    pub detail: Option<TaskDetail>,
}

pub struct TaskTool<Controller, Reader, Render, Selection> {
    kind: TaskToolKind,
    definition: ConfiguredToolDefinition,
    controller: Arc<Controller>,
    reader: Arc<Reader>,
    render_accepted: Arc<Render>,
    selection: PhantomData<fn(Selection)>,
}

impl<C, R, P, S> TaskTool<C, R, P, S>
where
    C: TaskController<
            Update = TaskUpdate<S>,
            Query = TaskQuery,
            Snapshot = TaskSnapshot,
            Error = ToolFailure,
        >,
    R: AuthorizedReadService,
    P: Fn(usize) -> Result<String, ToolFailure> + Send + Sync,
    S: Clone + Send + 'static,
{
    pub fn new(
        kind: TaskToolKind,
        definitions: &ToolDefinitionCatalog,
        controller: Arc<C>,
        reader: Arc<R>,
        render_accepted: Arc<P>,
    ) -> Result<Self, MissingToolDefinition> {
        Ok(Self {
            kind,
            definition: definitions.describe(kind.name())?.clone(),
            controller,
            reader,
            render_accepted,
            selection: PhantomData,
        })
    }

    pub fn name(&self) -> &'static str {
        self.kind.name()
    }

    pub async fn execute(
        &self,
        invocation: ToolInvocation,
        context: InvocationContext<'_, R::Security, S>,
    ) -> OperationResult<ToolOutput, ToolFailure> {
        match self.kind {
            TaskToolKind::Set => self.set(invocation, context).await,
            TaskToolKind::Get => self
                .get(&invocation.arguments_json)
                .await
                .map_err(OperationError::Failed),
        }
    }

    async fn set(
        &self,
        invocation: ToolInvocation,
        context: InvocationContext<'_, R::Security, S>,
    ) -> OperationResult<ToolOutput, ToolFailure> {
        let identity = TaskCallIdentity {
            assistant_sequence: invocation.assistant_sequence,
            call_id: invocation.call_id.clone(),
        };
        match self
            .controller
            .lookup_accepted(&identity, context.cancellation())
            .await
        {
            Ok(Some(count)) => return self.render_accepted_count(count),
            Ok(None) => {}
            Err(OperationError::Failed(error)) if error.task_caught() => {
                return Ok(ToolOutput::error(error));
            }
            Err(error) => return Err(error),
        }
        let input = match InputObject::parse(&invocation.arguments_json, &["path", "tasks"]) {
            Ok(input) => input,
            Err(error) => return Ok(ToolOutput::error(error)),
        };
        // Source validates presence before converting a path, so null and non-string
        // paths have the same nonblank-path result without invoking the reader.
        let path = input
            .raw("path")
            .and_then(|raw| serde_json::from_str::<String>(raw).ok());
        let tasks = match task_artifacts::parse_task_input(
            input.raw("path").is_some(),
            path.as_deref(),
            input.raw("tasks").is_some(),
            input.raw("tasks").unwrap_or("null"),
            self.reader.as_ref(),
            context.security(),
            context.cancellation(),
        )
        .await
        {
            Ok(tasks) => tasks,
            Err(OperationError::Cancelled) => return Err(OperationError::Cancelled),
            Err(OperationError::Failed(error)) => return task_input_failure(error),
        };
        let update = TaskUpdate {
            tasks,
            selection: context.selection().clone(),
            history_boundary: BeforeToolBatch {
                assistant_sequence: invocation.assistant_sequence,
                call_id: invocation.call_id,
            },
        };
        match self.controller.update(update, context.cancellation()).await {
            Ok(_) => match self
                .controller
                .lookup_accepted(&identity, context.cancellation())
                .await
            {
                Ok(Some(count)) => self.render_accepted_count(count),
                Ok(None) => Err(OperationError::Failed(ToolFailure::new(
                    FailureKind::InvalidOperation,
                    "Accepted task update has no durable receipt.",
                ))),
                Err(OperationError::Failed(error)) if error.task_caught() => {
                    Ok(ToolOutput::error(error))
                }
                Err(error) => Err(error),
            },
            Err(OperationError::Failed(error)) if error.task_caught() => {
                Ok(ToolOutput::error(error))
            }
            Err(error) => Err(error),
        }
    }

    fn render_accepted_count(&self, count: usize) -> OperationResult<ToolOutput, ToolFailure> {
        match (self.render_accepted)(count) {
            Ok(text) => Ok(ToolOutput::text(text)),
            Err(error) if error.task_caught() => Ok(ToolOutput::error(error)),
            Err(error) => Err(OperationError::Failed(error)),
        }
    }

    async fn get(&self, arguments: &str) -> Result<ToolOutput, ToolFailure> {
        let input = match InputObject::parse(arguments, &["name"]) {
            Ok(input) => input,
            Err(error) => return Ok(ToolOutput::error(error)),
        };
        let name = match input.string("name") {
            Ok(name) => name,
            Err(error) => return Ok(ToolOutput::error(error)),
        };
        if name.as_ref().is_some_and(|value| value.trim().is_empty()) {
            return Ok(ToolOutput::error(
                "Tool arguments require a nonblank string 'name'.",
            ));
        }
        let snapshot = self
            .controller
            .snapshot(TaskQuery { name: name.clone() })
            .await?;
        match name {
            None => Ok(ToolOutput::text(write_summaries(&snapshot.tasks))),
            Some(name) => match snapshot.detail {
                Some(detail) => {
                    let mut output = String::new();
                    write_task(
                        &mut output,
                        &detail.task,
                        detail.agent_name.as_deref(),
                        true,
                    );
                    Ok(ToolOutput::text(output))
                }
                None => Ok(ToolOutput::error(format!(
                    "AgentTask '{name}' does not exist."
                ))),
            },
        }
    }
}

impl<C, R, P, S> Tool for TaskTool<C, R, P, S>
where
    C: TaskController<
            Update = TaskUpdate<S>,
            Query = TaskQuery,
            Snapshot = TaskSnapshot,
            Error = ToolFailure,
        >,
    R: AuthorizedReadService,
    R::Security: 'static,
    P: Fn(usize) -> Result<String, ToolFailure> + Send + Sync,
    S: Clone + Send + 'static,
{
    type Definition = ConfiguredToolDefinition;
    type Invocation = ToolInvocation;
    type Selection = S;
    type Security = R::Security;
    type Output = ToolOutput;
    type Error = ToolFailure;

    fn definition(&self) -> &Self::Definition {
        &self.definition
    }
    fn parallel_safety(&self, _: &Self::Invocation) -> ParallelSafety {
        ParallelSafety::Sequential
    }
    fn enabled_after_interruption(&self) -> bool {
        false
    }
    fn invoke<'a>(
        &'a self,
        invocation: Self::Invocation,
        context: InvocationContext<'a, Self::Security, Self::Selection>,
    ) -> BoxFuture<'a, OperationResult<Self::Output, Self::Error>> {
        Box::pin(self.execute(invocation, context))
    }
}

fn task_input_failure(error: TaskInputError) -> OperationResult<ToolOutput, ToolFailure> {
    match error {
        TaskInputError::Arguments(_)
        | TaskInputError::Inline(_)
        | TaskInputError::Artifact(ArtifactReadError::Parse(_)) => Ok(ToolOutput::error(error)),
        TaskInputError::Artifact(ArtifactReadError::Read(error)) => {
            if error.kind == FileReadErrorKind::Denied {
                Ok(ToolOutput::error("access denied"))
            } else if error.is_io()
                || matches!(
                    error.kind,
                    FileReadErrorKind::InvalidPath | FileReadErrorKind::InvalidOperation
                )
            {
                Ok(ToolOutput::error(error))
            } else {
                Err(OperationError::Failed(ToolFailure::new(
                    FailureKind::Other,
                    error.to_string(),
                )))
            }
        }
    }
}

fn quoted(value: &str) -> String {
    serde_json::to_string(value).expect("serializing text cannot fail")
}

fn state(status: ExecutionStatus) -> &'static str {
    match status {
        ExecutionStatus::Pending => "pending",
        ExecutionStatus::Running => "running",
        ExecutionStatus::Succeeded => "succeeded",
        ExecutionStatus::Failed => "failed",
        ExecutionStatus::Canceled => "canceled",
    }
}

pub fn write_summaries(tasks: &[Task]) -> String {
    let entries: Vec<_> = tasks
        .iter()
        .map(|task| {
            format!(
                "{{\"name\":{},\"state\":{},\"description\":{},\"hidden\":{}}}",
                quoted(&task.name),
                quoted(state(task.state)),
                quoted(&task.description),
                task.hidden,
            )
        })
        .collect();
    format!("[{}]", entries.join(","))
}

fn write_task(output: &mut String, task: &Task, agent_name: Option<&str>, with_execution: bool) {
    output.push_str(&format!(
        "{{\"name\":{},\"dependencies\":[",
        quoted(&task.name)
    ));
    for (index, dependency) in task.dependencies.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        output.push_str(&quoted(dependency));
    }
    output.push_str(&format!(
        "],\"description\":{},\"payload\":",
        quoted(&task.description)
    ));
    match &task.payload {
        TaskPayload::Instruction(text) => output.push_str(&quoted(text)),
        TaskPayload::Tasks(children) => {
            output.push('[');
            for (index, child) in children.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_task(output, child, None, false);
            }
            output.push(']');
        }
    }
    output.push_str(&format!(
        ",\"acceptance_criteria\":{}",
        quoted(&task.acceptance_criteria)
    ));
    if let Some(model) = &task.model {
        output.push_str(&format!(",\"model\":{}", quoted(model)));
    }
    output.push_str(&format!(",\"hidden\":{}", task.hidden));
    if let Some(name) = agent_name {
        output.push_str(&format!(",\"agent_name\":{}", quoted(name)));
    }
    if with_execution {
        output.push_str(&format!(",\"state\":{}", quoted(state(task.state))));
        if let Some(result) = &task.result {
            output.push_str(&format!(",\"result\":{}", quoted(result)));
        }
        if let Some(failure) = &task.failure {
            output.push_str(&format!(",\"failure\":{}", quoted(failure)));
        }
    }
    output.push('}');
}
