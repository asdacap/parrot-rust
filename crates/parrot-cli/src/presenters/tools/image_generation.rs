use crate::presenters::*;
use serde_json::Value;

#[derive(Default)]
pub struct ImageGenerationToolPresenter;
impl ImageGenerationToolPresenter {
    fn input(call: &ToolCallPresentation) -> PresenterResult<(String, Vec<String>)> {
        let root: Value = serde_json::from_str(&call.arguments_json)?;
        let root = root
            .as_object()
            .ok_or_else(|| PresenterError("imagegen requires object arguments.".into()))?;
        let text = |key: &str| -> PresenterResult<String> {
            match root.get(key) {
                None | Some(Value::Null) => Ok(String::new()),
                Some(Value::String(value)) => Ok(value.clone()),
                _ => Err(PresenterError(format!("imagegen {key} must be a string."))),
            }
        };
        let path = text("output_path")?;
        let mut details = vec![text("prompt")?];
        if let Some(value) = root.get("referenced_image_paths") {
            let references = value.as_array().ok_or_else(|| {
                PresenterError("imagegen referenced_image_paths must be an array.".into())
            })?;
            let references = references
                .iter()
                .map(|value| match value {
                    Value::String(value) => Ok(value.clone()),
                    Value::Null => Ok(String::new()),
                    _ => Err(PresenterError(
                        "imagegen reference paths must be strings.".into(),
                    )),
                })
                .collect::<PresenterResult<Vec<_>>>()?;
            if !references.is_empty() {
                details.push(format!("references: {}", references.join(", ")));
            }
        }
        Ok((path, details))
    }
}
impl ToolPresenter for ImageGenerationToolPresenter {
    fn tool_name(&self) -> &str {
        "imagegen"
    }
    fn metadata(&self) -> ToolPresentationMetadata {
        ToolPresentationMetadata {
            style: ToolPresentationStyle::Muted,
            ..Default::default()
        }
    }
    fn present_live(
        &self,
        _call: &ToolCallPresentation,
        frame: usize,
    ) -> PresenterResult<Presentation> {
        Ok(Presentation::Tool(ToolReport::live(
            "running imagegen",
            ToolBlock::empty(),
            self.metadata(),
            frame,
        )))
    }
    fn present_started(
        &self,
        call: &ToolCallPresentation,
    ) -> PresenterResult<Option<Presentation>> {
        let (path, details) = Self::input(call)?;
        Ok(Some(Presentation::Tool(ToolReport::terminal(
            format!("imagegen {path}"),
            ToolBlock::from_details(details),
            ToolTerminalStatus::Succeeded,
            ToolPresentationMetadata {
                success_icon: "○".into(),
                ..self.metadata()
            },
        ))))
    }
    fn present_terminal(
        &self,
        call: &ToolCallPresentation,
        terminal: &ToolTerminalPresentation,
    ) -> PresenterResult<Option<Presentation>> {
        let (path, mut details) = Self::input(call)?;
        let status = terminal.resolve_status();
        if matches!(
            status,
            ToolTerminalStatus::Errored | ToolTerminalStatus::ReportedFailure
        ) {
            let error = terminal.describe_block(ToolBlockKind::None).text;
            if !error.is_empty() {
                details.push(error);
            }
        }
        let mut block = ToolBlock::from_details(details);
        if matches!(
            status,
            ToolTerminalStatus::Errored | ToolTerminalStatus::ReportedFailure
        ) && block.kind != ToolBlockKind::None
        {
            block = ToolBlock::from_error(block.text);
        }
        Ok(Some(Presentation::Tool(ToolReport::terminal(
            format!("imagegen {path}"),
            block,
            status,
            self.metadata(),
        ))))
    }
}
