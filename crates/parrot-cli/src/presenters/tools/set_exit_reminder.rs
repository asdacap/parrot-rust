use crate::presenters::*;
use serde_json::Value;

#[derive(Default)]
pub struct SetExitReminderToolPresenter;
impl SetExitReminderToolPresenter {
    fn label(call: &ToolCallPresentation) -> PresenterResult<String> {
        let root: Value = serde_json::from_str(&call.arguments_json)?;
        let root = root.as_object().ok_or_else(|| {
            PresenterError("set_exit_reminder requires a string title and description.".into())
        })?;
        let title = root.get("title").and_then(Value::as_str);
        let description = root.get("description").and_then(Value::as_str);
        let (Some(title), Some(description)) = (title, description) else {
            return Err(PresenterError(
                "set_exit_reminder requires a string title and description.".into(),
            ));
        };
        Ok(format!("Set exit reminder · {title} · {description}"))
    }
}
impl ToolPresenter for SetExitReminderToolPresenter {
    fn tool_name(&self) -> &str {
        "set_exit_reminder"
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
        if status == ToolTerminalStatus::Succeeded {
            return Ok(None);
        }
        Ok(Some(Presentation::Tool(ToolReport::terminal(
            Self::label(call)?,
            terminal.describe_block(ToolBlockKind::None),
            status,
            self.metadata(),
        ))))
    }
}
