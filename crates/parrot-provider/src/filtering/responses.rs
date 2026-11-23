use parrot_core::{Message, ReasoningFilter, ReasoningKind};
use serde_json::value::RawValue;
use std::collections::HashMap;

/// Responses API reasoning replay: `reasoning` output items are strippable
/// unless they carry `encrypted_content`, which protects the whole batch.
pub(crate) struct ResponsesReasoningFilter;

enum ResponsesEdit {
    Blocked,
    Rewritten(String),
}

enum OutputItem {
    EncryptedReasoning,
    PlainReasoning,
    Unrecognized,
}

fn output_item(raw: &str) -> OutputItem {
    let Ok(fields) = serde_json::from_str::<HashMap<String, Box<RawValue>>>(raw) else {
        return OutputItem::Unrecognized;
    };
    let Ok(kind) =
        serde_json::from_str::<String>(fields.get("type").map(|raw| raw.get()).unwrap_or_default())
    else {
        return OutputItem::Unrecognized;
    };
    if kind != "reasoning" {
        return OutputItem::Unrecognized;
    }
    let encrypted = fields
        .get("encrypted_content")
        .is_some_and(|raw| raw.get() != "null");
    if encrypted {
        OutputItem::EncryptedReasoning
    } else {
        OutputItem::PlainReasoning
    }
}

/// Returns the envelope without its plain reasoning items, `Blocked` for
/// encrypted reasoning, or `None` when this message carries nothing to strip.
fn responses_edit(raw: &str) -> Option<ResponsesEdit> {
    let fields: HashMap<String, Box<RawValue>> = serde_json::from_str(raw).ok()?;
    let version: i32 = serde_json::from_str(fields.get("Version")?.get()).ok()?;
    if version != 1 {
        return None;
    }
    let protocol: String = serde_json::from_str(fields.get("Protocol")?.get()).ok()?;
    if protocol != "responses" {
        return None;
    }
    let output: Vec<Box<RawValue>> = serde_json::from_str(fields.get("Output")?.get()).ok()?;
    let mut kept = Vec::with_capacity(output.len());
    let mut stripped = false;
    for item in output {
        match output_item(item.get()) {
            OutputItem::EncryptedReasoning => return Some(ResponsesEdit::Blocked),
            OutputItem::PlainReasoning => stripped = true,
            OutputItem::Unrecognized => kept.push(item),
        }
    }
    if !stripped {
        return None;
    }
    Some(ResponsesEdit::Rewritten(format!(
        "{{\"Version\":1,\"Protocol\":\"responses\",\"Output\":[{}]}}",
        kept.iter()
            .map(|item| item.get())
            .collect::<Vec<_>>()
            .join(",")
    )))
}

impl ReasoningFilter for ResponsesReasoningFilter {
    fn kind(&self, message: &Message) -> ReasoningKind {
        match responses_edit(&message.provider_content) {
            Some(ResponsesEdit::Blocked) => ReasoningKind::Protected,
            Some(ResponsesEdit::Rewritten(next)) if next != message.provider_content => {
                ReasoningKind::Plain
            }
            _ => ReasoningKind::Absent,
        }
    }

    fn strip(&self, message: &Message) -> Option<Message> {
        match responses_edit(&message.provider_content) {
            Some(ResponsesEdit::Rewritten(next)) if next != message.provider_content => {
                Some(Message {
                    provider_content: next,
                    ..message.clone()
                })
            }
            _ => None,
        }
    }
}
