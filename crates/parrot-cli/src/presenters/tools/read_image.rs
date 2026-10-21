use crate::presenters::*;
use serde_json::Value;

#[derive(Default)]
pub struct ReadImageToolPresenter;
impl ReadImageToolPresenter {
    fn label(call: &ToolCallPresentation) -> PresenterResult<String> {
        let root: Value = serde_json::from_str(&call.arguments_json)?;
        let path = root
            .as_object()
            .and_then(|root| root.get("path"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        Ok(format!("read image {path}"))
    }
    fn bytes(bytes: i64) -> String {
        if bytes < 1024 {
            format!("{bytes} B")
        } else if bytes < 1024 * 1024 {
            format!("{:.1} KB", bytes as f64 / 1024.0)
        } else {
            format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
        }
    }
}
impl ToolPresenter for ReadImageToolPresenter {
    fn tool_name(&self) -> &str {
        "read_image"
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
        let block = if matches!(
            status,
            ToolTerminalStatus::Errored | ToolTerminalStatus::ReportedFailure
        ) {
            terminal.describe_block(ToolBlockKind::None)
        } else {
            if let Some(artifact) = terminal.artifacts.first() {
                label.push_str(&format!(
                    " ({}, {}x{})",
                    Self::bytes(artifact.byte_length),
                    artifact.width,
                    artifact.height
                ));
            }
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
