use crate::presenters::*;
use serde_json::Value;

pub struct FileMutationToolPresenter {
    tool_name: String,
}
impl FileMutationToolPresenter {
    pub fn new(tool_name: impl Into<String>) -> Self {
        Self {
            tool_name: tool_name.into(),
        }
    }
}
impl ToolPresenter for FileMutationToolPresenter {
    fn tool_name(&self) -> &str {
        &self.tool_name
    }
    fn present_live(
        &self,
        call: &ToolCallPresentation,
        frame: usize,
    ) -> PresenterResult<Presentation> {
        Ok(Presentation::Tool(ToolReport::live(
            format!("{} {}", self.tool_name, path(&call.arguments_json)?),
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
        let path = path(&call.arguments_json)?;
        let block = if matches!(
            status,
            ToolTerminalStatus::Errored | ToolTerminalStatus::ReportedFailure
        ) {
            terminal.describe_block(ToolBlockKind::None)
        } else if !terminal.result_present
            || terminal.result.is_empty()
            || terminal.result == "No changes made."
        {
            ToolBlock::empty()
        } else if terminal
            .result
            .split_once('\n')
            .is_some_and(|(first, remaining)| {
                first.starts_with("--- ") && remaining.starts_with("+++ ")
            })
        {
            ToolBlock::from_diff(terminal.result.clone())
        } else {
            ToolBlock::from_text(terminal.result.clone())
        };
        Ok(Some(if block.kind == ToolBlockKind::Diff {
            Presentation::Diff {
                tool_name: self.tool_name.clone(),
                path,
                diff: block.text,
            }
        } else {
            Presentation::Tool(ToolReport::terminal(
                format!("{} {path}", self.tool_name),
                block,
                status,
                self.metadata(),
            ))
        }))
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
