use crate::presenters::*;
use serde_json::Value;
#[derive(Default)]
pub struct WebFetchToolPresenter;
impl ToolPresenter for WebFetchToolPresenter {
    fn tool_name(&self) -> &str {
        "web_fetch"
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
            format!("web fetch {}", request(&call.arguments_json)?),
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
            format!("web fetch {}", request(&call.arguments_json)?),
            terminal.describe_block(ToolBlockKind::Text),
            terminal.resolve_status(),
            self.metadata(),
        ))))
    }
}
fn request(json: &str) -> PresenterResult<String> {
    let value: Value = serde_json::from_str(json)?;
    let Some(url) = value.get("url").and_then(Value::as_str) else {
        return Ok(String::new());
    };
    let method = value.get("method").and_then(Value::as_str).unwrap_or("GET");
    let method: String = method
        .chars()
        .map(|character| {
            let mut upper = character.to_uppercase();
            let first = upper.next().unwrap_or(character);
            if upper.next().is_none() {
                first
            } else {
                character
            }
        })
        .collect();
    Ok(format!("{method} {url}"))
}
