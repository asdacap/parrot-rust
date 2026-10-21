use crate::presenters::*;
use serde_json::Value;

#[derive(Default)]
pub struct AgentSpawnToolPresenter;

impl ToolPresenter for AgentSpawnToolPresenter {
    fn tool_name(&self) -> &str {
        "agent_spawn"
    }

    fn metadata(&self) -> ToolPresentationMetadata {
        ToolPresentationMetadata {
            success_icon: "♟".into(),
            terminal_only: true,
            ..Default::default()
        }
    }

    fn present_live(
        &self,
        call: &ToolCallPresentation,
        frame: usize,
    ) -> PresenterResult<Presentation> {
        let arguments = parse(&call.arguments_json)?;
        Ok(Presentation::Tool(ToolReport::live(
            label(&arguments)?,
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
        let arguments = parse(&call.arguments_json)?;
        let label = label(&arguments)?;
        let status = terminal.resolve_status();
        if status == ToolTerminalStatus::Succeeded {
            let metadata = [
                ("agent", ""),
                ("model", ""),
                ("scope", "scope: "),
                ("fork", "fork: "),
            ]
            .into_iter()
            .filter_map(|(key, prefix)| {
                arguments
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
                    .map(|v| format!("{prefix}{v}"))
            })
            .collect::<Vec<_>>()
            .join(" · ");
            return Ok(Some(Presentation::AgentSpawn {
                name: optional_string(&arguments, "name")?
                    .unwrap_or_default()
                    .into(),
                metadata,
                prompt: required_nullable_string(&arguments, "prompt")?.into(),
            }));
        }
        let block = if matches!(
            status,
            ToolTerminalStatus::Errored | ToolTerminalStatus::ReportedFailure
        ) {
            terminal.describe_block(ToolBlockKind::None)
        } else {
            ToolBlock::from_completed_input(completed_input(&arguments)?)
        };
        Ok(Some(Presentation::Tool(ToolReport::terminal(
            label,
            block,
            status,
            self.metadata(),
        ))))
    }
}

fn parse(json: &str) -> PresenterResult<Value> {
    let value: Value = serde_json::from_str(json)?;
    if !value.is_object() {
        return Err(PresenterError("agent_spawn requires an object.".into()));
    }
    Ok(value)
}
fn optional_string<'a>(value: &'a Value, name: &str) -> PresenterResult<Option<&'a str>> {
    match value.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        _ => Err(PresenterError(format!(
            "agent_spawn {name} must be a string."
        ))),
    }
}
fn required_nullable_string<'a>(value: &'a Value, name: &str) -> PresenterResult<&'a str> {
    if value.get(name).is_none() {
        return Err(PresenterError(format!("agent_spawn requires {name}.")));
    }
    Ok(optional_string(value, name)?.unwrap_or_default())
}
fn label(value: &Value) -> PresenterResult<String> {
    Ok(
        match optional_string(value, "name")?.filter(|v| !v.is_empty()) {
            Some(name) => format!("Start agent {name}"),
            None => "Start agent".into(),
        },
    )
}
fn completed_input(value: &Value) -> PresenterResult<String> {
    let mut values = Vec::new();
    for key in ["name", "agent", "model", "scope"] {
        if let Some(text) = value
            .get(key)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
        {
            values.push(format!("{key}: {text}"));
        }
    }
    let fork = optional_string(value, "fork")?
        .filter(|v| !v.is_empty())
        .unwrap_or("empty");
    values.push(format!("fork: {fork}"));
    let prompt = required_nullable_string(value, "prompt")?;
    if !prompt.is_empty() {
        values.push(if prompt.contains('\n') {
            format!(
                "prompt: |-\n{}",
                prompt
                    .split('\n')
                    .map(|line| format!("  {line}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        } else {
            format!("prompt: {prompt}")
        });
    }
    Ok(values.join("\n"))
}
