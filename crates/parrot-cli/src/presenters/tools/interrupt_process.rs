use crate::presenters::*;
use serde_json::Value;

#[derive(Default)]
pub struct InterruptProcessToolPresenter;
impl InterruptProcessToolPresenter {
    fn label(call: &ToolCallPresentation) -> PresenterResult<String> {
        let root: Value = serde_json::from_str(&call.arguments_json)?;
        let name = root
            .as_object()
            .and_then(|root| root.get("name"))
            .and_then(Value::as_str)
            .ok_or_else(|| PresenterError("interrupt_process requires a string name.".into()))?;
        let signal = match root.get("signal") {
            None | Some(Value::Null) => 2,
            Some(value) => value
                .as_i64()
                .and_then(|value| i32::try_from(value).ok())
                .ok_or_else(|| {
                    PresenterError("interrupt_process signal must be an integer.".into())
                })?,
        };
        Ok(format!("signal {signal} {name}"))
    }
}
impl ToolPresenter for InterruptProcessToolPresenter {
    fn tool_name(&self) -> &str {
        "interrupt_process"
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
        let status = terminal.resolve_process_status();
        let block = if status == ToolTerminalStatus::Succeeded {
            ToolBlock::empty()
        } else {
            ToolBlock::from_error(if terminal.result_present {
                terminal.result.clone()
            } else {
                terminal.error.clone()
            })
        };
        Ok(Some(Presentation::Tool(ToolReport::terminal(
            label,
            block,
            status,
            self.metadata(),
        ))))
    }
}
