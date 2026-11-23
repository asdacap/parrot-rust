use parrot_core::{BoxFuture, CancellationToken, Error, FilterProvider, FilterRequest, Message};
use serde_json::value::RawValue;
use std::collections::HashMap;

/// Strips reasoning payloads from replayable provider content without any
/// model request. Responses envelopes lose their `reasoning` output items,
/// Anthropic replay blocks lose their `thinking` blocks, and chat-completions
/// envelopes lose their `ReasoningContent` and `Reasoning` fields; every other
/// payload passes through unchanged. A batch that carries any encrypted
/// reasoning, meaning a Responses reasoning item with `encrypted_content` or an
/// Anthropic `redacted_thinking` block, is a whole-batch no-op because the
/// provider must replay it intact.
pub struct ReasoningSkipFilter;

impl FilterProvider for ReasoningSkipFilter {
    fn filter<'a, 'c>(
        &'a self,
        request: FilterRequest,
        _cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<Option<Vec<Message>>, Error>>
    where
        'c: 'a,
    {
        Box::pin(async move { Ok(filter_batch(request.messages)) })
    }
}

enum ReplayEdit {
    /// The batch carries encrypted reasoning and must not be filtered.
    Blocked,
    /// The payload changed and the supplied text replaces it.
    Rewritten(String),
}

/// Returns the replacement batch, or `None` when every message is unchanged,
/// including a batch blocked by encrypted reasoning.
pub(super) fn filter_batch(messages: Vec<Message>) -> Option<Vec<Message>> {
    let mut changed = false;
    let mut replacements = Vec::with_capacity(messages.len());
    for message in messages {
        match strip_reasoning(&message) {
            Some(ReplayEdit::Blocked) => return None,
            Some(ReplayEdit::Rewritten(provider_content)) => {
                let mut replacement = message;
                replacement.provider_content = provider_content;
                changed = true;
                replacements.push(replacement);
            }
            None => replacements.push(message),
        }
    }
    changed.then_some(replacements)
}

/// Returns the edited provider content, `Blocked` for encrypted reasoning, or
/// `None` when this message carries nothing to strip.
fn strip_reasoning(message: &Message) -> Option<ReplayEdit> {
    let raw = message.provider_content.as_str();
    if raw.trim_start().starts_with('[') {
        anthropic_replay(raw)
    } else {
        envelope_replay(raw)
    }
}

fn envelope_replay(raw: &str) -> Option<ReplayEdit> {
    let fields: HashMap<String, Box<RawValue>> = serde_json::from_str(raw).ok()?;
    let version: i32 = serde_json::from_str(fields.get("Version")?.get()).ok()?;
    if version != 1 {
        return None;
    }
    let protocol: String = serde_json::from_str(fields.get("Protocol")?.get()).ok()?;
    match protocol.as_str() {
        "responses" => responses_replay(fields),
        "chat-completions" => chat_completions_replay(fields),
        _ => None,
    }
}

fn responses_replay(mut fields: HashMap<String, Box<RawValue>>) -> Option<ReplayEdit> {
    let output: Vec<Box<RawValue>> = serde_json::from_str(fields.remove("Output")?.get()).ok()?;
    let mut kept = Vec::with_capacity(output.len());
    for item in output {
        match responses_item(item.get()) {
            ResponsesItem::EncryptedReasoning => return Some(ReplayEdit::Blocked),
            ResponsesItem::PlainReasoning => {}
            ResponsesItem::Unrecognized => kept.push(item),
        }
    }
    Some(ReplayEdit::Rewritten(format!(
        "{{\"Version\":1,\"Protocol\":\"responses\",\"Output\":[{}]}}",
        kept.iter()
            .map(|item| item.get())
            .collect::<Vec<_>>()
            .join(",")
    )))
}

/// Drops the reasoning fields from a chat-completions replay envelope and
/// returns `None` when neither field carries reasoning text. Every other
/// field, including `ToolCalls`, is preserved verbatim.
fn chat_completions_replay(mut fields: HashMap<String, Box<RawValue>>) -> Option<ReplayEdit> {
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
    Some(ReplayEdit::Rewritten(format!(
        "{{\"Version\":1,\"Protocol\":\"chat-completions\",{body}}}"
    )))
}

enum ResponsesItem {
    EncryptedReasoning,
    PlainReasoning,
    Unrecognized,
}

fn responses_item(raw: &str) -> ResponsesItem {
    let Ok(fields) = serde_json::from_str::<HashMap<String, Box<RawValue>>>(raw) else {
        return ResponsesItem::Unrecognized;
    };
    let Ok(kind) =
        serde_json::from_str::<String>(fields.get("type").map(|raw| raw.get()).unwrap_or_default())
    else {
        return ResponsesItem::Unrecognized;
    };
    if kind != "reasoning" {
        return ResponsesItem::Unrecognized;
    }
    let encrypted = fields
        .get("encrypted_content")
        .is_some_and(|raw| raw.get() != "null");
    if encrypted {
        ResponsesItem::EncryptedReasoning
    } else {
        ResponsesItem::PlainReasoning
    }
}

fn anthropic_replay(raw: &str) -> Option<ReplayEdit> {
    let blocks: Vec<Box<RawValue>> = serde_json::from_str(raw).ok()?;
    let mut kinds = Vec::with_capacity(blocks.len());
    for block in &blocks {
        let fields: HashMap<String, Box<RawValue>> = serde_json::from_str(block.get()).ok()?;
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
            "redacted_thinking" => return Some(ReplayEdit::Blocked),
            "thinking" => {}
            _ => kept.push(block.get().to_owned()),
        }
    }
    Some(ReplayEdit::Rewritten(format!("[{}]", kept.join(","))))
}
