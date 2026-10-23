//! Retained task tools over the owning scope's actual scheduler and parser.

use std::sync::Arc;

use parrot_domain::contracts::{
    async_ops::{BoxFuture, Cancellation, OperationError},
    runtime::TaskController,
    security::RestrictableAccessSnapshot,
};
use parrot_runtime::{
    agent_turn::services::TurnSelection,
    children::{ChildError, ChildResult},
    task_scheduler::{SchedulerConfig, TaskScheduler},
};
use parrot_storage::conversation::HistoryForkBoundary;
use parrot_tools::{
    task_artifacts::TaskArtifactParserAdapter,
    tasks_queues::{
        FailureKind, ToolFailure,
        tasks::{TaskDetail, TaskQuery, TaskSnapshot, TaskUpdate},
    },
};

/// The scheduler retains its runner, real parser, output-blob store, owned work
/// and scoped notifier. This adapter only converts the tool's settled values.
/// `capture` forwards the invocation's captured selection; it does not resolve
/// a replacement model or read the scope's mutable selection later.
pub struct ScopedTaskController<S: RestrictableAccessSnapshot + 'static, T> {
    scheduler: Arc<TaskScheduler<S, TaskArtifactParserAdapter>>,
    capture: fn(&T) -> TurnSelection<S>,
}

impl<S, T> ScopedTaskController<S, T>
where
    S: RestrictableAccessSnapshot + 'static,
    T: Send + 'static,
{
    pub async fn new(
        config: SchedulerConfig<S, TaskArtifactParserAdapter>,
        capture: fn(&T) -> TurnSelection<S>,
    ) -> ChildResult<Arc<Self>> {
        let scheduler = TaskScheduler::new(config).await?;
        Ok(Arc::new(Self { scheduler, capture }))
    }

    pub fn scheduler(&self) -> &Arc<TaskScheduler<S, TaskArtifactParserAdapter>> {
        &self.scheduler
    }

    async fn capture_snapshot(&self, query: TaskQuery) -> TaskSnapshot {
        match query.name {
            Some(name) => TaskSnapshot {
                tasks: Vec::new(),
                detail: self
                    .scheduler
                    .capture_detail(&name)
                    .await
                    .map(|detail| TaskDetail {
                        task: detail.task,
                        agent_name: detail.agent_name,
                    }),
            },
            None => TaskSnapshot {
                tasks: self.scheduler.snapshot().await,
                detail: None,
            },
        }
    }
}

impl<S, T> TaskController for ScopedTaskController<S, T>
where
    S: RestrictableAccessSnapshot + 'static,
    T: Send + 'static,
{
    type Update = TaskUpdate<T>;
    type Query = TaskQuery;
    type Snapshot = TaskSnapshot;
    type Error = ToolFailure;

    fn lookup_accepted<'a>(
        &'a self,
        identity: &'a parrot_domain::values::agent_tasks::TaskCallIdentity,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<usize>, OperationError<ToolFailure>>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            tokio::select! { biased; _ = cancellation.cancelled() => Err(OperationError::Cancelled), receipt = self.scheduler.lookup_accepted(identity) => receipt.map_err(operation_error) }
        })
    }
    fn update<'a>(
        &'a self,
        update: Self::Update,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Self::Snapshot, OperationError<Self::Error>>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            let selection = (self.capture)(&update.selection);
            let operation = self
                .scheduler
                .set_tool_tasks(
                    update.tasks,
                    selection,
                    HistoryForkBoundary::BeforeToolBatch {
                        assistant_sequence: update.history_boundary.assistant_sequence,
                        tool_call_id: update.history_boundary.call_id.clone(),
                    },
                    parrot_domain::values::agent_tasks::TaskCallIdentity {
                        assistant_sequence: update.history_boundary.assistant_sequence,
                        call_id: update.history_boundary.call_id,
                    },
                )
                .map_err(operation_error)?;
            // Cancellation stops only this observer. The actual scheduler owns
            // admission, notification and execution until settlement joins them.
            operation
                .wait(cancellation)
                .await
                .map_err(operation_error)?;
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(OperationError::Cancelled),
                snapshot = self.capture_snapshot(TaskQuery { name: None }) => Ok(snapshot),
            }
        })
    }

    fn snapshot(&self, query: Self::Query) -> BoxFuture<'_, Result<Self::Snapshot, Self::Error>> {
        Box::pin(async move { Ok(self.capture_snapshot(query).await) })
    }
}

fn operation_error(error: ChildError) -> OperationError<ToolFailure> {
    if matches!(error, ChildError::Cancelled) || error.code() == "cancelled" {
        return OperationError::Cancelled;
    }
    let kind = match &error {
        ChildError::Registry(_) | ChildError::NameConflict(_) => FailureKind::Registry,
        ChildError::Failure {
            code: "argument", ..
        } => FailureKind::Argument,
        ChildError::Failure {
            code: "invalid_operation",
            ..
        } => FailureKind::InvalidOperation,
        ChildError::Failure {
            code: "access_denied",
            ..
        } => FailureKind::Denied,
        ChildError::Failure { code: "io", .. } => FailureKind::Io,
        _ => FailureKind::Other,
    };
    OperationError::Failed(ToolFailure::new(kind, error.to_string()))
}
