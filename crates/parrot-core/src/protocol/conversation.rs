use super::message::Message;

/// Captures durable history facts and the independent current provider-visible view.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConversationState {
    pub summary: Option<ConversationSummary>,
    /// Contains the chronological durable tail after the captured summary watermark.
    pub history: Vec<ConversationHistoryItem>,
    pub checkpoint_sequences: Vec<i64>,
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
