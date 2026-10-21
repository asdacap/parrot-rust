use crate::presenters::*;
use crate::terminal_layout::ToolYamlFormatter;

#[derive(Default)]
pub struct GenericToolPresenter;

impl ToolPresenter for GenericToolPresenter {
    fn tool_name(&self) -> &str {
        "generic"
    }

    fn present_live(
        &self,
        call: &ToolCallPresentation,
        frame: usize,
    ) -> PresenterResult<Presentation> {
        let block = if call.arguments_json.is_empty() {
            ToolBlock::empty()
        } else {
            ToolBlock::from_text(ToolYamlFormatter::format(&call.arguments_json)?)
        };
        Ok(Presentation::Tool(ToolReport::live(
            &call.tool_name,
            block,
            self.metadata(),
            frame,
        )))
    }

    fn present_terminal(
        &self,
        call: &ToolCallPresentation,
        terminal: &ToolTerminalPresentation,
    ) -> PresenterResult<Option<Presentation>> {
        let mut details = Vec::new();
        if !call.arguments_json.is_empty() {
            details.push(ToolYamlFormatter::format(&call.arguments_json)?);
        }
        if terminal.result_present {
            let separator = if call.arguments_json.is_empty() {
                ""
            } else {
                "---\n"
            };
            details.push(format!(
                "{separator}{}",
                ToolYamlFormatter::format(&terminal.result)?
            ));
        }
        if !terminal.error.is_empty() {
            details.push(terminal.error.clone());
        }
        let status = terminal.resolve_status();
        let mut block = ToolBlock::from_details(details);
        if matches!(
            status,
            ToolTerminalStatus::Errored | ToolTerminalStatus::ReportedFailure
        ) && block.kind != ToolBlockKind::None
        {
            block = ToolBlock::from_error(block.text);
        }
        let suffix = if terminal.status == ToolTerminalStatus::Cancelled {
            " cancelled"
        } else {
            ""
        };
        Ok(Some(Presentation::Tool(ToolReport::terminal(
            format!("tool call {}{suffix}", call.tool_name),
            block,
            status,
            self.metadata(),
        ))))
    }
}
