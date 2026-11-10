use crate::presenters::*;
use parrot_tools::agents::input::AskInput;

#[derive(Default)]
pub struct AskToolPresenter;

impl ToolPresenter for AskToolPresenter {
    fn tool_name(&self) -> &str {
        "ask"
    }

    fn present_live(
        &self,
        call: &ToolCallPresentation,
        frame: usize,
    ) -> PresenterResult<Presentation> {
        let input =
            AskInput::parse(&call.arguments_json).map_err(|error| PresenterError(error.0))?;
        Ok(Presentation::Tool(ToolReport::live(
            format!("Ask {}", call.resolve_agent_reference(&input.name)),
            ToolBlock::from_text(format!("Question:\n{}", input.message)),
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
            AskInput::parse(&call.arguments_json).map_err(|error| PresenterError(error.0))?;
        let status = terminal.resolve_status();
        let block = if status == ToolTerminalStatus::Succeeded {
            let mut text = format!("Question:\n{}", input.message);
            if terminal.result_present {
                text.push_str(&format!("\nResponse:\n{}", terminal.result));
            }
            ToolBlock::from_text(text)
        } else {
            terminal.describe_block(ToolBlockKind::None)
        };
        Ok(Some(Presentation::Tool(ToolReport::terminal(
            format!("Ask {}", call.resolve_agent_reference(&input.name)),
            block,
            status,
            self.metadata(),
        ))))
    }
}
