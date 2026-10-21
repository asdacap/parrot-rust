use crate::presenters::*;
use serde_json::Value;

#[derive(Default)]
pub struct GlobToolPresenter;
impl GlobToolPresenter {
    fn label(call: &ToolCallPresentation) -> PresenterResult<String> {
        let root: Value = serde_json::from_str(&call.arguments_json)?;
        let pattern = root
            .get("pattern")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let path = root.get("path").and_then(Value::as_str).unwrap_or_default();
        Ok(if path.is_empty() {
            format!("glob \"{pattern}\"")
        } else {
            format!("glob \"{pattern}\" in {path}")
        })
    }
}
impl ToolPresenter for GlobToolPresenter {
    fn tool_name(&self) -> &str {
        "glob"
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
        let mut label = Self::label(call)?;
        let status = terminal.resolve_status();
        if status == ToolTerminalStatus::Succeeded {
            let count = terminal
                .result
                .split('\n')
                .filter(|line| {
                    let line = line.trim();
                    !line.is_empty() && !line.starts_with('[')
                })
                .count();
            label.push_str(&format!(
                " · {count} {}",
                if count == 1 { "path" } else { "paths" }
            ));
        }
        Ok(Some(Presentation::Tool(ToolReport::terminal(
            label,
            if status == ToolTerminalStatus::Succeeded {
                ToolBlock::empty()
            } else {
                terminal.describe_block(ToolBlockKind::None)
            },
            status,
            self.metadata(),
        ))))
    }
}
