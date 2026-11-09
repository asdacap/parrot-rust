//! Structured authoritative SQLite history, with optional derived-file refresh.
mod agent;
pub mod compaction;
pub mod forks;
pub mod history;
pub mod prompts;
pub mod reads;
pub mod types;
pub use crate::events::EventError;
use crate::events::EventRepository;
use crate::history_files::SessionIndex;
pub use agent::{AgentConversation, AgentHistoryView, ConversationReadView};
pub use forks::ForkError;
use parrot_domain::contracts::async_ops::BoxFuture;
use std::sync::Arc;
pub use types::*;

/// Derived JSONL is refreshed from SQLite only; it is never an input authority.
/// The concrete file implementation belongs to T19.
pub trait HistoryProjection: Send + Sync {
    fn selected_root(&self) -> &parrot_domain::contracts::paths::SelectedSessionRoot;
    fn validate_session(&self, agent: &str) -> Result<(), EventError>;
    fn refresh<'a>(
        &'a self,
        repository: &'a ConversationRepository,
        agent: &'a str,
    ) -> BoxFuture<'a, Result<(), EventError>>;
    fn rebuild<'a>(
        &'a self,
        repository: &'a ConversationRepository,
        agent: &'a str,
    ) -> BoxFuture<'a, Result<(), EventError>>;
}
#[derive(Clone)]
pub struct ConversationRepository {
    pub(crate) events: EventRepository,
    pub(crate) image_store: Option<crate::artifacts::ImageArtifactStore>,
    pub(crate) projection: Option<Arc<dyn HistoryProjection>>,
    pub(crate) metadata: SessionIndex,
}
impl ConversationRepository {
    pub fn new(events: EventRepository, metadata: SessionIndex) -> Result<Self, EventError> {
        if metadata.selected_root() != events.database().selected_root() {
            return Err(EventError::Argument(
                "session metadata must use the selected session root".into(),
            ));
        }
        Ok(Self {
            events,
            metadata,
            image_store: None,
            projection: None,
        })
    }
    pub fn with_image_store(
        mut self,
        store: crate::artifacts::ImageArtifactStore,
    ) -> Result<Self, EventError> {
        if store.selected_root() != self.events.database().selected_root() {
            return Err(EventError::Argument(
                "image store must use the selected session root".into(),
            ));
        }
        self.image_store = Some(store);
        Ok(self)
    }
    pub fn events(&self) -> &EventRepository {
        &self.events
    }
    pub fn bind_agent_history(
        &self,
        agent: String,
        projection: Arc<dyn HistoryProjection>,
    ) -> Result<AgentHistoryView, EventError> {
        if agent.trim().is_empty() {
            return Err(EventError::Argument(
                "agentSessionId must not be empty or whitespace".into(),
            ));
        }
        if projection.selected_root() != self.events.database().selected_root() {
            return Err(EventError::Argument(
                "history projection must use the selected session root".into(),
            ));
        }
        let mut repository = self.clone();
        repository.projection = Some(projection);
        Ok(AgentHistoryView::new(repository, agent))
    }
    pub(crate) fn validate(&self, agent: &str) -> Result<(), EventError> {
        if let Some(file) = &self.projection {
            file.validate_session(agent)?;
        }
        Ok(())
    }
    pub async fn refresh_agent_history(&self, agent: &str) -> Result<(), EventError> {
        if let Some(file) = &self.projection {
            file.refresh(self, agent).await?;
        }
        Ok(())
    }
    pub(crate) async fn rebuild_agent_history(&self, agent: &str) -> Result<(), EventError> {
        if let Some(file) = &self.projection {
            file.rebuild(self, agent).await?;
        }
        Ok(())
    }
}
