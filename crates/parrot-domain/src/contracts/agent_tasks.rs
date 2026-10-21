//! Domain boundary for synchronous, in-memory agent-task parsing and validation.
//! Parser behavior and implementation remain in tools; runtime depends on this port.

use crate::values::agent_tasks::{LeafResponse, Task, TaskArtifact};

pub trait TaskArtifactParser: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;

    fn parse_artifact(&self, input: &str) -> Result<TaskArtifact, Self::Error>;
    fn parse_task_set(&self, input: &str) -> Result<Vec<Task>, Self::Error>;
    fn validate_merged_graph(&self, tasks: &[Task], path: &str) -> Result<(), Self::Error>;
    fn parse_leaf_response(&self, input: &str) -> Result<LeafResponse, Self::Error>;
}
