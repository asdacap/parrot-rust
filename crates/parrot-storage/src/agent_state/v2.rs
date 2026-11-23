use parrot_domain::values::{
    agent_state::{
        AGENT_STATE_VERSION, AcceptedTaskCall, AgentDescriptor, AgentStateSnapshot,
        DedicatedTaskExecutionSnapshot, ExecutionCheckpoint, ForkBoundarySnapshot,
        NotificationCheckpoint, RetainedChild, SelectionSnapshot, TaskEntrySnapshot,
        TaskExecutionSnapshot, TaskForkModeSnapshot, TaskGraphMigrationSnapshot, TaskGraphSnapshot,
        TaskNotificationSnapshot, TaskResponseReference, TaskRunnerCheckpoint, TaskRunnerPhase,
        TaskStopCheckpoint,
    },
    agent_tasks::Task,
};
use serde::Deserialize;

/// The `schema_version` 2 snapshot shape. Released builds wrote it while runner
/// checkpoints carried response repair state, so `normalize` maps it forward
/// while every write uses the current shape.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct V2AgentStateSnapshot {
    #[serde(rename = "schema_version")]
    _schema_version: u64,
    owner_session_id: String,
    revision: u64,
    descriptor: AgentDescriptor,
    children: Vec<RetainedChild>,
    tasks: V2TaskGraphSnapshot,
    execution: Option<ExecutionCheckpoint>,
    #[serde(default)]
    core: Option<parrot_core::AgentState>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct V2TaskGraphSnapshot {
    #[serde(default)]
    migration: TaskGraphMigrationSnapshot,
    entries: Vec<V2TaskEntrySnapshot>,
    revision: u64,
    complete: bool,
    all_resolved: bool,
    completion_notification: Option<NotificationCheckpoint>,
    notifications: Vec<TaskNotificationSnapshot>,
    accepted_calls: Vec<AcceptedTaskCall>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct V2TaskEntrySnapshot {
    task: Task,
    diagnostic_id: String,
    selection: SelectionSnapshot,
    fork_boundary: ForkBoundarySnapshot,
    fork_mode: TaskForkModeSnapshot,
    execution: V2TaskExecutionSnapshot,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum V2TaskExecutionSnapshot {
    Unassigned,
    Owner {
        generation_id: String,
    },
    Dedicated {
        worker: Box<V2DedicatedTaskExecutionSnapshot>,
    },
    RetiringLegacy {
        worker: Box<V2DedicatedTaskExecutionSnapshot>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct V2DedicatedTaskExecutionSnapshot {
    worker_session_id: Option<String>,
    runner: Option<V2TaskRunnerCheckpoint>,
    terminal_notification: Option<NotificationCheckpoint>,
    pending_stop: Option<TaskStopCheckpoint>,
}

// Stored version 2 checkpoints carry the retired repair fields; serde drops
// them instead of binding them into the current shape.
#[derive(Deserialize)]
struct V2TaskRunnerCheckpoint {
    original_task: Task,
    current_task: Task,
    feedback: Vec<String>,
    carried_result: Option<String>,
    attempt: u64,
    phase: V2TaskRunnerPhase,
    generation_id: String,
    prompt_message_id: String,
    prompt: String,
    maximum_attempts: u64,
    assignment_header: String,
    outcome: Option<Task>,
    response_reference: Option<TaskResponseReference>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum V2TaskRunnerPhase {
    PromptIntent,
    AwaitingResponse,
    RepairingResponse,
    AssessingResponse,
    RetryIntent,
    Completed,
}

impl V2AgentStateSnapshot {
    pub(super) fn normalize(self) -> AgentStateSnapshot {
        AgentStateSnapshot {
            schema_version: AGENT_STATE_VERSION,
            owner_session_id: self.owner_session_id,
            revision: self.revision,
            descriptor: self.descriptor,
            children: self.children,
            tasks: self.tasks.normalize(),
            execution: self.execution,
            core: self.core,
        }
    }
}

impl V2TaskGraphSnapshot {
    fn normalize(self) -> TaskGraphSnapshot {
        TaskGraphSnapshot {
            migration: self.migration,
            entries: self
                .entries
                .into_iter()
                .map(V2TaskEntrySnapshot::normalize)
                .collect(),
            revision: self.revision,
            complete: self.complete,
            all_resolved: self.all_resolved,
            completion_notification: self.completion_notification,
            notifications: self.notifications,
            accepted_calls: self.accepted_calls,
        }
    }
}

impl V2TaskEntrySnapshot {
    fn normalize(self) -> TaskEntrySnapshot {
        TaskEntrySnapshot {
            task: self.task,
            diagnostic_id: self.diagnostic_id,
            selection: self.selection,
            fork_boundary: self.fork_boundary,
            fork_mode: self.fork_mode,
            execution: match self.execution {
                V2TaskExecutionSnapshot::Unassigned => TaskExecutionSnapshot::Unassigned,
                V2TaskExecutionSnapshot::Owner { generation_id } => {
                    TaskExecutionSnapshot::Owner { generation_id }
                }
                V2TaskExecutionSnapshot::Dedicated { worker } => TaskExecutionSnapshot::Dedicated {
                    worker: Box::new(worker.normalize()),
                },
                V2TaskExecutionSnapshot::RetiringLegacy { worker } => {
                    TaskExecutionSnapshot::RetiringLegacy {
                        worker: Box::new(worker.normalize()),
                    }
                }
            },
        }
    }
}

impl V2DedicatedTaskExecutionSnapshot {
    fn normalize(self) -> DedicatedTaskExecutionSnapshot {
        DedicatedTaskExecutionSnapshot {
            worker_session_id: self.worker_session_id,
            runner: self.runner.map(V2TaskRunnerCheckpoint::normalize),
            terminal_notification: self.terminal_notification,
            pending_stop: self.pending_stop,
        }
    }
}

impl V2TaskRunnerCheckpoint {
    fn normalize(self) -> TaskRunnerCheckpoint {
        TaskRunnerCheckpoint {
            original_task: self.original_task,
            current_task: self.current_task,
            feedback: self.feedback,
            carried_result: self.carried_result,
            attempt: self.attempt,
            phase: match self.phase {
                // A persisted repair prompt re-admits as a fresh prompt; the
                // turn's completion blocker owns response validation now.
                V2TaskRunnerPhase::RepairingResponse => TaskRunnerPhase::PromptIntent,
                V2TaskRunnerPhase::PromptIntent => TaskRunnerPhase::PromptIntent,
                V2TaskRunnerPhase::AwaitingResponse => TaskRunnerPhase::AwaitingResponse,
                V2TaskRunnerPhase::AssessingResponse => TaskRunnerPhase::AssessingResponse,
                V2TaskRunnerPhase::RetryIntent => TaskRunnerPhase::RetryIntent,
                V2TaskRunnerPhase::Completed => TaskRunnerPhase::Completed,
            },
            generation_id: self.generation_id,
            prompt_message_id: self.prompt_message_id,
            prompt: self.prompt,
            maximum_attempts: self.maximum_attempts,
            assignment_header: self.assignment_header,
            outcome: self.outcome,
            response_reference: self.response_reference,
        }
    }
}
