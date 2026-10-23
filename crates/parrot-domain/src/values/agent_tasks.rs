//! Pure values shared by agent-task parsing and runtime scheduling.

use serde::{Deserialize, Serialize};

pub const ARTIFACT_VERSION: u64 = 1;
pub const MAX_DESCRIPTION_LENGTH: usize = 300;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskArtifact {
    pub schema_version: u64,
    pub tasks: Vec<Task>,
}
impl TaskArtifact {
    pub fn display_name(&self) -> String {
        if self.tasks.len() == 1 {
            self.tasks[0].name.clone()
        } else {
            format!("{} top-level tasks", self.tasks.len())
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub name: String,
    pub dependencies: Vec<String>,
    pub description: String,
    pub payload: TaskPayload,
    pub acceptance_criteria: String,
    pub model: Option<String>,
    pub hidden: bool,
    pub state: ExecutionStatus,
    pub result: Option<String>,
    pub failure: Option<String>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum TaskPayload {
    Instruction(String),
    Tasks(Vec<Task>),
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    #[default]
    Pending,
    Running,
    Succeeded,
    Failed,
    Canceled,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LeafResponse {
    pub result: String,
    pub verdict: AcceptanceVerdict,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AcceptanceVerdict {
    Accept,
    RejectAndHalt {
        feedback: String,
    },
    RejectAndRetry {
        feedback: String,
        payload: Option<String>,
        replacement_result: Option<String>,
    },
}
