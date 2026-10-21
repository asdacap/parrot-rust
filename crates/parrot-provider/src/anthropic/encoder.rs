use super::view::{
    ContentKind, ContentView, MessageView, RequestView, Role, ToolCallView, ToolDefinitionView,
};
use super::wire_json::{normalize_raw_json, quote};
use serde_json::value::RawValue;
use std::{fmt, io};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedRequest {
    pub body: Vec<u8>,
}

#[derive(Debug)]
pub enum EncodeError {
    Json(String),
    Serialization(String),
    Image(io::Error),
}

impl fmt::Display for EncodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(error) | Self::Serialization(error) => formatter.write_str(error),
            Self::Image(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for EncodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(_) | Self::Serialization(_) => None,
            Self::Image(error) => Some(error),
        }
    }
}

/// Convert complete history to Anthropic's Messages body, without serializing a
/// provider-neutral request or eagerly reading caller-owned images.
pub fn encode(
    request: &impl RequestView,
    provider_id: &str,
) -> Result<PreparedRequest, EncodeError> {
    let mut system = Vec::new();
    if !request.instructions().is_empty() {
        system.push(text_block(request.instructions()));
    }
    let mut messages: Vec<(&str, Vec<String>)> = Vec::new();
    for message in request.messages() {
        if message.role() == Role::System && messages.is_empty() {
            let text = message_text(message);
            if !text.is_empty() {
                system.push(text_block(&text));
            }
            continue;
        }
        let role = if message.role() == Role::Assistant {
            "assistant"
        } else {
            "user"
        };
        let replay = if message.role() == Role::Assistant
            && message.provider_id() == provider_id
            && !message.provider_content().is_empty()
        {
            super::replay::blocks(message.provider_content()).map_err(EncodeError::Serialization)?
        } else {
            None
        };
        let blocks = match replay {
            Some(blocks) => blocks,
            None => ordinary_blocks(message)?,
        };
        if blocks.is_empty() {
            continue;
        }
        match messages.last_mut() {
            Some((previous_role, previous_blocks)) if *previous_role == role => {
                previous_blocks.extend(blocks)
            }
            _ => messages.push((role, blocks)),
        }
    }
    if let Some(last) = system.last_mut() {
        last.pop();
        last.push_str(",\"cache_control\":{\"type\":\"ephemeral\"}}");
    }

    let effort = request.reasoning_effort().unwrap_or_default();
    let replayed_thinking = messages.iter().flat_map(|(_, blocks)| blocks).any(|block| {
        block.starts_with("{\"type\":\"thinking\"")
            || block.starts_with("{\"type\":\"redacted_thinking\"")
    });
    // Summary, preferences, router metadata and image dimensions are deliberately
    // unused by the source Anthropic adapter, but remain available on the view.
    let mut body = format!(
        "{{\"model\":{},\"max_tokens\":{}",
        quote(request.model()),
        request.max_output_tokens()
    );
    if !system.is_empty() {
        body.push_str(&format!(",\"system\":[{}]", system.join(",")));
    }
    let messages = messages
        .into_iter()
        .map(|(role, blocks)| {
            format!(
                "{{\"role\":{},\"content\":[{}]}}",
                quote(role),
                blocks.join(",")
            )
        })
        .collect::<Vec<_>>();
    body.push_str(&format!(",\"messages\":[{}]", messages.join(",")));
    if !request.tools().is_empty() {
        let mut tools = Vec::new();
        for tool in request.tools() {
            let mut output = format!("{{\"name\":{}", quote(tool.name()));
            if !tool.description().is_empty() {
                output.push_str(&format!(",\"description\":{}", quote(tool.description())));
            }
            let schema = if tool.parameters_json().is_empty() {
                "{}"
            } else {
                tool.parameters_json()
            };
            let schema = normalize_raw_json(schema).map_err(EncodeError::Json)?;
            output.push_str(&format!(",\"input_schema\":{schema}}}"));
            tools.push(output);
        }
        body.push_str(&format!(",\"tools\":[{}]", tools.join(",")));
    }
    body.push_str(",\"stream\":true");
    if !effort.is_empty() || replayed_thinking {
        body.push_str(",\"thinking\":{\"type\":\"adaptive\",\"display\":\"summarized\",\"block_binding\":{\"prefix_mismatch_behavior\":\"drop_block\"}}");
    }
    if !effort.is_empty() {
        body.push_str(&format!(
            ",\"output_config\":{{\"effort\":{}}}",
            quote(effort)
        ));
    }
    body.push_str(",\"cache_control\":{\"type\":\"ephemeral\"}}");
    Ok(PreparedRequest {
        body: body.into_bytes(),
    })
}

fn text_block(text: &str) -> String {
    format!("{{\"type\":\"text\",\"text\":{}}}", quote(text))
}

fn message_text(message: &impl MessageView) -> String {
    message
        .contents()
        .iter()
        .filter(|part| part.kind() == ContentKind::Text)
        .map(ContentView::text)
        .collect()
}

fn ordinary_blocks(message: &impl MessageView) -> Result<Vec<String>, EncodeError> {
    if message.role() == Role::System {
        let escaped = message_text(message)
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;");
        return Ok(vec![text_block(&format!(
            "<system-update>\n{escaped}\n</system-update>"
        ))]);
    }
    let mut blocks = Vec::new();
    for part in message.contents() {
        if part.kind() == ContentKind::Image {
            let source = if !part.image_url().is_empty() {
                format!("{{\"type\":\"url\",\"url\":{}}}", quote(part.image_url()))
            } else {
                let bytes = part.read_image().map_err(EncodeError::Image)?;
                format!(
                    "{{\"type\":\"base64\",\"media_type\":{},\"data\":{}}}",
                    quote(part.media_type()),
                    quote(&base64(&bytes))
                )
            };
            blocks.push(format!("{{\"type\":\"image\",\"source\":{source}}}"));
        } else if !part.text().is_empty() {
            blocks.push(text_block(part.text()));
        }
    }
    if message.role() == Role::Tool {
        let mut block = format!(
            "{{\"type\":\"tool_result\",\"tool_use_id\":{}",
            quote(message.tool_call_id())
        );
        if !blocks.is_empty() {
            block.push_str(&format!(",\"content\":[{}]", blocks.join(",")));
        }
        block.push('}');
        return Ok(vec![block]);
    }
    for call in message.tool_calls() {
        let input = tool_input(call.arguments_json()).map_err(EncodeError::Serialization)?;
        blocks.push(format!(
            "{{\"type\":\"tool_use\",\"id\":{},\"name\":{},\"input\":{input}}}",
            quote(call.id()),
            quote(call.name())
        ));
    }
    Ok(blocks)
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let bits = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        output.push(char::from(ALPHABET[((bits >> 18) & 63) as usize]));
        output.push(char::from(ALPHABET[((bits >> 12) & 63) as usize]));
        output.push(if chunk.len() > 1 {
            char::from(ALPHABET[((bits >> 6) & 63) as usize])
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            char::from(ALPHABET[(bits & 63) as usize])
        } else {
            '='
        });
    }
    output
}

/// Only parsing/non-object input falls back. A valid object may still fail when
/// its opaque JsonElement strings are serialized.
pub(crate) fn tool_input(raw: &str) -> Result<String, String> {
    let Ok(value) = serde_json::from_str::<Box<RawValue>>(raw) else {
        return Ok("{}".into());
    };
    if !value.get().starts_with('{') {
        return Ok("{}".into());
    }
    normalize_raw_json(value.get())
}
