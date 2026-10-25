//! Per-agent activity accumulation for enhanced terminal rendering.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use parrot_domain::protocol::messages::parrot as wire;
use parrot_domain::values::context::TokenEstimator;

use super::{
    LiveModelAliasIcon, PresenterResult, RuntimeUsage, ToolCallPresentation, ToolPresenterRegistry,
    ToolTerminalPresentation, ToolTerminalStatus,
};
use crate::terminal_layout::TerminalText;

use super::activity::{ActivityItem, ActivityValue};

pub const AGENT_ACTIVITY_ID: &str = "agent";
pub const COMPACTION_ACTIVITY_ID: &str = "compaction";
pub const TOOL_ACTIVITY_PREFIX: &str = "tool:";

#[derive(Clone, Debug, Default)]
struct ToolCallState {
    name: String,
    arguments: String,
}

/// Durable-stream state is scoped to one agent identity, never to the input UI.
#[derive(Default)]
pub struct AgentSessionState {
    pub agent_session_id: String,
    pub name: Option<String>,
    pub activities: BTreeSet<String>,
    tool_calls: BTreeMap<String, ToolCallState>,
    folded_tools: BTreeMap<String, (String, u64)>,
    terminal_tools: BTreeSet<String>,
    tool_live: Mutex<BTreeMap<String, super::Presentation>>,
    response: String,
    reasoning: String,
    reasoning_tokens: i64,
    agent_task_revision: u64,
    terminal_committed: bool,
    request_attempt: u32,
    waiting_for_first_token: bool,
    agent_terminal_pending: bool,
    response_complete: bool,
    response_line_breaks: usize,
    folded_tool_order: u64,
    pub statistics: Option<wire::AgentStatisticsUpdatedEvent>,
    pub model_alias_icon: Option<LiveModelAliasIcon>,
    pub agent_task_progress: Option<wire::AgentTaskProgressSnapshot>,
    pub is_reasoning_summary: bool,
}

impl AgentSessionState {
    pub fn new(agent_session_id: impl Into<String>) -> Self {
        Self {
            agent_session_id: agent_session_id.into(),
            ..Self::default()
        }
    }

    pub fn name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.agent_session_id)
    }

    pub fn is_agent_active(&self) -> bool {
        self.activities.contains(AGENT_ACTIVITY_ID)
    }

    pub fn is_streaming_response(&self) -> bool {
        !self.waiting_for_first_token && self.request_attempt == 0 && !self.response.is_empty()
    }

    pub fn modeline_label(&self) -> String {
        format!("agent {}", self.name())
    }

    pub fn agent_label(&self) -> String {
        let mut labels = Vec::new();
        if let Some(statistics) = &self.statistics {
            let usage = RuntimeUsage {
                input_tokens: statistics.input_tokens,
                cached_input_tokens: statistics.cached_input_tokens,
                output_tokens: statistics.output_tokens,
                context_size: statistics.context_size,
                context_limit: statistics.context_limit,
                cost: 0.0,
            };
            let context = usage.format_context();
            if !context.is_empty() {
                labels.push(format!("({context})"));
            }
            let tokens = usage.format_tokens();
            if !tokens.is_empty() {
                labels.push(tokens);
            }
        }
        if let Some((name, _)) = self
            .folded_tools
            .values()
            .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
        {
            labels.push(format!("Working: {name}"));
        }
        labels.join(" ")
    }

    pub fn agent_spinner_text(&self) -> String {
        let label = self.agent_label();
        let text = if self.waiting_for_first_token {
            format!("{label} Waiting for first token…")
        } else {
            match self.request_attempt {
                0 => label,
                1 => format!("{label} Requesting…"),
                attempt => format!("{label} Requesting (attempt {attempt})…"),
            }
        };
        text.trim().to_owned()
    }

    pub fn observe_request_phase(&mut self, update: &wire::ProviderRequestPhaseChangedEvent) {
        self.waiting_for_first_token = self.is_agent_active()
            && update.phase
                == wire::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_HEADERS_RECEIVED.into();
        self.request_attempt = if self.is_agent_active()
            && update.phase == wire::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_REQUESTING.into()
        {
            update.attempt.max(1)
        } else {
            0
        };
    }

    pub fn start_turn(&mut self, icon: Option<LiveModelAliasIcon>) -> bool {
        if !self.activities.insert(AGENT_ACTIVITY_ID.to_owned()) {
            return false;
        }
        self.request_attempt = 0;
        self.waiting_for_first_token = false;
        self.model_alias_icon = icon;
        self.terminal_committed = false;
        self.agent_terminal_pending = true;
        self.folded_tools.clear();
        self.folded_tool_order = 0;
        self.response.clear();
        self.reasoning.clear();
        self.reasoning_tokens = 0;
        self.response_complete = false;
        self.response_line_breaks = 0;
        true
    }

    pub fn collect_response(&mut self, fragment: &str) {
        if self.terminal_committed {
            return;
        }
        for character in TerminalText::sanitize(fragment).chars() {
            if self.response_complete {
                break;
            }
            if character == '\n' && self.response_line_breaks == 9 {
                self.response_complete = true;
                break;
            }
            self.response.push(character);
            if character == '\n' {
                self.response_line_breaks += 1;
            }
        }
    }

    pub fn collect_reasoning(&mut self, chunk: &wire::ReasoningChunk) {
        self.is_reasoning_summary =
            chunk.kind == wire::ReasoningKind::REASONING_KIND_SUMMARY.into();
        let fragment = TerminalText::sanitize(&chunk.fragment);
        // A running estimate keeps each live frame from re-tokenizing the whole reasoning.
        self.reasoning_tokens += TokenEstimator::estimate_tokens(&fragment);
        self.reasoning.push_str(&fragment);
    }

    pub fn has_reasoning(&self) -> bool {
        !self.reasoning.is_empty()
    }

    pub(crate) fn reasoning_item(&self, frame: usize) -> ActivityValue {
        if self.is_reasoning_summary {
            ActivityValue::Streamed {
                marker: "✦".to_owned(),
                text: self.reasoning.clone(),
            }
        } else {
            ActivityValue::Spinner {
                text: format!(
                    "Thinking ({} tokens)…",
                    format_token_count(self.reasoning_tokens)
                ),
                frame,
            }
        }
    }

    pub(crate) fn drain_reasoning(&mut self) -> Option<ActivityValue> {
        let reasoning = std::mem::take(&mut self.reasoning);
        self.reasoning_tokens = 0;
        if reasoning.trim().is_empty() {
            return None;
        }
        Some(if self.is_reasoning_summary {
            ActivityValue::ReasoningSummary(reasoning)
        } else {
            ActivityValue::Notice {
                marker: "✦".to_owned(),
                text: format!(
                    "Reasoned for {} tokens…",
                    format_token_count(TokenEstimator::estimate_tokens(&reasoning))
                ),
            }
        })
    }

    pub fn drain_response(&mut self) -> Option<String> {
        let response = std::mem::take(&mut self.response);
        self.response_complete = false;
        self.response_line_breaks = 0;
        (!response.trim().is_empty()).then_some(response)
    }

    pub fn finish_child_turn(&mut self) -> Option<String> {
        if !self.activities.remove(AGENT_ACTIVITY_ID) {
            return None;
        }
        self.request_attempt = 0;
        self.waiting_for_first_token = false;
        self.terminal_committed = true;
        Some(self.drain_response().unwrap_or_default())
    }

    pub fn agent_terminal_pending(&self) -> bool {
        self.agent_terminal_pending
    }

    pub fn complete_agent(&mut self) {
        self.agent_terminal_pending = false;
        self.request_attempt = 0;
        self.waiting_for_first_token = false;
        self.terminal_committed = true;
        self.activities.remove(AGENT_ACTIVITY_ID);
        self.drain_response();
    }

    pub fn collect_tool_call(&mut self, chunk: &wire::ToolCallChunk) {
        if self.terminal_tools.contains(&chunk.tool_call_id) {
            return;
        }
        let call = self
            .tool_calls
            .entry(chunk.tool_call_id.clone())
            .or_default();
        if !chunk.tool_name.is_empty() {
            call.name.clone_from(&chunk.tool_name);
        }
        call.arguments.push_str(&chunk.arguments_fragment);
        self.tool_live
            .get_mut()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&chunk.tool_call_id);
    }

    pub fn start_tool(&mut self, tool: &wire::ToolStarted, fold: bool) -> bool {
        let activity_id = format!("{TOOL_ACTIVITY_PREFIX}{}", tool.tool_call_id);
        if self.activities.contains(&activity_id)
            || self.terminal_tools.contains(&tool.tool_call_id)
        {
            return false;
        }
        let call = self
            .tool_calls
            .entry(tool.tool_call_id.clone())
            .or_default();
        if !tool.tool_name.is_empty() {
            call.name.clone_from(&tool.tool_name);
        }
        self.tool_live
            .get_mut()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&tool.tool_call_id);
        self.activities.insert(activity_id);
        if fold {
            self.folded_tools.insert(
                tool.tool_call_id.clone(),
                (call.name.clone(), self.folded_tool_order),
            );
            self.folded_tool_order += 1;
        }
        true
    }

    pub fn is_tool_active(&self, id: &str) -> bool {
        self.activities
            .contains(&format!("{TOOL_ACTIVITY_PREFIX}{id}"))
    }

    pub fn is_folded_activity(&self, id: &str) -> bool {
        id.strip_prefix(TOOL_ACTIVITY_PREFIX)
            .is_some_and(|id| self.folded_tools.contains_key(id))
    }

    pub fn refresh_tool_presentations(&mut self) {
        self.tool_live
            .get_mut()
            .unwrap_or_else(|poison| poison.into_inner())
            .clear();
    }

    pub fn tool_call(
        &self,
        id: &str,
        resolver: Arc<dyn Fn(&str) -> String + Send + Sync>,
    ) -> ToolCallPresentation {
        let mut call = self.tool_calls.get(id).map_or_else(
            || ToolCallPresentation::new("", ""),
            |tool| ToolCallPresentation::new(&tool.name, &tool.arguments),
        );
        call.agent_reference_resolver = resolver;
        call
    }

    pub fn finish_tool(
        &mut self,
        published: &wire::Event,
        start_omitted: bool,
        resolver: Arc<dyn Fn(&str) -> String + Send + Sync>,
    ) -> PresenterResult<(ToolCallPresentation, ToolTerminalPresentation)> {
        use wire::event::Payload;
        let (id, name, mut terminal) = match published.payload.as_ref() {
            Some(Payload::ToolFinished(tool)) => (
                &tool.tool_call_id,
                &tool.tool_name,
                ToolTerminalPresentation {
                    status: ToolTerminalStatus::Succeeded,
                    result_present: tool.result.is_some(),
                    result: tool.result.clone().unwrap_or_default(),
                    yielded_process: tool.yielded_process.as_ref().cloned(),
                    artifacts: tool.artifacts.clone(),
                    ..ToolTerminalPresentation::default()
                },
            ),
            Some(Payload::ToolCancelled(tool)) => (
                &tool.tool_call_id,
                &tool.tool_name,
                ToolTerminalPresentation {
                    status: ToolTerminalStatus::Cancelled,
                    ..ToolTerminalPresentation::default()
                },
            ),
            Some(Payload::ToolError(tool)) => (
                &tool.tool_call_id,
                &tool.tool_name,
                ToolTerminalPresentation {
                    status: ToolTerminalStatus::Errored,
                    error: tool.message.clone(),
                    ..ToolTerminalPresentation::default()
                },
            ),
            _ => {
                return Err(super::PresenterError(
                    "The tool event is not terminal.".to_owned(),
                ));
            }
        };
        self.tool_live
            .get_mut()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(id);
        self.terminal_tools.insert(id.clone());
        let tool = self.tool_calls.remove(id).unwrap_or_default();
        self.activities
            .remove(&format!("{TOOL_ACTIVITY_PREFIX}{id}"));
        self.folded_tools.remove(id);
        let mut call = ToolCallPresentation::new(
            if tool.name.is_empty() {
                name
            } else {
                &tool.name
            },
            tool.arguments,
        );
        call.agent_reference_resolver = resolver;
        terminal.start_omitted = start_omitted;
        Ok((call, terminal))
    }

    pub fn offer_agent_task_progress(
        &mut self,
        snapshot: &wire::AgentTaskProgressSnapshot,
    ) -> bool {
        if snapshot.revision <= self.agent_task_revision {
            return false;
        }
        self.agent_task_revision = snapshot.revision;
        self.agent_task_progress = Some(snapshot.clone());
        true
    }

    pub fn is_current_agent_task_progress(&self, revision: u64) -> bool {
        self.agent_task_progress.is_some() && self.agent_task_revision == revision
    }

    pub fn retire_agent_task_progress(&mut self, revision: u64) -> bool {
        if !self.is_current_agent_task_progress(revision)
            || !self
                .agent_task_progress
                .as_ref()
                .is_some_and(is_terminal_graph)
        {
            return false;
        }
        self.agent_task_progress = None;
        true
    }

    pub(crate) fn create_live_item(
        &self,
        id: &str,
        frame: usize,
        presenters: &ToolPresenterRegistry,
        resolver: Arc<dyn Fn(&str) -> String + Send + Sync>,
    ) -> PresenterResult<ActivityItem> {
        if id == AGENT_ACTIVITY_ID {
            return Ok(ActivityItem::Value(if self.is_streaming_response() {
                ActivityValue::Streamed {
                    marker: "●".to_owned(),
                    text: self.response.clone(),
                }
            } else {
                ActivityValue::Spinner {
                    text: self.agent_spinner_text(),
                    frame,
                }
            }));
        }
        if id == COMPACTION_ACTIVITY_ID {
            return Ok(ActivityItem::Value(ActivityValue::Spinner {
                text: "Compacting…".to_owned(),
                frame,
            }));
        }
        let call = self.tool_call(
            id.strip_prefix(TOOL_ACTIVITY_PREFIX).unwrap_or(id),
            resolver,
        );
        let mut cache = self
            .tool_live
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let id = id.strip_prefix(TOOL_ACTIVITY_PREFIX).unwrap_or(id);
        if !cache.contains_key(id) {
            cache.insert(id.to_owned(), presenters.present_live(&call, frame)?);
        }
        let mut value = cache[id].clone();
        if let super::Presentation::Tool(report) = &mut value {
            report.frame = frame;
        }
        Ok(ActivityItem::Tool(value))
    }
}

fn is_terminal_graph(snapshot: &wire::AgentTaskProgressSnapshot) -> bool {
    !snapshot.root_nodes.is_empty() && snapshot.root_nodes.iter().all(is_terminal_node)
}
fn is_terminal_node(node: &wire::AgentTaskProgressNode) -> bool {
    matches!(
        node.status.enum_value(),
        Ok(
            wire::AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_SUCCEEDED
                | wire::AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_CANCELED
        )
    ) && node.children.iter().all(is_terminal_node)
}
fn format_token_count(count: i64) -> String {
    let (number, suffix) = if count >= 1_000_000 {
        (count as f64 / 1_000_000.0, "m")
    } else if count >= 1_000 {
        (count as f64 / 1_000.0, "k")
    } else {
        return count.to_string();
    };
    format!(
        "{}{}",
        format!("{number:.1}")
            .trim_end_matches('0')
            .trim_end_matches('.'),
        suffix
    )
}
