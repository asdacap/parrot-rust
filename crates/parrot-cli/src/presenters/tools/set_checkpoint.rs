use crate::presenters::*;
use serde_json::Value;

#[derive(Default)]
pub struct SetCheckpointToolPresenter;
impl SetCheckpointToolPresenter {
    fn label(call: &ToolCallPresentation) -> PresenterResult<String> {
        let root: Value = serde_json::from_str(&call.arguments_json)?;
        let title = root
            .as_object()
            .and_then(|root| root.get("title"))
            .and_then(Value::as_str)
            .ok_or_else(|| PresenterError("set_checkpoint requires a string title.".into()))?;
        Ok(format!("Set checkpoint {title}"))
    }
}
impl ToolPresenter for SetCheckpointToolPresenter {
    fn tool_name(&self) -> &str {
        "set_checkpoint"
    }
    fn present_live(
        &self,
        call: &ToolCallPresentation,
        frame: usize,
    ) -> PresenterResult<Presentation> {
        Ok(Presentation::Tool(ToolReport::live(
            Self::label(call)?,
            ToolBlock::empty(),
            self.metadata(),
            frame,
        )))
    }
    fn present_terminal(
        &self,
        call: &ToolCallPresentation,
        terminal: &ToolTerminalPresentation,
    ) -> PresenterResult<Option<Presentation>> {
        let status = terminal.resolve_status();
        Ok(Some(Presentation::Tool(ToolReport::terminal(
            Self::label(call)?,
            if status == ToolTerminalStatus::Succeeded {
                ToolBlock::empty()
            } else {
                terminal.describe_block(ToolBlockKind::None)
            },
            status,
            self.metadata(),
        ))))
    }
}
