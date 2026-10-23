//! Agent-owned recovery state, separate from conversation and event history.

use super::agent_tasks::Task;
use serde::{Deserialize, Serialize};

pub const AGENT_STATE_VERSION: u64 = 1;
pub const AGENT_STATE_FILE_NAME: &str = "agent-state.json";
pub const AGENT_STATE_STAGING_FILE_NAME: &str = "agent-state.json.tmp";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentStateSnapshot {
    pub schema_version: u64,
    pub owner_session_id: String,
    pub revision: u64,
    pub descriptor: AgentDescriptor,
    pub children: Vec<RetainedChild>,
    pub tasks: TaskGraphSnapshot,
    pub execution: Option<ExecutionCheckpoint>,
}
impl AgentStateSnapshot {
    pub fn new(owner_session_id: String, descriptor: AgentDescriptor) -> Self {
        Self {
            schema_version: AGENT_STATE_VERSION,
            owner_session_id,
            revision: 0,
            descriptor,
            children: Vec::new(),
            tasks: TaskGraphSnapshot::default(),
            execution: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDescriptor {
    pub parent_session_id: Option<String>,
    pub name: String,
    pub selection: SelectionSnapshot,
    pub local_scope: String,
    pub completion_delivery: CompletionDeliverySnapshot,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionSnapshot {
    pub profile_id: String,
    pub model_selector: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionDeliverySnapshot {
    Automatic,
    RetainedOnly,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedChild {
    pub name: String,
    pub session_id: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskGraphSnapshot {
    pub entries: Vec<TaskEntrySnapshot>,
    pub revision: u64,
    pub complete: bool,
    pub all_resolved: bool,
    pub completion_notification: Option<NotificationCheckpoint>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskEntrySnapshot {
    pub task: Task,
    pub diagnostic_id: String,
    pub selection: SelectionSnapshot,
    pub fork_boundary: ForkBoundarySnapshot,
    pub worker_session_id: Option<String>,
    pub runner: Option<TaskRunnerCheckpoint>,
    pub terminal_notification: Option<NotificationCheckpoint>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ForkBoundarySnapshot {
    BeforeToolBatch {
        assistant_sequence: i64,
        tool_call_id: String,
    },
    AfterCompletedHistory,
    AfterSafeHistoryPrefix,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRunnerCheckpoint {
    pub original_task: Task,
    pub current_task: Task,
    pub feedback: Vec<String>,
    pub carried_result: Option<String>,
    pub attempt: u64,
    pub repair_attempt: u64,
    pub phase: TaskRunnerPhase,
    pub execution_id: String,
    pub prompt_message_id: String,
    pub response_message_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskRunnerPhase {
    PromptIntent,
    AwaitingResponse,
    RepairingResponse,
    AssessingResponse,
    RetryIntent,
    Completed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionCheckpoint {
    pub execution_id: String,
    pub initial_message_id: String,
    pub initial_parts: Vec<ExecutionInputPart>,
    pub history_watermark: i64,
    pub phase: ExecutionPhase,
    pub terminal: Option<ExecutionTerminal>,
    pub parent_notification: Option<NotificationCheckpoint>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionInputPart {
    Text {
        text: String,
    },
    ImageArtifact {
        artifact_id: String,
        media_type: String,
        display_name: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionPhase {
    AdmissionIntent,
    Active,
    Suspended,
    Completed,
    Canceled,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionTerminal {
    pub event_id: String,
    /// The authoritative conversation result reference, not the lifecycle event.
    pub history_sequence: i64,
    pub outcome: ExecutionOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "failure", rename_all = "snake_case")]
pub enum ExecutionOutcome {
    Succeeded,
    Failed(String),
    Canceled,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationCheckpoint {
    pub message_id: String,
    pub admitted: bool,
    pub delivery_error: Option<String>,
}

/// Replaces one owned section without overwriting concurrently saved sections.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentStateUpdate {
    Descriptor(AgentDescriptor),
    Children(Vec<RetainedChild>),
    Tasks(TaskGraphSnapshot),
    Execution(Option<ExecutionCheckpoint>),
}
