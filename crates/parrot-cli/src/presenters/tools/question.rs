use crate::presenters::*;
use serde_json::Value;

#[derive(Default)]
pub struct QuestionToolPresenter;
impl ToolPresenter for QuestionToolPresenter {
    fn tool_name(&self) -> &str {
        "question"
    }
    fn metadata(&self) -> ToolPresentationMetadata {
        ToolPresentationMetadata {
            modeline: true,
            ..Default::default()
        }
    }
    fn present_live(
        &self,
        call: &ToolCallPresentation,
        frame: usize,
    ) -> PresenterResult<Presentation> {
        let questions = questions(&call.arguments_json)?;
        let prompt = questions
            .first()
            .map(prompt)
            .transpose()?
            .unwrap_or("Question");
        Ok(Presentation::Tool(ToolReport::live(
            label(questions.len()),
            ToolBlock::from_details(vec![prompt.into()]),
            self.metadata(),
            frame,
        )))
    }
    fn present_child_started(
        &self,
        call: &ToolCallPresentation,
    ) -> PresenterResult<Option<Presentation>> {
        let questions = questions(&call.arguments_json)?;
        let mut details = Vec::new();
        for question in &questions {
            details.push(prompt(question)?.into());
            if let Some(options) = question.get("options") {
                for option in options
                    .as_array()
                    .ok_or_else(|| PresenterError("question options must be an array.".into()))?
                {
                    let label = option
                        .get("label")
                        .ok_or_else(|| PresenterError("question option requires label.".into()))?;
                    let text = match label {
                        Value::Null => "",
                        Value::String(value) => value,
                        _ => {
                            return Err(PresenterError(
                                "question option label must be a string.".into(),
                            ));
                        }
                    };
                    details.push(format!("  - {text}"));
                }
            }
        }
        let metadata = ToolPresentationMetadata {
            style: ToolPresentationStyle::Muted,
            success_icon: "○".into(),
            ..self.metadata()
        };
        Ok(Some(Presentation::Tool(ToolReport::terminal(
            label(questions.len()),
            ToolBlock::from_details(details),
            ToolTerminalStatus::Succeeded,
            metadata,
        ))))
    }
    fn present_terminal(
        &self,
        call: &ToolCallPresentation,
        terminal: &ToolTerminalPresentation,
    ) -> PresenterResult<Option<Presentation>> {
        let questions = questions(&call.arguments_json)?;
        for question in &questions {
            let _ = prompt(question)?;
        }
        Ok(Some(Presentation::Tool(ToolReport::terminal(
            label(questions.len()),
            terminal.describe_block(ToolBlockKind::Text),
            terminal.resolve_status(),
            self.metadata(),
        ))))
    }
}
fn questions(json: &str) -> PresenterResult<Vec<Value>> {
    let root: Value = serde_json::from_str(json)?;
    root.get("questions")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| PresenterError("question requires a questions array.".into()))
}
fn prompt(question: &Value) -> PresenterResult<&str> {
    match question.get("prompt") {
        Some(Value::Null) => Ok("Question"),
        Some(Value::String(value)) => Ok(value),
        _ => Err(PresenterError("question requires a string prompt.".into())),
    }
}
fn label(count: usize) -> String {
    format!(
        "Question · {count} {}",
        if count == 1 { "item" } else { "items" }
    )
}
