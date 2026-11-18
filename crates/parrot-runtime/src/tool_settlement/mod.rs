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
    artifacts::output_blobs::{ToolOutputBlobStore, tool_output_preview},
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

/// Renders the templated spill notice for a saved tool output blob and appends
/// the templated preview block when the output fits the preview budget.
/// The limit text carries the configured spill threshold into the wording.
pub(crate) fn render_spill_notice(
    templates: &PromptTemplateCatalog,
    path: &str,
    limit_text: &str,
    output: &str,
) -> Result<String, String> {
    let mut notice = templates
        .render(
            "tool-result.spill-notice",
            &[
                (
                    "path".to_owned(),
                    serde_json::Value::String(path.to_owned()),
                ),
                (
                    "limit".to_owned(),
                    serde_json::Value::String(limit_text.to_owned()),
                ),
            ],
            &parrot_storage::events::NeverCancelled,
        )
        .map_err(|error| error.to_string())?;
    if let Some(preview) = tool_output_preview(output) {
        let block = templates
            .render(
                "tool-result.spill-preview",
                &[
                    ("head".to_owned(), serde_json::Value::String(preview.head)),
                    ("tail".to_owned(), serde_json::Value::String(preview.tail)),
                    (
                        "omitted".to_owned(),
                        serde_json::Value::from(preview.omitted),
                    ),
                ],
                &parrot_storage::events::NeverCancelled,
            )
            .map_err(|error| error.to_string())?;
        notice.push('\n');
        notice.push_str(&block);
    }
    Ok(notice)
}
