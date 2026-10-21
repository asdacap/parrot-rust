use crate::presenters::*;
use serde_json::Value;

#[derive(Default)]
pub struct QueueTakeToolPresenter;
impl QueueTakeToolPresenter {
    fn input(call: &ToolCallPresentation) -> PresenterResult<(String, i32)> {
        let root: Value = serde_json::from_str(&call.arguments_json)?;
        let name = root
            .as_object()
            .and_then(|root| root.get("name"))
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| PresenterError("Queue take arguments require a name.".into()))?;
        let count = match root.get("count") {
            None => 1,
            Some(value) => value
                .as_i64()
                .and_then(|value| i32::try_from(value).ok())
                .filter(|value| *value >= 1)
                .ok_or_else(|| {
                    PresenterError("Queue take count must be a positive integer.".into())
                })?,
        };
        Ok((name.into(), count))
    }
    fn label(name: &str, count: i32) -> String {
        format!(
            "Take from queue {name} · up to {count} {}",
            if count == 1 { "item" } else { "items" }
        )
    }
    fn result(result: &str) -> PresenterResult<(String, String, i32, bool, Vec<String>)> {
        let root: Value = serde_json::from_str(result)?;
        let fail = || {
            PresenterError(
                "Queue take result requires a name, size, closed state, and items.".into(),
            )
        };
        let name = root
            .as_object()
            .and_then(|root| root.get("name"))
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or_else(fail)?;
        let size = root
            .get("size")
            .and_then(Value::as_i64)
            .and_then(|size| i32::try_from(size).ok())
            .filter(|size| *size >= 0)
            .ok_or_else(fail)?;
        let closed = root
            .get("closed")
            .and_then(Value::as_bool)
            .ok_or_else(fail)?;
        let items = root
            .get("items")
            .and_then(Value::as_array)
            .ok_or_else(fail)?
            .iter()
            .map(|item| {
                item.as_str().map(str::to_owned).ok_or_else(|| {
                    PresenterError("Queue take result items must be strings.".into())
                })
            })
            .collect::<PresenterResult<Vec<_>>>()?;
        let description = match root.get("description") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(value)) => value.clone(),
            _ => {
                return Err(PresenterError(
                    "Queue take result description must be a string.".into(),
                ));
            }
        };
        Ok((name.into(), description, size, closed, items))
    }
}
impl ToolPresenter for QueueTakeToolPresenter {
    fn tool_name(&self) -> &str {
        "queue_take"
    }
    fn present_live(
        &self,
        call: &ToolCallPresentation,
        frame: usize,
    ) -> PresenterResult<Presentation> {
        let (name, count) = Self::input(call)?;
        Ok(Presentation::Tool(ToolReport::live(
            Self::label(&name, count),
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
        let (name, count) = Self::input(call)?;
        let status = terminal.resolve_status();
        if status != ToolTerminalStatus::Succeeded {
            return Ok(Some(Presentation::Tool(ToolReport::terminal(
                Self::label(&name, count),
                terminal.describe_block(ToolBlockKind::None),
                status,
                self.metadata(),
            ))));
        }
        let (name, description, size, closed, items) = Self::result(&terminal.result)?;
        let mut label = Self::label(&name, count);
        if !description.is_empty() {
            label.push_str(&format!(" · {description}"));
        }
        if closed {
            label.push_str(" · closed");
        }
        let text = std::iter::once(format!("{size} remaining"))
            .chain(items)
            .collect::<Vec<_>>()
            .join("\n");
        Ok(Some(Presentation::Tool(ToolReport::terminal(
            label,
            ToolBlock::from_queue(text),
            status,
            self.metadata(),
        ))))
    }
}
