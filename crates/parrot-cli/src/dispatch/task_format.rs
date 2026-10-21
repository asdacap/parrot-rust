//! Plain-text task declarations and snapshots used by the basic CLI.

use parrot_domain::protocol::messages::parrot as wire;
use protobuf::Message;

use crate::terminal_layout::TerminalText;

/// Source `AgentDurationFormatter`: round milliseconds to the nearest second.
pub struct DurationFormatter;

impl DurationFormatter {
    pub fn format(elapsed_milliseconds: i64) -> String {
        // C#'s unchecked addition wraps, including at Int64.MaxValue.
        let seconds = elapsed_milliseconds.wrapping_add(500).max(0) / 1000;
        match seconds {
            0..60 => format!("{seconds}s"),
            60..3600 => format!("{}m {:02}s", seconds / 60, seconds % 60),
            _ => format!(
                "{}h {:02}m {:02}s",
                seconds / 3600,
                (seconds / 60) % 60,
                seconds % 60
            ),
        }
    }
}

/// The plan report deliberately exposes only names and descriptions, not payloads.
pub struct AgentTaskDeclarationFormatter;

impl AgentTaskDeclarationFormatter {
    pub fn format(declarations: &[wire::PlanTaskDeclaration]) -> Vec<String> {
        Self::render(declarations)
            .into_iter()
            .map(|line| line.text)
            .collect()
    }

    pub fn format_for_width(
        declarations: &[wire::PlanTaskDeclaration],
        columns: usize,
    ) -> Vec<String> {
        let width = columns.max(1);
        Self::render(declarations)
            .into_iter()
            .flat_map(|line| {
                let layout_width = width.max(TerminalText::width(&line.indent) + 1);
                TerminalText::layout_words_hanging(&line.text, layout_width, &line.indent)
            })
            .collect()
    }

    fn render(declarations: &[wire::PlanTaskDeclaration]) -> Vec<RenderedLine> {
        let mut lines = Vec::new();
        append_declarations(declarations, "", &mut lines);
        if !lines.is_empty() {
            lines.insert(0, RenderedLine::new("Agent tasks:", ""));
        }
        lines
    }
}

struct RenderedLine {
    text: String,
    indent: String,
}

impl RenderedLine {
    fn new(text: impl Into<String>, indent: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            indent: indent.into(),
        }
    }
}

fn append_declarations(
    declarations: &[wire::PlanTaskDeclaration],
    indent: &str,
    lines: &mut Vec<RenderedLine>,
) {
    for declaration in declarations
        .iter()
        .filter(|declaration| !declaration.hidden)
    {
        lines.push(RenderedLine::new(
            format!(
                "{indent}- name: {}",
                TerminalText::sanitize_line(&declaration.name)
            ),
            format!("{indent}  "),
        ));
        let field_indent = format!("{indent}  ");
        let continuation_indent = format!("{field_indent}  ");
        let sanitized = TerminalText::sanitize(&declaration.description);
        let physical_lines: Vec<_> = sanitized.split('\n').collect();
        if physical_lines.len() == 1
            && !physical_lines[0].is_empty()
            && TerminalText::width(physical_lines[0]) <= 80
        {
            lines.push(RenderedLine::new(
                format!("{field_indent}description: {}", physical_lines[0]),
                &continuation_indent,
            ));
        } else {
            lines.push(RenderedLine::new(
                format!("{field_indent}description:"),
                &continuation_indent,
            ));
            for line in &physical_lines {
                if line.is_empty() && physical_lines.len() == 1 {
                    continue;
                }
                lines.push(RenderedLine::new(
                    format!("{continuation_indent}{line}"),
                    &continuation_indent,
                ));
            }
        }
        if let Some(wire::plan_task_declaration::Payload::Children(children)) = &declaration.payload
        {
            append_declarations(&children.tasks, &format!("{indent}    "), lines);
        }
    }
}

/// The basic view of the actual protobuf task snapshot; no JSON projection.
pub struct ProgressFormatter;

impl ProgressFormatter {
    pub const KIND: &'static str = "agent_task_progress";

    /// Malformed task snapshot bytes are an error, not an unrelated/silent event.
    pub fn try_read(
        event: &wire::Event,
    ) -> protobuf::Result<Option<wire::AgentTaskProgressSnapshot>> {
        match &event.payload {
            Some(wire::event::Payload::ToolSnapshot(snapshot)) if snapshot.kind == Self::KIND => {
                wire::AgentTaskProgressSnapshot::parse_from_bytes(&snapshot.data).map(Some)
            }
            _ => Ok(None),
        }
    }

    pub fn format(snapshot: &wire::AgentTaskProgressSnapshot) -> Vec<String> {
        let mut lines = Vec::new();
        for root in snapshot.root_nodes.iter().filter(|root| !root.hidden) {
            append_progress_node(root, "", &mut lines);
            append_progress_children(&root.children, "", &mut lines);
        }
        if !lines.is_empty() {
            lines.insert(0, "Agent tasks:".into());
        }
        lines
    }
}

fn append_progress_children(
    nodes: &[wire::AgentTaskProgressNode],
    ancestors: &str,
    lines: &mut Vec<String>,
) {
    let mut visible = nodes.iter().filter(|node| !node.hidden).peekable();
    while let Some(node) = visible.next() {
        let last = visible.peek().is_none();
        append_progress_node(
            node,
            &format!("{ancestors}{} ", if last { "└──" } else { "├──" }),
            lines,
        );
        append_progress_children(
            &node.children,
            &format!("{ancestors}{}", if last { "    " } else { "│   " }),
            lines,
        );
    }
}

fn append_progress_node(node: &wire::AgentTaskProgressNode, lead: &str, lines: &mut Vec<String>) {
    use wire::AgentTaskProgressStatus as Status;
    let icon = match node.status.enum_value() {
        Ok(Status::AGENT_TASK_PROGRESS_STATUS_PENDING) => "○",
        Ok(Status::AGENT_TASK_PROGRESS_STATUS_RUNNING) => "◐",
        Ok(Status::AGENT_TASK_PROGRESS_STATUS_SUCCEEDED) => "✓",
        Ok(Status::AGENT_TASK_PROGRESS_STATUS_FAILED) => "✗",
        Ok(Status::AGENT_TASK_PROGRESS_STATUS_CANCELED) => "■",
        _ => "?",
    };
    let text = if node.description.is_empty() {
        &node.name
    } else {
        &node.description
    };
    lines.push(format!(
        "{lead}{icon} {}",
        TerminalText::sanitize_line(text)
    ));
}
