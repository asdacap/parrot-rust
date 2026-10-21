use crate::presenters::*;
use serde_json::Value;
#[derive(Default)]
pub struct WriteStdinToolPresenter;
impl ToolPresenter for WriteStdinToolPresenter {
    fn tool_name(&self) -> &str {
        "write_stdin"
    }
    fn metadata(&self) -> ToolPresentationMetadata {
        ToolPresentationMetadata {
            redacted_input_fields: vec!["input".into()],
            suppress_terminal_details: true,
            ..Default::default()
        }
    }
    fn present_live(
        &self,
        call: &ToolCallPresentation,
        frame: usize,
    ) -> PresenterResult<Presentation> {
        Ok(Presentation::Tool(ToolReport::live(
            describe(call),
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
        Ok(Some(Presentation::Tool(ToolReport::terminal(
            describe(call),
            ToolBlock::empty(),
            terminal.resolve_status(),
            self.metadata(),
        ))))
    }
}
fn describe(call: &ToolCallPresentation) -> String {
    let root = serde_json::from_str::<Value>(&call.arguments_json)
        .ok()
        .filter(Value::is_object);
    let name = root
        .as_ref()
        .and_then(|root| root.get("name"))
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("process");
    let characters = root
        .as_ref()
        .and_then(|root| root.get("input"))
        .and_then(Value::as_str)
        .map(|input| format!("{} chars", count(input)))
        .unwrap_or_else(|| "input".into());
    format!("write {characters} to {name}")
}
fn count(input: &str) -> usize {
    if let Some(number) = input
        .strip_prefix("<redacted: ")
        .and_then(|value| value.strip_suffix(" chars>"))
    {
        if !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()) {
            if let Ok(number) = number.parse::<i32>() {
                return number as usize;
            }
        }
    }
    input.chars().count()
}
