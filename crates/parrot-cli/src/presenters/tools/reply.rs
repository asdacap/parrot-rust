use crate::presenters::*;
use parrot_tools::agents::input::ReplyInput;

#[derive(Default)]
pub struct ReplyToolPresenter;

impl ToolPresenter for ReplyToolPresenter {
    fn tool_name(&self) -> &str {
        "reply"
    }

    fn present_live(
        &self,
        call: &ToolCallPresentation,
        frame: usize,
    ) -> PresenterResult<Presentation> {
        let input =
            ReplyInput::parse(&call.arguments_json).map_err(|error| PresenterError(error.0))?;
        Ok(Presentation::Tool(ToolReport::live(
            format!("Reply to {}", input.request_id),
            ToolBlock::from_text(input.message),
            self.metadata(),
            frame,
        )))
    }

    fn present_terminal(
        &self,
        call: &ToolCallPresentation,
        terminal: &ToolTerminalPresentation,
    ) -> PresenterResult<Option<Presentation>> {
        let input =
            ReplyInput::parse(&call.arguments_json).map_err(|error| PresenterError(error.0))?;
        let status = terminal.resolve_status();
        let block = if status == ToolTerminalStatus::Succeeded {
            ToolBlock::from_text(input.message)
        } else {
            terminal.describe_block(ToolBlockKind::None)
        };
        Ok(Some(Presentation::Tool(ToolReport::terminal(
            format!("Reply to {}", input.request_id),
            block,
            status,
            self.metadata(),
        ))))
    }
}
