use parrot_core::{Message, ReasoningFilter, ReasoningKind};
use serde_json::value::RawValue;

/// Anthropic Messages reasoning replay: `thinking` blocks are strippable and
/// `redacted_thinking` blocks are protected.
pub(crate) struct AnthropicReasoningFilter;

enum AnthropicEdit {
    Blocked,
    Rewritten(String),
}

/// Returns the content blocks without their `thinking` blocks, `Blocked` for
/// encrypted reasoning, or `None` when this message carries nothing to strip.
fn anthropic_edit(raw: &str) -> Option<AnthropicEdit> {
    let blocks: Vec<Box<RawValue>> = serde_json::from_str(raw).ok()?;
    let mut kinds = Vec::with_capacity(blocks.len());
    for block in &blocks {
        let fields: std::collections::HashMap<String, Box<RawValue>> =
            serde_json::from_str(block.get()).ok()?;
        let kind = serde_json::from_str::<String>(
            fields.get("type").map(|raw| raw.get()).unwrap_or_default(),
        )
        .ok()?;
        kinds.push(kind);
    }
    if !kinds
        .iter()
        .any(|kind| kind == "thinking" || kind == "redacted_thinking")
    {
        return None;
    }
    let mut kept = Vec::with_capacity(blocks.len());
    for (block, kind) in blocks.iter().zip(&kinds) {
        match kind.as_str() {
            "redacted_thinking" => return Some(AnthropicEdit::Blocked),
            "thinking" => {}
            _ => kept.push(block.get().to_owned()),
        }
    }
    Some(AnthropicEdit::Rewritten(format!("[{}]", kept.join(","))))
}

impl ReasoningFilter for AnthropicReasoningFilter {
    fn kind(&self, message: &Message) -> ReasoningKind {
        match anthropic_edit(&message.provider_content) {
            Some(AnthropicEdit::Blocked) => ReasoningKind::Protected,
            Some(AnthropicEdit::Rewritten(next)) if next != message.provider_content => {
                ReasoningKind::Plain
            }
            _ => ReasoningKind::Absent,
        }
    }

    fn strip(&self, message: &Message) -> Option<Message> {
        match anthropic_edit(&message.provider_content) {
            Some(AnthropicEdit::Rewritten(next)) if next != message.provider_content => {
                Some(Message {
                    provider_content: next,
                    ..message.clone()
                })
            }
            _ => None,
        }
    }
}
