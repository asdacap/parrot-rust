use parrot_domain::values::{
    agent_state::{
        AGENT_STATE_VERSION, AcceptedTaskCall, AgentDescriptor, AgentStateSnapshot,
        DedicatedTaskExecutionSnapshot, ExecutionCheckpoint, ForkBoundarySnapshot,
        NotificationCheckpoint, RetainedChild, SelectionSnapshot, TaskCallReceipt,
        TaskEntrySnapshot, TaskExecutionSnapshot, TaskForkModeSnapshot, TaskGraphMigrationSnapshot,
        TaskGraphSnapshot, TaskNotificationSnapshot, TaskRunnerCheckpoint, TaskStopCheckpoint,
    },
    agent_tasks::{ExecutionStatus, Task, TaskCallIdentity},
};
use serde::Deserialize;

/// The `schema_version` 1 snapshot shape. Released builds wrote it before entries
/// carried execution ownership, graphs carried migration state, calls carried
/// typed receipts, and the snapshot carried core state, so `normalize` maps it
/// forward while every write uses the current shape.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct V1AgentStateSnapshot {
    #[serde(rename = "schema_version")]
    _schema_version: u64,
    owner_session_id: String,
    revision: u64,
    descriptor: AgentDescriptor,
    children: Vec<RetainedChild>,
    tasks: V1TaskGraphSnapshot,
    execution: Option<ExecutionCheckpoint>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct V1TaskGraphSnapshot {
    entries: Vec<V1TaskEntrySnapshot>,
    revision: u64,
    complete: bool,
    all_resolved: bool,
    completion_notification: Option<NotificationCheckpoint>,
    notifications: Vec<V1TaskNotificationSnapshot>,
    accepted_calls: Vec<V1AcceptedTaskCall>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct V1AcceptedTaskCall {
    identity: TaskCallIdentity,
    accepted_count: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct V1TaskEntrySnapshot {
    task: Task,
    diagnostic_id: String,
    selection: SelectionSnapshot,
    fork_boundary: ForkBoundarySnapshot,
    worker_session_id: Option<String>,
    runner: Option<TaskRunnerCheckpoint>,
    terminal_notification: Option<NotificationCheckpoint>,
    fork_mode: TaskForkModeSnapshot,
    pending_stop: Option<TaskStopCheckpoint>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct V1TaskNotificationSnapshot {
    message_id: String,
    target_session_id: String,
    text: String,
    wake: bool,
    ready: bool,
}

impl V1AgentStateSnapshot {
    pub(super) fn normalize(self) -> AgentStateSnapshot {
        AgentStateSnapshot {
            schema_version: AGENT_STATE_VERSION,
            owner_session_id: self.owner_session_id,
            revision: self.revision,
            descriptor: self.descriptor,
            children: self.children,
            tasks: self.tasks.normalize(),
            execution: self.execution,
            core: None,
        }
    }
}

impl V1TaskGraphSnapshot {
    fn normalize(self) -> TaskGraphSnapshot {
        TaskGraphSnapshot {
            migration: TaskGraphMigrationSnapshot::Pending,
            entries: self
                .entries
                .into_iter()
                .map(V1TaskEntrySnapshot::normalize)
                .collect(),
            revision: self.revision,
            complete: self.complete,
            all_resolved: self.all_resolved,
            completion_notification: self.completion_notification,
            notifications: self
                .notifications
                .into_iter()
                .map(V1TaskNotificationSnapshot::normalize)
                .collect(),
            accepted_calls: self
                .accepted_calls
                .into_iter()
                .map(|call| AcceptedTaskCall {
                    identity: call.identity,
                    receipt: TaskCallReceipt::LegacyCount {
                        accepted_count: call.accepted_count,
                    },
                })
                .collect(),
        }
    }
}

impl V1TaskEntrySnapshot {
    fn normalize(self) -> TaskEntrySnapshot {
        let must_retire = self.task.state == ExecutionStatus::Running
            || self.pending_stop.is_some()
            || (self.task.state == ExecutionStatus::Pending && self.runner.is_some());
        let has_provenance = self.worker_session_id.is_some()
            || self.runner.is_some()
            || self.terminal_notification.is_some();
        let worker = DedicatedTaskExecutionSnapshot {
            worker_session_id: self.worker_session_id,
            runner: self.runner,
            terminal_notification: self.terminal_notification,
            pending_stop: self.pending_stop,
        };
        let execution = if must_retire {
            TaskExecutionSnapshot::RetiringLegacy {
                worker: Box::new(worker),
            }
        } else if self.task.state != ExecutionStatus::Pending && has_provenance {
            TaskExecutionSnapshot::Dedicated {
                worker: Box::new(worker),
            }
        } else {
            TaskExecutionSnapshot::Unassigned
        };
        TaskEntrySnapshot {
            task: self.task,
            diagnostic_id: self.diagnostic_id,
            selection: self.selection,
            fork_boundary: self.fork_boundary,
            fork_mode: self.fork_mode,
            execution,
        }
    }
}

impl V1TaskNotificationSnapshot {
    fn normalize(self) -> TaskNotificationSnapshot {
        TaskNotificationSnapshot {
            message_id: self.message_id,
            target_session_id: self.target_session_id,
            text: self.text,
            wake: self.wake,
            ready: self.ready,
            owner_assignment: None,
        }
    }
}
