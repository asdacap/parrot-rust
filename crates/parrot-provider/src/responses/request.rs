use crate::image_inspection::image_reference;
use crate::view::{ContentView, MessageView, RequestView, ToolCallView, ToolView};
use crate::wire_json::{source_compact, source_quote};
use parrot_core::Role;
use serde_json::value::RawValue;
use std::io;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedResponsesRequest {
    body: String,
}
impl PreparedResponsesRequest {
    pub fn encode_http(&self) -> Vec<u8> {
        self.body.as_bytes().to_vec()
    }
    pub fn body(&self) -> &str {
        &self.body
    }
}

#[derive(Debug)]
pub enum ResponsesPrepareError {
    InvalidJson(serde_json::Error),
    PreferencesNotObject,
    ImageIo(io::Error),
}
impl std::fmt::Display for ResponsesPrepareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidJson(error) => error.fmt(f),
            Self::PreferencesNotObject => f.write_str("provider preferences must be a JSON object"),
            Self::ImageIo(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for ResponsesPrepareError {}

pub fn prepare(
    request: &impl RequestView,
    provider_id: &str,
) -> Result<PreparedResponsesRequest, ResponsesPrepareError> {
    let mut input = Vec::new();
    for message in request.messages() {
        add_message(message, provider_id, &mut input)?;
    }
    let tools = if request.tools().is_empty() {
        None
    } else {
        let mut encoded = Vec::with_capacity(request.tools().len());
        for tool in request.tools() {
            let raw_schema = if tool.parameters_json().is_empty() {
                "{}"
            } else {
                tool.parameters_json()
            };
            let schema: Box<RawValue> =
                serde_json::from_str(raw_schema).map_err(ResponsesPrepareError::InvalidJson)?;
            let mut fields = vec![("type", "\"function\"".to_owned())];
            fields.push(("name", quote(tool.name())));
            if !tool.description().is_empty() {
                fields.push(("description", quote(tool.description())));
            }
            fields.push((
                "parameters",
                source_compact(schema.get()).map_err(ResponsesPrepareError::InvalidJson)?,
            ));
            fields.push(("strict", "false".to_owned()));
            encoded.push(object(fields));
        }
        Some(format!("[{}]", encoded.join(",")))
    };

    let reasoning = {
        let effort = request.reasoning_effort().unwrap_or_default();
        let summary = request.reasoning_summary();
        (!effort.is_empty() || !summary.is_empty()).then(|| {
            let mut fields = Vec::new();
            if !effort.is_empty() {
                fields.push(("effort", quote(effort)));
            }
            if !summary.is_empty() {
                fields.push(("summary", quote(summary)));
            }
            object(fields)
        })
    };
    let provider_preferences = if request.provider_preferences().trim().is_empty() {
        None
    } else {
        let preferences: Box<RawValue> = serde_json::from_str(request.provider_preferences())
            .map_err(ResponsesPrepareError::InvalidJson)?;
        if !preferences.get().trim_start().starts_with('{') {
            return Err(ResponsesPrepareError::PreferencesNotObject);
        }
        Some(source_compact(preferences.get()).map_err(ResponsesPrepareError::InvalidJson)?)
    };

    let mut body = vec![("model", quote(request.model()))];
    if !request.service_tier().is_empty() {
        body.push(("service_tier", quote(request.service_tier())));
    }
    if !request.instructions().is_empty() {
        body.push(("instructions", quote(request.instructions())));
    }
    body.push(("input", format!("[{}]", input.join(","))));
    if let Some(tools) = tools {
        body.push(("tools", tools));
    }
    body.push(("stream", "true".to_owned()));
    body.push(("store", "false".to_owned()));
    body.push(("include", r#"["reasoning.encrypted_content"]"#.to_owned()));
    if let Some(reasoning) = reasoning {
        body.push(("reasoning", reasoning));
    }
    if let Some(provider) = provider_preferences {
        body.push(("provider", provider));
    }
    if request.max_output_tokens() > 0 {
        body.push(("max_output_tokens", request.max_output_tokens().to_string()));
    }
    Ok(PreparedResponsesRequest { body: object(body) })
}

fn add_message<M: MessageView>(
    message: &M,
    provider_id: &str,
    input: &mut Vec<String>,
) -> Result<(), ResponsesPrepareError> {
    if message.role() == Role::Assistant
        && message.provider_id() == provider_id
        && let Some(items) = replayable_output(message.provider_content())
    {
        input.extend(
            items
                .into_iter()
                .map(|item| source_compact(&item).expect("selected item was validated JSON")),
        );
        return Ok(());
    }
    if message.role() == Role::Tool {
        input.push(object(vec![
            ("type", quote("function_call_output")),
            ("call_id", quote(message.tool_call_id())),
            ("output", quote(message.text())),
        ]));
        return Ok(());
    }

    let role = match message.role() {
        Role::System => "developer",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => unreachable!(),
    };
    let mut content = Vec::new();
    for part in message.contents() {
        if !part.is_text() {
            let reference = image_reference(Some(part.image_url()), part.media_type(), || {
                part.read_image()
            })
            .map_err(ResponsesPrepareError::ImageIo)?;
            content.push(object(vec![
                ("type", quote("input_image")),
                ("image_url", quote(&reference)),
            ]));
        } else if !part.text().is_empty() {
            content.push(object(vec![
                (
                    "type",
                    quote(if message.role() == Role::Assistant {
                        "output_text"
                    } else {
                        "input_text"
                    }),
                ),
                ("text", quote(part.text())),
            ]));
        }
    }
    if !content.is_empty() || message.tool_calls().is_empty() {
        input.push(object(vec![
            ("type", quote("message")),
            ("role", quote(role)),
            ("content", format!("[{}]", content.join(","))),
        ]));
    }
    for call in message.tool_calls() {
        input.push(object(vec![
            ("type", quote("function_call")),
            ("call_id", quote(call.id())),
            ("name", quote(call.name())),
            ("arguments", quote(call.arguments_json())),
        ]));
    }
    Ok(())
}

fn replayable_output(raw: &str) -> Option<Vec<String>> {
    let root: Box<RawValue> = serde_json::from_str(raw).ok()?;
    let object: std::collections::HashMap<String, Box<RawValue>> =
        serde_json::from_str(root.get()).ok()?;
    let version: i32 = serde_json::from_str(object.get("Version")?.get()).ok()?;
    let protocol: String = serde_json::from_str(object.get("Protocol")?.get()).ok()?;
    if version != 1 || protocol != "responses" {
        return None;
    }
    let output: Vec<Box<RawValue>> = serde_json::from_str(object.get("Output")?.get()).ok()?;
    if output.is_empty() || output.iter().any(|item| !replayable_item(item.get())) {
        return None;
    }
    Some(
        output
            .into_iter()
            .map(|item| item.get().to_owned())
            .collect(),
    )
}

fn replayable_item(raw: &str) -> bool {
    let Ok(object) = serde_json::from_str::<std::collections::HashMap<String, Box<RawValue>>>(raw)
    else {
        return false;
    };
    let text = |name: &str| {
        object
            .get(name)
            .and_then(|raw| serde_json::from_str::<String>(raw.get()).ok())
            .unwrap_or_default()
    };
    match text("type").as_str() {
        "reasoning" => {
            let encrypted = object.get("encrypted_content").is_none_or(|raw| {
                raw.get() == "null" || serde_json::from_str::<String>(raw.get()).is_ok()
            });
            let summary = object
                .get("summary")
                .is_none_or(|raw| raw.get().trim_start().starts_with('['));
            encrypted && summary
        }
        "message" => {
            text("role") == "assistant"
                && object
                    .get("content")
                    .is_some_and(|raw| raw.get().trim_start().starts_with('['))
        }
        "function_call" => {
            !text("call_id").is_empty()
                && !text("name").is_empty()
                && object
                    .get("arguments")
                    .is_some_and(|raw| serde_json::from_str::<String>(raw.get()).is_ok())
        }
        _ => false,
    }
}

fn quote(text: &str) -> String {
    source_quote(text)
}
fn object(fields: Vec<(&str, String)>) -> String {
    let fields = fields
        .into_iter()
        .map(|(key, value)| format!("{}:{value}", quote(key)))
        .collect::<Vec<_>>()
        .join(",");
    format!("{{{fields}}}")
}
