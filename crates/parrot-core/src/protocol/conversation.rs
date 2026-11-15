use super::{CompletionBlockerFeedback, FinishReason, Message};

/// Captures durable history facts and the independent current provider-visible view.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConversationState {
    pub summary: Option<ConversationSummary>,
    /// Contains the chronological durable tail after the captured summary watermark.
    pub history: Vec<ConversationHistoryItem>,
    pub messages: Vec<Message>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationSummary {
    pub content: String,
    pub watermark: i64,
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
        start_event_id: String,
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
