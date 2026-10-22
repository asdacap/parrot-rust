//! Source-compatible observations of owned work. Observation never settles or cancels it.
use crate::{
    agent_queues::AgentQueues, agent_turn::services::TurnError, children::ChildRegistry,
    task_scheduler::TaskScheduler,
};
use parrot_domain::{
    contracts::{
        agent_tasks::TaskArtifactParser,
        async_ops::BoxFuture,
        security::RestrictableAccessSnapshot,
        status::{ActiveWorkObservation, ActiveWorkState, ProcessStatusSource},
    },
    values::agent_tasks::{ExecutionStatus, Task},
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_storage::events::NeverCancelled;
use std::sync::Arc;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockerResult {
    pub work_section: Option<String>,
    pub reminder: Option<String>,
}

pub trait ActiveWorkBlocker: Send + Sync {
    fn observe(&self) -> BoxFuture<'_, Result<Option<BlockerResult>, TurnError>>;
}

pub fn format_section(heading: &str, observations: &[ActiveWorkObservation]) -> String {
    if observations.is_empty() {
        return String::new();
    }
    let mut ordered = observations.iter().collect::<Vec<_>>();
    ordered.sort_by(|a, b| super::status::ordinal(&a.id, &b.id));
    let mut section = format!("\n{heading}:");
    for item in ordered {
        section.push_str(&format!("\n- {} (name: {})", item.id, item.name));
    }
    section
}

pub struct ActiveWorkReminder {
    pub blockers: Vec<Arc<dyn ActiveWorkBlocker>>,
    pub templates: Arc<PromptTemplateCatalog>,
}
impl ActiveWorkReminder {
    pub async fn build(&self) -> Result<Option<String>, TurnError> {
        let mut sections = Vec::new();
        let mut reminders = Vec::new();
        let mut blocking = false;
        for blocker in &self.blockers {
            if let Some(result) = blocker.observe().await? {
                blocking = true;
                if let Some(section) = result.work_section {
                    sections.push(section);
                }
                if let Some(reminder) = result.reminder {
                    reminders.push(reminder);
                }
            }
        }
        if !blocking {
            return Ok(None);
        }
        if !sections.is_empty() {
            reminders.insert(
                0,
                self.templates
                    .render(
                        "agent-session.active-work-reminder",
                        &[("active_work".into(), sections.concat().into())],
                        &NeverCancelled,
                    )
                    .map_err(|error| TurnError::Service(error.to_string()))?,
            );
        }
        Ok(Some(reminders.join("\n")))
    }

    pub async fn work_sections(&self) -> Result<String, TurnError> {
        let mut sections = String::new();
        for blocker in &self.blockers {
            if let Some(result) = blocker.observe().await? {
                if let Some(section) = result.work_section {
                    sections.push_str(&section);
                }
            }
        }
        Ok(sections)
    }
}

pub struct ChildBlocker<S: RestrictableAccessSnapshot + 'static> {
    pub children: Arc<ChildRegistry<S>>,
    pub owner_session_id: String,
}
impl<S: RestrictableAccessSnapshot + 'static> ActiveWorkBlocker for ChildBlocker<S> {
    fn observe(&self) -> BoxFuture<'_, Result<Option<BlockerResult>, TurnError>> {
        Box::pin(async move {
            let children = self
                .children
                .snapshot_descendants()
                .into_iter()
                .filter(|child| {
                    child.is_active()
                        && child.identity().session.parent_session_id == self.owner_session_id
                })
                .map(|child| ActiveWorkObservation {
                    id: child.identity().session.session_id.clone(),
                    name: child.identity().session.name.clone(),
                    state: ActiveWorkState::Running,
                })
                .collect::<Vec<_>>();
            Ok((!children.is_empty()).then(|| BlockerResult {
                work_section: Some(format_section("Running direct subagents", &children)),
                reminder: None,
            }))
        })
    }
}

pub struct ProcessBlocker {
    pub processes: Arc<dyn ProcessStatusSource>,
}
impl ActiveWorkBlocker for ProcessBlocker {
    fn observe(&self) -> BoxFuture<'_, Result<Option<BlockerResult>, TurnError>> {
        Box::pin(async move {
            let active = self.processes.active();
            Ok((!active.is_empty()).then(|| BlockerResult {
                work_section: Some(format_section("Running processes", &active)),
                reminder: None,
            }))
        })
    }
}

pub trait TaskSnapshotSource: Send + Sync {
    fn snapshot(&self) -> BoxFuture<'_, Vec<Task>>;
}
impl<S: RestrictableAccessSnapshot + 'static, P: TaskArtifactParser + 'static> TaskSnapshotSource
    for TaskScheduler<S, P>
{
    fn snapshot(&self) -> BoxFuture<'_, Vec<Task>> {
        Box::pin(TaskScheduler::snapshot(self))
    }
}
pub struct TaskBlocker {
    pub tasks: Arc<dyn TaskSnapshotSource>,
    pub templates: Arc<PromptTemplateCatalog>,
}
impl ActiveWorkBlocker for TaskBlocker {
    fn observe(&self) -> BoxFuture<'_, Result<Option<BlockerResult>, TurnError>> {
        Box::pin(async move {
            let tasks = self.tasks.snapshot().await;
            let unresolved = tasks
                .into_iter()
                .filter_map(|task| {
                    let state = match task.state {
                        ExecutionStatus::Pending => "pending",
                        ExecutionStatus::Running => "running",
                        ExecutionStatus::Failed => "failed",
                        ExecutionStatus::Succeeded | ExecutionStatus::Canceled => return None,
                    };
                    Some(ActiveWorkObservation {
                        id: format!("{} [{state}]", task.name),
                        name: task.description,
                        state: ActiveWorkState::Running,
                    })
                })
                .collect::<Vec<_>>();
            if unresolved.is_empty() {
                return Ok(None);
            }
            let heading = self
                .templates
                .render(
                    "agent-session.active-work-unresolved-agent-tasks-heading",
                    &[],
                    &NeverCancelled,
                )
                .map_err(|error| TurnError::Service(error.to_string()))?;
            Ok(Some(BlockerResult {
                work_section: Some(format_section(&heading, &unresolved)),
                reminder: None,
            }))
        })
    }
}

pub struct QueueBlocker<S: RestrictableAccessSnapshot + 'static> {
    pub queues: Arc<AgentQueues<S>>,
    pub templates: Arc<PromptTemplateCatalog>,
}
impl<S: RestrictableAccessSnapshot + 'static> ActiveWorkBlocker for QueueBlocker<S> {
    fn observe(&self) -> BoxFuture<'_, Result<Option<BlockerResult>, TurnError>> {
        Box::pin(async move {
            let mut queues = self
                .queues
                .snapshot()
                .map_err(|error| TurnError::Service(error.to_string()))?
                .queues;
            queues.retain(|queue| queue.size > 0);
            queues.sort_by(|a, b| a.name.cmp(&b.name));
            if queues.is_empty() {
                return Ok(None);
            }
            let queues = queues
                .into_iter()
                .map(|queue| serde_json::json!({ "name": queue.name, "size": queue.size }))
                .collect::<Vec<_>>();
            let reminder = self
                .templates
                .render(
                    "agent-session.nonempty-queues-reminder",
                    &[("queues".into(), queues.into())],
                    &NeverCancelled,
                )
                .map_err(|error| TurnError::Service(error.to_string()))?;
            Ok(Some(BlockerResult {
                work_section: None,
                reminder: Some(reminder),
            }))
        })
    }
}
