use crate::presenters::*;
use serde_json::Value;

#[derive(Default)]
pub struct AnswerToolPresenter;

impl AnswerToolPresenter {
    fn input(call: &ToolCallPresentation) -> PresenterResult<(String, ToolBlock)> {
        let root: Value = serde_json::from_str(&call.arguments_json)?;
        let name = match root.get("agent_name") {
            Some(Value::String(value)) => value.as_str(),
            Some(Value::Null) => "",
            _ => return Err(PresenterError("answer requires string agent_name.".into())),
        };
        let mut answers = Vec::new();
        if let Some(Value::Array(values)) = root.get("answers") {
            for value in values {
                answers.push(match value {
                    Value::String(value) => value.clone(),
                    Value::Null => String::new(),
                    _ => return Err(PresenterError("Answer values must be strings.".into())),
                });
            }
        }
        let count = answers.len();
        let label = format!(
            "Answer · {} · {count} {}",
            call.resolve_agent_reference(name),
            if count == 1 { "item" } else { "items" }
        );
        let text = if count == 1 {
            answers.remove(0)
        } else {
            answers
                .iter()
                .enumerate()
                .map(|(index, answer)| {
                    let prefix = format!("{}. ", index + 1);
                    format!(
                        "{prefix}{}",
                        answer.replace('\n', &format!("\n{}", " ".repeat(prefix.len())))
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        Ok((label, ToolBlock::from_text(text)))
    }
}

impl ToolPresenter for AnswerToolPresenter {
    fn tool_name(&self) -> &str {
        "answer"
    }
    fn present_live(
        &self,
        call: &ToolCallPresentation,
        frame: usize,
    ) -> PresenterResult<Presentation> {
        let (label, block) = Self::input(call)?;
        Ok(Presentation::Tool(ToolReport::live(
            label,
            block,
            self.metadata(),
            frame,
        )))
    }
    fn present_terminal(
        &self,
        call: &ToolCallPresentation,
        terminal: &ToolTerminalPresentation,
    ) -> PresenterResult<Option<Presentation>> {
        let (label, block) = Self::input(call)?;
        let status = terminal.resolve_status();
        Ok(Some(Presentation::Tool(ToolReport::terminal(
            label,
            if status == ToolTerminalStatus::Succeeded {
                block
            } else {
                terminal.describe_block(ToolBlockKind::None)
            },
            status,
            self.metadata(),
        ))))
    }
}
