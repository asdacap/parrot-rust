//! Enhanced task rows, worker visibility and retained-child graph nesting.

use std::collections::{BTreeMap, BTreeSet};

use parrot_domain::protocol::messages::parrot as wire;
use protobuf::EnumOrUnknown;

use crate::terminal_layout::TerminalText;

use super::{PresenterError, PresenterResult};

// The basic and enhanced paths share the already-delivered declaration helper.
pub use crate::dispatch::task_format::AgentTaskDeclarationFormatter;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveModelAliasIcon {
    pub glyph: String,
    pub color: wire::TurnModelAliasIconColor,
}

impl LiveModelAliasIcon {
    pub fn convert(icon: Option<&wire::TurnModelAliasIcon>) -> PresenterResult<Option<Self>> {
        icon.map(|icon| {
            let color = icon
                .color
                .enum_value()
                .map_err(|_| PresenterError("The model alias icon color is invalid.".to_owned()))?;
            Ok(Self {
                glyph: icon.glyph.clone(),
                color,
            })
        })
        .transpose()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskAgentLine {
    pub text: String,
    pub model_alias_icon: Option<LiveModelAliasIcon>,
    /// UTF-16 code units, matching the source's string indices.
    pub glyph_start_index: usize,
}

impl TaskAgentLine {
    pub fn new(text: impl Into<String>, model_alias_icon: Option<LiveModelAliasIcon>) -> Self {
        Self {
            text: text.into(),
            model_alias_icon,
            glyph_start_index: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentTaskRow {
    pub text: String,
    pub hanging_indent: String,
    pub status: Option<EnumOrUnknown<wire::AgentTaskProgressStatus>>,
    pub model_alias_icon: Option<LiveModelAliasIcon>,
    /// UTF-16 code units, matching the source's string indices.
    pub glyph_start_index: usize,
}

impl AgentTaskRow {
    pub fn create(lead: &str, description: &str) -> Self {
        Self {
            text: format!("{lead}{description}"),
            hanging_indent: " ".repeat(TerminalText::width(lead)),
            status: None,
            model_alias_icon: None,
            glyph_start_index: 0,
        }
    }
}

pub struct AgentTaskProgressFormatter;

impl AgentTaskProgressFormatter {
    pub fn try_read(
        event: &wire::Event,
    ) -> PresenterResult<Option<wire::AgentTaskProgressSnapshot>> {
        crate::dispatch::task_format::ProgressFormatter::try_read(event)
            .map_err(|error| PresenterError(error.to_string()))
    }

    pub fn format(snapshot: &wire::AgentTaskProgressSnapshot) -> Vec<String> {
        Self::format_rows(snapshot, None, None)
            .into_iter()
            .map(|row| row.text)
            .collect()
    }

    pub fn format_rows(
        snapshot: &wire::AgentTaskProgressSnapshot,
        agent_lines: Option<&BTreeMap<String, TaskAgentLine>>,
        active_agent_session_ids: Option<&BTreeSet<String>>,
    ) -> Vec<AgentTaskRow> {
        let nodes = select_visible_nodes(&snapshot.root_nodes, false, active_agent_session_ids);
        if nodes.is_empty() {
            return Vec::new();
        }
        let mut rows = vec![AgentTaskRow::create("", "Agent tasks:")];
        for node in &nodes {
            append_node(node, "", agent_lines, &mut rows);
            append_children(&node.children, "", agent_lines, &mut rows);
        }
        rows
    }

    pub fn nest(
        snapshot: &wire::AgentTaskProgressSnapshot,
        trees: &BTreeMap<String, wire::AgentTaskProgressSnapshot>,
    ) -> wire::AgentTaskProgressSnapshot {
        let mut nested = snapshot.clone();
        nest_nodes(&mut nested.root_nodes, trees, &mut BTreeSet::new());
        nested
    }

    pub fn get_embedded_agent_session_ids(
        snapshot: &wire::AgentTaskProgressSnapshot,
        agent_lines: &BTreeMap<String, TaskAgentLine>,
        active_agent_session_ids: &BTreeSet<String>,
    ) -> Vec<String> {
        let nodes =
            select_visible_nodes(&snapshot.root_nodes, false, Some(active_agent_session_ids));
        let mut flattened = Vec::new();
        flatten(&nodes, &mut flattened);
        flattened
            .into_iter()
            .filter(|node| agent_lines.contains_key(&node.agent_session_id))
            .map(|node| node.agent_session_id.clone())
            .collect()
    }

    pub fn get_visible_child_graph_owner_session_ids(
        snapshot: &wire::AgentTaskProgressSnapshot,
        active_agent_session_ids: &BTreeSet<String>,
    ) -> Vec<String> {
        let nodes =
            select_visible_nodes(&snapshot.root_nodes, false, Some(active_agent_session_ids));
        let mut flattened = Vec::new();
        flatten(&nodes, &mut flattened);
        flattened
            .into_iter()
            .filter(|node| !node.children.is_empty())
            .map(|node| node.agent_session_id.clone())
            .collect()
    }

    pub fn display_text(node: &wire::AgentTaskProgressNode) -> String {
        TerminalText::sanitize_line(if node.description.is_empty() {
            &node.name
        } else {
            &node.description
        })
    }
}

fn select_visible_nodes(
    nodes: &[wire::AgentTaskProgressNode],
    hidden_ancestor: bool,
    active_agent_session_ids: Option<&BTreeSet<String>>,
) -> Vec<wire::AgentTaskProgressNode> {
    let mut visible = Vec::new();
    for node in nodes {
        let hidden = hidden_ancestor || node.hidden;
        let children = select_visible_nodes(&node.children, hidden, active_agent_session_ids);
        if hidden
            && children.is_empty()
            && !active_agent_session_ids
                .is_some_and(|active| active.contains(&node.agent_session_id))
        {
            continue;
        }
        let mut copy = node.clone();
        copy.children = children;
        visible.push(copy);
    }
    visible
}

fn append_children(
    nodes: &[wire::AgentTaskProgressNode],
    ancestors: &str,
    agent_lines: Option<&BTreeMap<String, TaskAgentLine>>,
    rows: &mut Vec<AgentTaskRow>,
) {
    for (index, node) in nodes.iter().enumerate() {
        let last = index + 1 == nodes.len();
        let descendants = format!("{ancestors}{}", if last { "    " } else { "│   " });
        append_node(
            node,
            &format!("{ancestors}{} ", if last { "└──" } else { "├──" }),
            agent_lines,
            rows,
        );
        append_children(&node.children, &descendants, agent_lines, rows);
    }
}

fn append_node(
    node: &wire::AgentTaskProgressNode,
    lead: &str,
    agent_lines: Option<&BTreeMap<String, TaskAgentLine>>,
    rows: &mut Vec<AgentTaskRow>,
) {
    let mut row =
        if let Some(line) = agent_lines.and_then(|lines| lines.get(&node.agent_session_id)) {
            let mut row = AgentTaskRow::create(
                lead,
                &format!(
                    "{} {}",
                    line.text.trim_end(),
                    AgentTaskProgressFormatter::display_text(node)
                ),
            );
            row.model_alias_icon = line.model_alias_icon.clone();
            row.glyph_start_index = lead.encode_utf16().count() + line.glyph_start_index;
            row
        } else {
            AgentTaskRow::create(
                &format!("{lead}{} ", icon(node.status)),
                &AgentTaskProgressFormatter::display_text(node),
            )
        };
    row.status = Some(node.status);
    rows.push(row);
}

fn nest_nodes(
    nodes: &mut [wire::AgentTaskProgressNode],
    trees: &BTreeMap<String, wire::AgentTaskProgressSnapshot>,
    ancestors: &mut BTreeSet<String>,
) {
    for node in nodes {
        let Some(tree) = trees.get(&node.agent_session_id) else {
            continue;
        };
        if !ancestors.insert(node.agent_session_id.clone()) {
            continue;
        }
        node.children = tree.root_nodes.clone();
        nest_nodes(&mut node.children, trees, ancestors);
        ancestors.remove(&node.agent_session_id);
    }
}

fn flatten<'a>(
    nodes: &'a [wire::AgentTaskProgressNode],
    result: &mut Vec<&'a wire::AgentTaskProgressNode>,
) {
    for node in nodes {
        result.push(node);
        flatten(&node.children, result);
    }
}

fn icon(status: EnumOrUnknown<wire::AgentTaskProgressStatus>) -> &'static str {
    use wire::AgentTaskProgressStatus as Status;
    match status.enum_value() {
        Ok(Status::AGENT_TASK_PROGRESS_STATUS_PENDING) => "○",
        Ok(Status::AGENT_TASK_PROGRESS_STATUS_RUNNING) => "◐",
        Ok(Status::AGENT_TASK_PROGRESS_STATUS_SUCCEEDED) => "✓",
        Ok(Status::AGENT_TASK_PROGRESS_STATUS_FAILED) => "✗",
        Ok(Status::AGENT_TASK_PROGRESS_STATUS_CANCELED) => "■",
        _ => "?",
    }
}
