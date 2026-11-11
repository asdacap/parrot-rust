use parrot_domain::{
    contracts::filenames::FilenameGenerator,
    values::{activity::AgentSessionActivity, identifiers::Identifier},
};
use parrot_foundation::{clock::SystemClock, template_catalog::PromptTemplateCatalog};
use parrot_runtime::tool_settlement::{ToolDiagnostics, ToolExecutionConfig, ToolPublisher};
use parrot_storage::{
    artifacts::output_blobs::ToolOutputBlobStore, conversation::AgentHistoryView,
    diagnostics::DiagnosticEvent,
};
use std::{collections::BTreeMap, sync::Arc};

struct Names;
impl FilenameGenerator for Names {
    fn next_filename(&self) -> String {
        Identifier::event_id().unwrap()
    }
}
struct Diagnostics;
impl ToolDiagnostics for Diagnostics {
    fn write(&self, _: &DiagnosticEvent) {}
}

pub fn tool_execution_config(
    history: &AgentHistoryView,
    templates: Arc<PromptTemplateCatalog>,
    publisher: Arc<ToolPublisher>,
) -> ToolExecutionConfig {
    ToolExecutionConfig {
        templates,
        output_blobs: Arc::new(
            ToolOutputBlobStore::for_session(
                history.repository().events().database().selected_root(),
                "scratch",
                Arc::new(Names),
            )
            .unwrap(),
        ),
        publisher,
        activity: Arc::new(AgentSessionActivity::new(Arc::new(SystemClock))),
        diagnostics: Arc::new(Diagnostics),
        image_bytes_per_cycle: 0,
        path_variables: BTreeMap::new(),
    }
}
