use crate::presenters::*;
use crate::terminal_layout::ToolYamlFormatter;
use serde_json::Value;
#[derive(Default)]
pub struct ReadToolPresenter;
impl ToolPresenter for ReadToolPresenter {
    fn tool_name(&self) -> &str {
        "read"
    }
    fn metadata(&self) -> ToolPresentationMetadata {
        ToolPresentationMetadata {
            style: ToolPresentationStyle::Muted,
            ..Default::default()
        }
    }
    fn present_live(
        &self,
        call: &ToolCallPresentation,
        frame: usize,
    ) -> PresenterResult<Presentation> {
        Ok(Presentation::Tool(ToolReport::live(
            format!("read {}", path(&call.arguments_json)?),
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
        let path = path(&call.arguments_json)?;
        let status = terminal.resolve_status();
        let block = if matches!(
            status,
            ToolTerminalStatus::Errored | ToolTerminalStatus::ReportedFailure
        ) {
            ToolBlock::from_error(format!(
                "{}\n---\n{}",
                ToolYamlFormatter::format(&call.arguments_json)?,
                terminal.describe_block(ToolBlockKind::None).text
            ))
        } else {
            ToolBlock::empty()
        };
        Ok(Some(Presentation::Tool(ToolReport::terminal(
            format!("read {path}"),
            block,
            status,
            self.metadata(),
        ))))
    }
}
fn path(json: &str) -> PresenterResult<String> {
    let value: Value = serde_json::from_str(json)?;
    Ok(value
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .into())
}
