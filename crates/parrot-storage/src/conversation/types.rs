//! Source-local history carriers; persisted events remain generated protobufs.
pub use crate::events::{
    ConversationItem, ConversationMessage, ConversationOrigin, ConversationPart, MessageRole,
    ProviderContent, ToolCall,
};
use std::path::PathBuf;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryCheckpoint {
    pub title: String,
    pub assistant_sequence: i64,
    pub tool_call_id: String,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HistoryForkBoundary {
    BeforeToolBatch {
        assistant_sequence: i64,
        tool_call_id: String,
    },
    AfterCompletedHistory,
    AfterSafeHistoryPrefix,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryForkKind {
    Empty,
    Full,
    Named,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryForkSelection {
    pub kind: HistoryForkKind,
    pub title: String,
}
impl HistoryForkSelection {
    pub fn parse(value: &str) -> Self {
        Self {
            kind: match value {
                "" | "empty" => HistoryForkKind::Empty,
                "full" => HistoryForkKind::Full,
                _ => HistoryForkKind::Named,
            },
            title: if matches!(value, "" | "empty" | "full") {
                String::new()
            } else {
                value.into()
            },
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryForkSource {
    pub agent_session_id: String,
    pub boundary: HistoryForkBoundary,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionSnapshot {
    pub summary: String,
    pub watermark: i64,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionContext {
    pub snapshot: CompactionSnapshot,
    pub status: Option<ConversationItem>,
    pub tail: Vec<ConversationItem>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationGroup {
    pub items: Vec<ConversationItem>,
    pub start_watermark: i64,
    pub end_watermark: i64,
    pub assistant_sequence: i64,
    pub is_complete: bool,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectiveConversationHistory {
    pub snapshot: Option<CompactionSnapshot>,
    pub status: Option<ConversationItem>,
    pub groups: Vec<ConversationGroup>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingStatus {
    pub mode: String,
    pub version: i64,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextReminderCheckpoint {
    pub canonical_model: String,
    pub context_limit: i32,
    pub percentage: i32,
}
#[derive(Clone, Debug, PartialEq)]
pub enum AgentHistoryEntry {
    Message {
        sequence: i64,
        conversation_sequence: i64,
        origin: String,
        role: String,
        parts: Vec<ConversationPart>,
        tool_calls: Vec<ToolCall>,
        tool_call_id: String,
    },
    Compaction {
        sequence: i64,
        summary: String,
        watermark: i64,
    },
    Request {
        sequence: i64,
        event_sequence: i64,
        request_id: String,
        provider: String,
        model: String,
        effort: Option<String>,
        input_tokens: i64,
        cached_input_tokens: i64,
        output_tokens: i64,
        input_cost: f64,
        output_cost: f64,
        tool_calls: i64,
    },
}
impl AgentHistoryEntry {
    pub fn sequence(&self) -> i64 {
        match self {
            Self::Message { sequence, .. }
            | Self::Compaction { sequence, .. }
            | Self::Request { sequence, .. } => *sequence,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelContent {
    Text(String),
    ImageFile {
        path: PathBuf,
        media_type: String,
        width: i32,
        height: i32,
    },
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelMessage {
    pub role: MessageRole,
    pub contents: Vec<ModelContent>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: String,
    pub provider_content: ProviderContent,
}
impl ModelMessage {
    pub fn content(&self) -> String {
        self.contents
            .iter()
            .filter_map(|c| match c {
                ModelContent::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }
}
