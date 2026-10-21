use crate::presenters::*;
use serde_json::Value;

#[derive(Default)]
pub struct QueuePushToolPresenter;
impl ToolPresenter for QueuePushToolPresenter {
    fn tool_name(&self) -> &str {
        "queue_push"
    }
    fn present_live(
        &self,
        call: &ToolCallPresentation,
        frame: usize,
    ) -> PresenterResult<Presentation> {
        let input = Input::parse(&call.arguments_json)?;
        Ok(Presentation::Tool(ToolReport::live(
            input.label(input.close),
            ToolBlock::from_queue(input.details.join("\n")),
            self.metadata(),
            frame,
        )))
    }
    fn present_terminal(
        &self,
        call: &ToolCallPresentation,
        terminal: &ToolTerminalPresentation,
    ) -> PresenterResult<Option<Presentation>> {
        let input = Input::parse(&call.arguments_json)?;
        let status = terminal.resolve_status();
        let closed = if status == ToolTerminalStatus::Succeeded {
            input.close
        } else {
            terminal.result_present
                && terminal.result.starts_with("error: queue: '")
                && terminal.result.contains("' is closed")
        };
        let block = if status == ToolTerminalStatus::Succeeded {
            ToolBlock::from_queue(input.details.join("\n"))
        } else {
            terminal.describe_block(ToolBlockKind::None)
        };
        Ok(Some(Presentation::Tool(ToolReport::terminal(
            input.label(closed),
            block,
            status,
            self.metadata(),
        ))))
    }
}
struct Input {
    name: String,
    details: Vec<String>,
    close: bool,
}
impl Input {
    fn parse(json: &str) -> PresenterResult<Self> {
        let root: Value = serde_json::from_str(json)?;
        let name = root
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                PresenterError(
                    "Queue push arguments require a name and exactly one item source.".into(),
                )
            })?;
        let details = match (root.get("items"), root.get("source_file")) {
            (Some(items), None) => items
                .as_array()
                .ok_or_else(|| PresenterError("Queue push items must be an array.".into()))?
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| PresenterError("Queue push items must be strings.".into()))
                })
                .collect::<PresenterResult<Vec<_>>>()?,
            (None, Some(source_file)) => vec![format!(
                "source_file: {}",
                source_file
                    .as_str()
                    .filter(|file| !file.is_empty())
                    .ok_or_else(|| PresenterError(
                        "Queue push source_file must be a non-empty string.".into()
                    ))?
            )],
            _ => {
                return Err(PresenterError(
                    "Queue push arguments require exactly one of items or source_file.".into(),
                ));
            }
        };
        let close = match root.get("close") {
            None => false,
            Some(Value::Bool(value)) => *value,
            _ => return Err(PresenterError("Queue push close must be a boolean.".into())),
        };
        Ok(Self {
            name: name.into(),
            details,
            close,
        })
    }
    fn label(&self, closed: bool) -> String {
        format!(
            "Push to queue {} · {}",
            self.name,
            if closed { "closed" } else { "open" }
        )
    }
}
