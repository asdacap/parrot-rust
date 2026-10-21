use crate::presenters::*;
use serde_json::Value;

#[derive(Default)]
pub struct AgentSendToolPresenter;

impl AgentSendToolPresenter {
    fn input(call: &ToolCallPresentation) -> PresenterResult<(String, String)> {
        let root: Value = serde_json::from_str(&call.arguments_json)?;
        let string = |key: &str| -> PresenterResult<String> {
            match root.get(key) {
                Some(Value::String(value)) => Ok(value.clone()),
                Some(Value::Null) => Ok(String::new()),
                _ => Err(PresenterError(format!("agent_send requires string {key}."))),
            }
        };
        Ok((string("name")?, string("message")?))
    }

    fn recipient(result: &str) -> Option<String> {
        serde_json::from_str::<Value>(result)
            .ok()?
            .as_object()?
            .get("name")?
            .as_str()
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
    }
}

impl ToolPresenter for AgentSendToolPresenter {
    fn tool_name(&self) -> &str {
        "agent_send"
    }

    fn present_live(
        &self,
        call: &ToolCallPresentation,
        frame: usize,
    ) -> PresenterResult<Presentation> {
        let root: Value = serde_json::from_str(&call.arguments_json)?;
        let name = match root.get("name") {
            Some(Value::String(value)) => value.as_str(),
            Some(Value::Null) => "",
            _ => return Err(PresenterError("agent_send requires string name.".into())),
        };
        Ok(Presentation::Tool(ToolReport::live(
            format!("Send to {}", call.resolve_agent_reference(name)),
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
        let (name, message) = Self::input(call)?;
        let status = terminal.resolve_status();
        let recipient = if status == ToolTerminalStatus::Succeeded {
            Self::recipient(&terminal.result).unwrap_or_else(|| call.resolve_agent_reference(&name))
        } else {
            call.resolve_agent_reference(&name)
        };
        let block = if status == ToolTerminalStatus::Succeeded {
            ToolBlock::from_text(message)
        } else {
            terminal.describe_block(ToolBlockKind::None)
        };
        Ok(Some(Presentation::Tool(ToolReport::terminal(
            format!("Send to {recipient}"),
            block,
            status,
            self.metadata(),
        ))))
    }
}
