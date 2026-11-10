//! Retained tool factories, inventories and execution configuration.
mod model;
mod snapshot;
pub use model::*;
pub use snapshot::ToolSnapshot;

use crate::event_broker::BrokerError;
use parrot_domain::{
    contracts::events::EventPublisher, protocol::messages::parrot::Event,
    values::activity::AgentSessionActivity,
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_storage::{
    artifacts::output_blobs::ToolOutputBlobStore,
    diagnostics::{DiagnosticEvent, FileDiagnosticLog},
};
use std::sync::Arc;

pub trait ToolDiagnostics: Send + Sync {
    fn write(&self, event: &DiagnosticEvent);
}
impl ToolDiagnostics for FileDiagnosticLog {
    fn write(&self, event: &DiagnosticEvent) {
        FileDiagnosticLog::write(self, event);
    }
}
pub type ToolPublisher = dyn EventPublisher<Event = Event, Error = BrokerError>;
pub struct ToolExecutionConfig {
    pub templates: Arc<PromptTemplateCatalog>,
    pub output_blobs: Arc<ToolOutputBlobStore>,
    pub publisher: Arc<ToolPublisher>,
    pub activity: Arc<AgentSessionActivity>,
    pub diagnostics: Arc<dyn ToolDiagnostics>,
    pub image_bytes_per_cycle: i32,
    pub path_variables: std::collections::BTreeMap<String, String>,
}
