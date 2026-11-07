use crate::presenters::*;
use crate::terminal_layout::ToolYamlFormatter;
use serde_json::Value;
#[derive(Default)]
pub struct ReadToolPresenter;
impl ReadToolPresenter {
    fn label(call: &ToolCallPresentation) -> PresenterResult<String> {
        let value: Value = serde_json::from_str(&call.arguments_json)?;
        let path = value
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let offset = value.get("offset").and_then(Value::as_i64);
        let limit = value.get("limit").and_then(Value::as_i64);
        Ok(if offset.is_some() || limit.is_some() {
            let offset = offset.unwrap_or(1);
            let limit = limit.unwrap_or(2000);
            let end = offset.saturating_add(limit.saturating_sub(1));
            format!("read L{offset}-L{end} {path}")
        } else {
            format!("read {path}")
        })
    }
}
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
        let label = Self::label(call)?;
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
            label,
            block,
            status,
            self.metadata(),
        ))))
    }
}
