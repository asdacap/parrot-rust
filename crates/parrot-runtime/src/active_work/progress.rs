use super::blockers::{ActiveWorkBlocker, BlockerResult};
use crate::{
    agent_turn::services::TurnError,
    children::{ChildError, ChildResult, Operation, SnapshotPublisher},
    event_broker::EventBroker,
};
use parrot_domain::{
    contracts::{
        async_ops::BoxFuture,
        progress::{ProgressStore, ProgressStoreError, ProgressStoreErrorKind},
    },
    protocol::messages::parrot::{Event, ToolSnapshot},
    values::{
        identifiers::Identifier,
        progress::{
            PROGRESS_INVENTORY_KIND, ProgressSnapshot, ProgressUpdate, ProgressValidationError,
        },
    },
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_storage::events::NeverCancelled;
use std::{fmt, sync::Arc};
use tokio::sync::{Mutex, watch};

#[derive(Clone, Debug)]
pub enum ProgressError {
    Storage(ProgressStoreError),
    Validation(ProgressValidationError),
    OwnerMismatch,
    Identifier(String),
    Stopped,
}
impl fmt::Display for ProgressError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => error.fmt(formatter),
            Self::Validation(error) => error.fmt(formatter),
            Self::OwnerMismatch => {
                formatter.write_str("progress snapshot belongs to another agent")
            }
            Self::Identifier(error) => formatter.write_str(error),
            Self::Stopped => formatter.write_str("agent progress publication has stopped"),
        }
    }
}
impl std::error::Error for ProgressError {}

#[derive(Clone)]
struct ProgressInventory {
    snapshot: ProgressSnapshot,
    instance: String,
    removed: bool,
}
impl ProgressInventory {
    fn render_events(&self) -> ChildResult<Vec<Event>> {
        let mut event = Event {
            agent_session_id: self.snapshot.owner_session_id.clone(),
            ..Event::new()
        };
        event.set_tool_snapshot(ToolSnapshot {
            kind: PROGRESS_INVENTORY_KIND.into(),
            owner_agent_session_id: self.snapshot.owner_session_id.clone(),
            inventory_instance_id: self.instance.clone(),
            revision: self.snapshot.revision,
            final_chunk: true,
            removed: self.removed,
            data: serde_json::to_vec(&self.snapshot)
                .map_err(|error| ChildError::failure("unexpected", error.to_string()))?,
            ..ToolSnapshot::new()
        });
        Ok(vec![event])
    }
}

pub struct AgentProgress {
    store: Arc<dyn ProgressStore>,
    mutation: Mutex<()>,
    current: watch::Sender<Result<ProgressInventory, ProgressError>>,
}
impl AgentProgress {
    pub async fn new(
        store: Arc<dyn ProgressStore>,
        owner_session_id: String,
    ) -> Result<Self, ProgressError> {
        let snapshot = store
            .load()
            .await
            .map_err(ProgressError::Storage)?
            .unwrap_or_else(|| ProgressSnapshot::new(owner_session_id.clone()));
        snapshot.validate().map_err(ProgressError::Validation)?;
        if snapshot.owner_session_id != owner_session_id {
            return Err(ProgressError::OwnerMismatch);
        }
        let instance =
            Identifier::event_id().map_err(|error| ProgressError::Identifier(error.to_string()))?;
        let (current, _) = watch::channel(Ok(ProgressInventory {
            snapshot,
            instance,
            removed: false,
        }));
        Ok(Self {
            store,
            mutation: Mutex::new(()),
            current,
        })
    }

    pub fn capture_snapshot(&self) -> Result<ProgressSnapshot, ProgressError> {
        Ok(self.current.borrow().clone()?.snapshot)
    }

    /// The retained tool drain must join an admitted update before observing
    /// completion or disposing this owner. Persistence precedes observation.
    pub async fn update(&self, update: ProgressUpdate) -> Result<bool, ProgressError> {
        let _guard = self.mutation.lock().await;
        let mut inventory = self.current.borrow().clone()?;
        if inventory.removed {
            return Err(ProgressError::Stopped);
        }
        if !inventory
            .snapshot
            .apply(update)
            .map_err(ProgressError::Validation)?
        {
            return Ok(false);
        }
        if let Err(error) = self.store.replace(inventory.snapshot.clone()).await {
            if error.kind == ProgressStoreErrorKind::UncertainCommit {
                let reconciled = self.reconcile(&inventory.snapshot.owner_session_id).await;
                let _ = self
                    .current
                    .send_replace(reconciled.map(|snapshot| ProgressInventory {
                        snapshot,
                        ..inventory
                    }));
            }
            return Err(ProgressError::Storage(error));
        }
        let _ = self.current.send_replace(Ok(inventory));
        Ok(true)
    }

    async fn reconcile(&self, owner_session_id: &str) -> Result<ProgressSnapshot, ProgressError> {
        let snapshot = self
            .store
            .load()
            .await
            .map_err(ProgressError::Storage)?
            .ok_or_else(|| {
                ProgressError::Storage(ProgressStoreError::new(
                    ProgressStoreErrorKind::Corrupt,
                    "progress snapshot is missing after an uncertain commit",
                ))
            })?;
        snapshot.validate().map_err(ProgressError::Validation)?;
        if snapshot.owner_session_id != owner_session_id {
            return Err(ProgressError::OwnerMismatch);
        }
        Ok(snapshot)
    }

    pub async fn stop_publication(&self) -> Result<(), ProgressError> {
        let _guard = self.mutation.lock().await;
        let mut inventory = self.current.borrow().clone()?;
        if !inventory.removed {
            inventory.removed = true;
            let _ = self.current.send_replace(Ok(inventory));
        }
        Ok(())
    }
}

pub struct ProgressBlocker {
    pub progress: Arc<AgentProgress>,
    pub templates: Arc<PromptTemplateCatalog>,
}
impl ActiveWorkBlocker for ProgressBlocker {
    fn observe(&self) -> BoxFuture<'_, Result<Option<BlockerResult>, TurnError>> {
        Box::pin(async move {
            let snapshot = self
                .progress
                .capture_snapshot()
                .map_err(|error| TurnError::Service(error.to_string()))?;
            let Some(progress) = snapshot.progress else {
                return Ok(None);
            };
            let reminder = self
                .templates
                .render(
                    "agent-session.progress-reminder",
                    &[
                        ("steps".into(), progress.steps.to_string().into()),
                        (
                            "total_steps".into(),
                            progress.total_steps.to_string().into(),
                        ),
                        (
                            "current_step".into(),
                            progress.current_step.unwrap_or_default().into(),
                        ),
                    ],
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

pub struct ProgressSnapshotPublisher {
    progress: Arc<AgentProgress>,
    broker: Arc<EventBroker>,
}
impl ProgressSnapshotPublisher {
    pub fn new(progress: Arc<AgentProgress>, broker: Arc<EventBroker>) -> Self {
        Self { progress, broker }
    }
}
impl SnapshotPublisher for ProgressSnapshotPublisher {
    fn capture(&self) -> ChildResult<Vec<Event>> {
        self.progress
            .current
            .borrow()
            .clone()
            .map_err(|error| ChildError::failure("invalid_operation", error.to_string()))?
            .render_events()
    }
    fn start(&self) -> ChildResult<Operation<()>> {
        let mut changes = self.progress.current.subscribe();
        let broker = self.broker.clone();
        Ok(Operation::start(async move {
            loop {
                let inventory = changes
                    .borrow_and_update()
                    .clone()
                    .map_err(|error| ChildError::failure("invalid_operation", error.to_string()))?;
                broker
                    .publish_inventory(&inventory.render_events()?)
                    .map_err(|error| ChildError::failure("invalid_operation", error.to_string()))?;
                if inventory.removed || changes.changed().await.is_err() {
                    return Ok(());
                }
            }
        }))
    }
}

#[cfg(test)]
mod tests;
