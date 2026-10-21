use crate::presenters::*;
use serde_json::Value;

#[derive(Default)]
pub struct AgentStatusToolPresenter;
impl ToolPresenter for AgentStatusToolPresenter {
    fn tool_name(&self) -> &str {
        "agent_status"
    }
    fn present_live(
        &self,
        call: &ToolCallPresentation,
        frame: usize,
    ) -> PresenterResult<Presentation> {
        Ok(Presentation::Tool(ToolReport::live(
            label(call)?,
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
        let block = if status == ToolTerminalStatus::Succeeded && terminal.result_present {
            ToolBlock::from_status(terminal.result.clone())
        } else {
            terminal.describe_block(ToolBlockKind::Text)
        };
        Ok(Some(Presentation::Tool(ToolReport::terminal(
            label(call)?,
            block,
            status,
            self.metadata(),
        ))))
    }
}
fn label(call: &ToolCallPresentation) -> PresenterResult<String> {
    let value: Value = serde_json::from_str(&call.arguments_json)?;
    let name = value
        .as_object()
        .and_then(|object| object.get("name"))
        .and_then(Value::as_str)
        .ok_or_else(|| PresenterError("agent_status requires a string name.".into()))?;
    Ok(format!(
        "Agent status · {}",
        call.resolve_agent_reference(name)
    ))
}
