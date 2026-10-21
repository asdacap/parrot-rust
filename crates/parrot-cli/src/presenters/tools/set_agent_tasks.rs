use crate::presenters::tools::generic::GenericToolPresenter;
use crate::presenters::*;
use std::sync::Arc;

pub struct SetAgentTasksToolPresenter {
    generic: Arc<dyn ToolPresenter>,
}
impl Default for SetAgentTasksToolPresenter {
    fn default() -> Self {
        Self::new(Arc::new(GenericToolPresenter))
    }
}
impl SetAgentTasksToolPresenter {
    pub fn new(generic: Arc<dyn ToolPresenter>) -> Self {
        Self { generic }
    }
}
impl ToolPresenter for SetAgentTasksToolPresenter {
    fn tool_name(&self) -> &str {
        "set_agent_tasks"
    }
    fn metadata(&self) -> ToolPresentationMetadata {
        ToolPresentationMetadata {
            redacted_input_fields: vec!["path".into(), "tasks".into()],
            ..Default::default()
        }
    }
    fn present_live(
        &self,
        _call: &ToolCallPresentation,
        frame: usize,
    ) -> PresenterResult<Presentation> {
        Ok(Presentation::Tool(ToolReport::live(
            "setting agent tasks",
            ToolBlock::empty(),
            ToolPresentationMetadata::default(),
            frame,
        )))
    }
    fn present_terminal(
        &self,
        call: &ToolCallPresentation,
        terminal: &ToolTerminalPresentation,
    ) -> PresenterResult<Option<Presentation>> {
        let mut without_arguments = call.clone();
        without_arguments.arguments_json.clear();
        self.generic.present_terminal(&without_arguments, terminal)
    }
}
