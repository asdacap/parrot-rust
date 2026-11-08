use crate::presenters::*;

#[derive(Default)]
pub struct ProgressToolPresenter;
impl ToolPresenter for ProgressToolPresenter {
    fn tool_name(&self) -> &str {
        "progress"
    }
    fn present_live(
        &self,
        _call: &ToolCallPresentation,
        frame: usize,
    ) -> PresenterResult<Presentation> {
        Ok(Presentation::Tool(ToolReport::live(
            "progress",
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
        if status == ToolTerminalStatus::Succeeded {
            return Ok(None);
        }
        Ok(Some(Presentation::Tool(ToolReport::terminal(
            "progress",
            terminal.describe_block(ToolBlockKind::None),
            status,
            self.metadata(),
        ))))
    }
}
