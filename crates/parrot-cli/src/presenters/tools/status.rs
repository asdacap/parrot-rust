use crate::presenters::*;

#[derive(Default)]
pub struct StatusToolPresenter;
impl ToolPresenter for StatusToolPresenter {
    fn tool_name(&self) -> &str {
        "status"
    }
    fn present_live(
        &self,
        _call: &ToolCallPresentation,
        frame: usize,
    ) -> PresenterResult<Presentation> {
        Ok(Presentation::Tool(ToolReport::live(
            "status",
            ToolBlock::empty(),
            self.metadata(),
            frame,
        )))
    }
    fn present_terminal(
        &self,
        _call: &ToolCallPresentation,
        terminal: &ToolTerminalPresentation,
    ) -> PresenterResult<Option<Presentation>> {
        let status = terminal.resolve_status();
        let block = if status == ToolTerminalStatus::Succeeded && terminal.result_present {
            ToolBlock::from_status(terminal.result.clone())
        } else {
            terminal.describe_block(ToolBlockKind::Text)
        };
        Ok(Some(Presentation::Tool(ToolReport::terminal(
            "tool call status",
            block,
            status,
            self.metadata(),
        ))))
    }
}
