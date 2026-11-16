use super::{CompletionBlockerFeedback, FinishReason, Message};
use std::collections::BTreeMap;

/// Captures the persisted summary and history used to assemble provider messages.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConversationState {
    pub summary: Option<ConversationSummary>,
    /// Persisted messages after the summary watermark, in chronological order.
    /// Includes sequence and tool ownership metadata for compaction grouping.
    pub history: Vec<ConversationHistoryItem>,
}

impl ConversationState {
    /// Assembles the summary, its associated status, and retained provider messages.
    pub fn assemble_messages(&self) -> Vec<Message> {
        let mut messages = Vec::new();
        if let Some(summary) = &self.summary {
            messages.push(summary.message.clone());
            if let Some(sequence) = summary.status_sequence
                && let Some(status) = self.history.iter().find(|item| item.sequence == sequence)
            {
                messages.push(status.message.clone());
            }
        }
        messages.extend(
            self.history
                .iter()
                .filter(|item| self.summary.is_none() || !item.is_compaction_status)
                .map(|item| item.message.clone()),
        );
        messages
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationSummary {
    pub message: Message,
    pub watermark: i64,
    /// Identifies this summary's status message in the history tail, if present.
    pub status_sequence: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationHistoryItem {
    pub sequence: i64,
    pub tool_owner_sequence: Option<i64>,
    pub is_compaction_status: bool,
    pub message: Message,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversationOrigin {
    UserInput,
    Model,
    System,
    Tool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum ConversationEvent {
    #[default]
    Message,
    Notice {
        kind: String,
        notice: String,
    },
    InputPromoted {
        input_id: String,
        message_id: String,
    },
    Retry {
        reason: String,
        attempt: i32,
    },
    ToolRequestReceived {
        count: i32,
    },
    SkillLoaded {
        path: String,
    },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConversationPublication {
    pub event_id: String,
    pub agent_session_id: String,
    pub event: ConversationEvent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConversationPersistence {
    Append {
        origin: ConversationOrigin,
        event: ConversationEvent,
    },
    Committed(ConversationPublication),
    CompletionBlockerFeedback(CompletionBlockerFeedback),
    ToolResult {
        assistant_sequence: i64,
        result: crate::ToolResult,
        recovered: bool,
    },
    CloseTurn {
        finish_reason: FinishReason,
        usage: UsageTotals,
    },
}

/// The request token totals a close-turn terminal event reports, as recorded by
/// the states side for the current turn.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UsageTotals {
    pub input_tokens: i64,
    pub output_tokens: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationAddition {
    pub messages: Vec<Message>,
    pub persistence: ConversationPersistence,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ConversationReceipt {
    pub inserted: bool,
    pub sequence: i64,
}

/// The facts that decide causal grouping for one durable history row. Compaction and
/// storage forks both reduce their row types to this view before grouping.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryGroupMember {
    pub sequence: i64,
    pub tool_owner_sequence: Option<i64>,
    pub is_compaction_status: bool,
    pub is_assistant: bool,
    pub tool_call_ids: Vec<String>,
    pub tool_result_call_id: Option<String>,
}

/// One causal group: an owner row plus the tool results answering its calls.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryGroup {
    pub owner_sequence: i64,
    pub member_sequences: Vec<i64>,
    pub is_complete: bool,
}

impl HistoryGroupMember {
    /// A causal group starts here only when an assistant issues tool calls.
    pub fn makes_tool_calls(&self) -> bool {
        self.is_assistant && !self.tool_call_ids.is_empty()
    }
}

/// Groups rows that own history rows with their tool-result children, sorted by
/// sequence, and checks every tool call has a tool-role result inside its group.
/// Children whose owner row is absent are dropped. Compaction-status rows never group.
pub fn assemble_history_groups(members: &[HistoryGroupMember]) -> Vec<HistoryGroup> {
    let mut children: BTreeMap<i64, Vec<&HistoryGroupMember>> = BTreeMap::new();
    for member in members {
        if member.is_compaction_status {
            continue;
        }
        if let Some(owner) = member.tool_owner_sequence {
            children.entry(owner).or_default().push(member);
        }
    }
    let mut groups = Vec::new();
    for member in members {
        if member.is_compaction_status || member.tool_owner_sequence.is_some() {
            continue;
        }
        let mut group_members = vec![member];
        group_members.extend(children.remove(&member.sequence).into_iter().flatten());
        group_members.sort_by_key(|member| member.sequence);
        let owns_calls = member.makes_tool_calls();
        let is_complete = !owns_calls
            || member.tool_call_ids.iter().all(|call_id| {
                group_members.iter().any(|group_member| {
                    group_member.tool_result_call_id.as_deref() == Some(call_id.as_str())
                })
            });
        groups.push(HistoryGroup {
            owner_sequence: member.sequence,
            member_sequences: group_members
                .iter()
                .map(|group_member| group_member.sequence)
                .collect(),
            is_complete,
        });
    }
    groups
}
