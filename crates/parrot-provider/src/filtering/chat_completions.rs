use parrot_core::{Message, ReasoningFilter, ReasoningKind};
use serde_json::value::RawValue;
use std::collections::HashMap;

/// Chat-completions reasoning replay: `ReasoningContent` and `Reasoning`
/// fields are strippable, every other field is preserved verbatim.
pub(crate) struct ChatCompletionsReasoningFilter;

enum ChatEdit {
    Rewritten(String),
}

/// Returns the envelope without its reasoning fields, or `None` when neither
/// field carries reasoning text or the envelope is not a chat-completions
/// replay.
fn chat_edit(raw: &str) -> Option<ChatEdit> {
    let mut fields: HashMap<String, Box<RawValue>> = serde_json::from_str(raw).ok()?;
    let version: i32 = serde_json::from_str(fields.get("Version")?.get()).ok()?;
    if version != 1 {
        return None;
    }
    let protocol: String = serde_json::from_str(fields.get("Protocol")?.get()).ok()?;
    if protocol != "chat-completions" {
        return None;
    }
    let mut stripped = false;
    let mut kept = Vec::with_capacity(fields.len());
    for (name, value) in fields.drain() {
        match name.as_str() {
            "ReasoningContent" | "Reasoning" if value.get() != "null" => stripped = true,
            "Version" | "Protocol" => {}
            _ => kept.push((name, value.get().to_owned())),
        }
    }
    if !stripped {
        return None;
    }
    kept.sort_by(|left, right| left.0.cmp(&right.0));
    let body = kept
        .iter()
        .map(|(name, value)| format!("{name:?}:{value}"))
        .collect::<Vec<_>>()
        .join(",");
    let separator = if body.is_empty() { "" } else { "," };
    Some(ChatEdit::Rewritten(format!(
        "{{\"Version\":1,\"Protocol\":\"chat-completions\"{separator}{body}}}"
    )))
}

impl ReasoningFilter for ChatCompletionsReasoningFilter {
    fn kind(&self, message: &Message) -> ReasoningKind {
        match chat_edit(&message.provider_content) {
            Some(ChatEdit::Rewritten(next)) if next != message.provider_content => {
                ReasoningKind::Plain
            }
            _ => ReasoningKind::Absent,
        }
    }

    fn strip(&self, message: &Message) -> Option<Message> {
        match chat_edit(&message.provider_content) {
            Some(ChatEdit::Rewritten(next)) if next != message.provider_content => Some(Message {
                provider_content: next,
                ..message.clone()
            }),
            _ => None,
        }
    }
}
