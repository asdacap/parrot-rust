use crate::presenters::tools::generic::GenericToolPresenter;
use crate::presenters::*;
use serde_json::Value;
use std::sync::Arc;

pub struct GetAgentTasksToolPresenter {
    generic: Arc<dyn ToolPresenter>,
}
impl Default for GetAgentTasksToolPresenter {
    fn default() -> Self {
        Self::new(Arc::new(GenericToolPresenter))
    }
}
impl GetAgentTasksToolPresenter {
    pub fn new(generic: Arc<dyn ToolPresenter>) -> Self {
        Self { generic }
    }
    fn name(call: &ToolCallPresentation) -> PresenterResult<Option<String>> {
        let root: Value = serde_json::from_str(&call.arguments_json)?;
        let root = root
            .as_object()
            .ok_or_else(|| PresenterError("get_agent_tasks requires an object.".into()))?;
        match root.get("name") {
            None => Ok(None),
            Some(Value::String(name)) if !name.trim().is_empty() => Ok(Some(name.clone())),
            _ => Err(PresenterError(
                "get_agent_tasks requires a nonblank name.".into(),
            )),
        }
    }
    fn label(name: Option<&str>) -> String {
        name.map_or_else(
            || "Agent tasks".into(),
            |name| format!("Agent task · {name}"),
        )
    }
    fn text<'a>(task: &'a Value, key: &str) -> PresenterResult<&'a str> {
        task.get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| PresenterError(format!("Task {key} must be a string.")))
    }
    fn hidden(task: &Value) -> PresenterResult<bool> {
        match task.get("hidden") {
            None => Ok(false),
            Some(Value::Bool(value)) => Ok(*value),
            _ => Err(PresenterError("Task hidden must be a boolean.".into())),
        }
    }
    fn icon(state: &str) -> PresenterResult<&'static str> {
        match state {
            "pending" => Ok("○"),
            "running" => Ok("◐"),
            "succeeded" => Ok("✓"),
            "failed" => Ok("✗"),
            "canceled" => Ok("■"),
            _ => Err(PresenterError("Unsupported task execution state.".into())),
        }
    }
    fn optional(
        task: &Value,
        key: &str,
        label: &str,
        indent: &str,
        rows: &mut Vec<String>,
    ) -> PresenterResult<()> {
        if task.get(key).is_some() {
            rows.push(format!("{indent}{label}: {}", Self::text(task, key)?));
        }
        Ok(())
    }
    fn definition(
        task: &Value,
        indent: &str,
        rows: &mut Vec<String>,
        execution: bool,
    ) -> PresenterResult<()> {
        if execution {
            rows.push(format!("Description: {}", Self::text(task, "description")?));
            Self::optional(task, "agent_name", "Agent", indent, rows)?;
        }
        let dependencies = task
            .get("dependencies")
            .and_then(Value::as_array)
            .ok_or_else(|| PresenterError("Task dependencies must be an array.".into()))?
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| PresenterError("Task dependencies must be strings.".into()))
            })
            .collect::<PresenterResult<Vec<_>>>()?;
        if !dependencies.is_empty() {
            rows.push(format!("{indent}Dependencies: {}", dependencies.join(", ")));
        }
        Self::optional(task, "model", "Model", indent, rows)?;
        rows.push(format!(
            "{indent}Acceptance criteria: {}",
            Self::text(task, "acceptance_criteria")?
        ));
        if execution {
            Self::optional(task, "result", "Result", indent, rows)?;
            Self::optional(task, "failure", "Failure", indent, rows)?;
        } else if ["state", "result", "failure"]
            .iter()
            .any(|key| task.get(key).is_some())
        {
            return Err(PresenterError(
                "Declared child definitions must not contain execution state.".into(),
            ));
        }
        let payload = task
            .get("payload")
            .ok_or_else(|| PresenterError("Task payload is required.".into()))?;
        if let Some(text) = payload.as_str() {
            rows.push(format!("{indent}Instruction: {text}"));
            return Ok(());
        }
        let children = payload
            .as_array()
            .ok_or_else(|| PresenterError("Task payload must be a string or array.".into()))?;
        let mut shown = Vec::new();
        for child in children {
            if !Self::hidden(child)? {
                shown.push(child);
            }
        }
        if !shown.is_empty() {
            rows.push(format!("{indent}Declared child definitions:"));
        }
        for child in shown {
            rows.push(format!(
                "{indent}  {} · {}",
                Self::text(child, "name")?,
                Self::text(child, "description")?
            ));
            Self::definition(child, &format!("{indent}    "), rows, false)?;
        }
        Ok(())
    }
}
impl ToolPresenter for GetAgentTasksToolPresenter {
    fn tool_name(&self) -> &str {
        "get_agent_tasks"
    }
    fn present_live(
        &self,
        call: &ToolCallPresentation,
        frame: usize,
    ) -> PresenterResult<Presentation> {
        let name = Self::name(call)?;
        Ok(Presentation::Tool(ToolReport::live(
            Self::label(name.as_deref()),
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
        let name = Self::name(call)?;
        let status = terminal.resolve_status();
        if status != ToolTerminalStatus::Succeeded || !terminal.result_present {
            return self.generic.present_terminal(call, terminal);
        }
        let document: Value = serde_json::from_str(&terminal.result)?;
        let mut rows = Vec::new();
        if name.is_none() {
            let tasks = document
                .as_array()
                .ok_or_else(|| PresenterError("Task summary must be an array.".into()))?;
            for task in tasks {
                if Self::hidden(task)? {
                    continue;
                }
                let state = Self::text(task, "state")?;
                rows.push(format!(
                    "{} {state} · {} · {}",
                    Self::icon(state)?,
                    Self::text(task, "name")?,
                    Self::text(task, "description")?
                ));
            }
            if rows.is_empty() {
                if !tasks.is_empty() {
                    return Ok(None);
                }
                rows.push("No agent tasks.".into());
            }
        } else {
            if Self::hidden(&document)? {
                return Ok(None);
            }
            let state = Self::text(&document, "state")?;
            Self::icon(state)?;
            rows.push(format!("{} · {state}", Self::text(&document, "name")?));
            Self::definition(&document, "", &mut rows, true)?;
        }
        Ok(Some(Presentation::Tool(ToolReport::terminal(
            Self::label(name.as_deref()),
            ToolBlock::from_details(rows),
            status,
            self.metadata(),
        ))))
    }
}
