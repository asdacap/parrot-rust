use std::collections::BTreeMap;
use std::fmt;

use serde_json::{Value, value::RawValue};

use super::encoder::tool_input;
use super::raw_object::RawObject;
use super::wire_json::{selected_json_string, serialize};

type Object<'a> = RawObject<'a>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnthropicEvent {
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
        part_id: String,
        completed: bool,
    },
    ToolCallDelta(ToolCall),
    Completed(CompletedMessage),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments_json: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletedMessage {
    pub finish_reason: String,
    pub input_tokens: i32,
    pub cached_input_tokens: i32,
    pub output_tokens: i32,
    pub assistant_text: String,
    pub tool_calls: Vec<ToolCall>,
    pub provider_content: String,
    pub provider_id: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ParserError {
    Json(String),
    InvalidJsonString(String),
    Serialization(String),
    ProviderResponse {
        error_type: String,
        error_code: String,
        detail: String,
        response_body: String,
    },
    Wire(String),
}

impl fmt::Display for ParserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(message)
            | Self::InvalidJsonString(message)
            | Self::Serialization(message)
            | Self::Wire(message) => f.write_str(message),
            Self::ProviderResponse {
                error_type,
                error_code,
                detail,
                ..
            } => {
                f.write_str(if detail.is_empty() {
                    "provider error"
                } else {
                    detail
                })?;
                if !error_type.is_empty() {
                    write!(f, " ({error_type})")?;
                }
                if !error_code.is_empty() {
                    write!(f, " [{error_code}]")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ParserError {}

#[derive(Default)]
struct Block {
    kind: String,
    id: String,
    name: String,
    initial_input: String,
    data: String,
    text: String,
    signature: String,
}

impl Block {
    fn arguments(&self) -> &str {
        if self.text.is_empty() {
            &self.initial_input
        } else {
            &self.text
        }
    }

    fn tool_call(&self) -> ToolCall {
        ToolCall {
            id: self.id.clone(),
            name: self.name.clone(),
            arguments_json: self.arguments().to_owned(),
        }
    }

    fn content(&self) -> Result<Option<String>, ParserError> {
        let quote = |text: &str| serialize(&Value::String(text.to_owned()));
        Ok(match self.kind.as_str() {
            "text" if !self.text.is_empty() => Some(format!(
                "{{\"type\":\"text\",\"text\":{}}}",
                quote(&self.text)
            )),
            "thinking" => Some(format!(
                "{{\"type\":\"thinking\",\"thinking\":{},\"signature\":{}}}",
                quote(&self.text),
                quote(&self.signature)
            )),
            "redacted_thinking" => Some(format!(
                "{{\"type\":\"redacted_thinking\",\"data\":{}}}",
                quote(&self.data)
            )),
            "tool_use" => {
                let normalized =
                    tool_input(self.arguments()).map_err(ParserError::Serialization)?;
                Some(format!(
                    "{{\"type\":\"tool_use\",\"id\":{},\"name\":{},\"input\":{normalized}}}",
                    quote(&self.id),
                    quote(&self.name)
                ))
            }
            _ => None,
        })
    }
}

pub struct AnthropicParser {
    provider_id: String,
    message_id: String,
    stopped: bool,
    finished: bool,
    finishing_tools: Option<std::vec::IntoIter<ToolCall>>,
    finish_reason: String,
    input_tokens: i32,
    cache_read_input_tokens: i32,
    cache_creation_input_tokens: i32,
    output_tokens: i32,
    assistant_text: String,
    blocks: BTreeMap<i32, Block>,
}

impl AnthropicParser {
    pub fn new(provider_id: impl Into<String>) -> Self {
        Self {
            provider_id: provider_id.into(),
            message_id: String::new(),
            stopped: false,
            finished: false,
            finishing_tools: None,
            finish_reason: "incomplete".to_owned(),
            input_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            output_tokens: 0,
            assistant_text: String::new(),
            blocks: BTreeMap::new(),
        }
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    pub fn consume(&mut self, data: &str) -> Result<Vec<AnthropicEvent>, ParserError> {
        if self.stopped {
            return Ok(Vec::new());
        }
        let raw: Box<RawValue> = serde_json::from_str(data).map_err(|error| {
            ParserError::Json(super::wire_json::format_json_error(data, &error))
        })?;
        let Some(root) = RawObject::new(&raw) else {
            return Ok(Vec::new());
        };
        let mut events = Vec::new();
        match string(&root, "type")?.as_str() {
            "message_start" => {
                if let Some(message) = child_object(&root, "message")? {
                    self.message_id = string(&message, "id")?;
                    self.read_usage(&message)?;
                }
            }
            "content_block_start" => {
                if let Some(block) = child_object(&root, "content_block")? {
                    self.start_block(integer(&root, "index")?, &block, &mut events)?;
                }
            }
            "content_block_delta" => {
                if let Some(delta) = child_object(&root, "delta")? {
                    self.continue_block(integer(&root, "index")?, &delta, &mut events)?;
                }
            }
            "content_block_stop" => {
                let index = integer(&root, "index")?;
                if self
                    .blocks
                    .get(&index)
                    .is_some_and(|block| block.kind == "thinking")
                {
                    events.push(AnthropicEvent::ReasoningDelta {
                        text: String::new(),
                        part_id: self.part_id(index),
                        completed: true,
                    });
                }
            }
            "message_delta" => {
                if let Some(delta) = child_object(&root, "delta")? {
                    if let Some(reason) = read_string(&delta, "stop_reason")? {
                        self.finish_reason = match reason.as_str() {
                            "end_turn" | "stop_sequence" => "stop",
                            "tool_use" => "tool_calls",
                            "max_tokens" | "model_context_window_exceeded" => "length",
                            "refusal" => "content_filter",
                            _ => "incomplete",
                        }
                        .to_owned();
                    }
                }
                self.read_usage(&root)?;
            }
            "message_stop" => self.stopped = true,
            "error" => {
                let nested = child_object(&root, "error")?;
                let error = nested.as_ref().unwrap_or(&root);
                return Err(ParserError::ProviderResponse {
                    error_type: string(error, "type")?,
                    error_code: String::new(),
                    detail: string(error, "message")?,
                    response_body: bound_response_body(data),
                });
            }
            _ => {}
        }
        Ok(events)
    }

    /// Collect successful completion events. Streaming callers should use
    /// `finish_next` so tool deltas remain published if content serialization fails.
    pub fn finish(&mut self) -> Result<Vec<AnthropicEvent>, ParserError> {
        let mut events = Vec::new();
        while let Some(event) = self.finish_next()? {
            events.push(event);
        }
        Ok(events)
    }

    pub fn finish_next(&mut self) -> Result<Option<AnthropicEvent>, ParserError> {
        if !self.stopped {
            return Err(ParserError::Wire(
                "anthropic: unexpected provider EOF (stream ended without message_stop)".to_owned(),
            ));
        }
        if self.finished {
            return Ok(None);
        }
        if self.finishing_tools.is_none() {
            self.finishing_tools = Some(self.tool_calls().into_iter());
        }
        if let Some(call) = self.finishing_tools.as_mut().unwrap().next() {
            return Ok(Some(AnthropicEvent::ToolCallDelta(call)));
        }
        self.finished = true;
        let content: Vec<_> = self
            .blocks
            .values()
            .map(Block::content)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect();
        Ok(Some(AnthropicEvent::Completed(CompletedMessage {
            finish_reason: self.finish_reason.clone(),
            input_tokens: self
                .input_tokens
                .wrapping_add(self.cache_read_input_tokens)
                .wrapping_add(self.cache_creation_input_tokens),
            cached_input_tokens: self.cache_read_input_tokens,
            output_tokens: self.output_tokens,
            assistant_text: self.assistant_text.clone(),
            tool_calls: self.tool_calls(),
            provider_content: format!("[{}]", content.join(",")),
            provider_id: self.provider_id.clone(),
        })))
    }

    fn tool_calls(&self) -> Vec<ToolCall> {
        self.blocks
            .values()
            .filter(|block| block.kind == "tool_use")
            .map(Block::tool_call)
            .collect()
    }

    fn part_id(&self, index: i32) -> String {
        format!("{}:{index}", self.message_id)
    }

    fn read_usage(&mut self, scope: &Object<'_>) -> Result<(), ParserError> {
        let Some(usage) = child_object(scope, "usage")? else {
            return Ok(());
        };
        for (key, target) in [
            ("input_tokens", &mut self.input_tokens),
            ("cache_read_input_tokens", &mut self.cache_read_input_tokens),
            (
                "cache_creation_input_tokens",
                &mut self.cache_creation_input_tokens,
            ),
            ("output_tokens", &mut self.output_tokens),
        ] {
            if let Some(value) = read_integer(&usage, key)?.filter(|value| *value >= 0) {
                *target = value;
            }
        }
        Ok(())
    }

    fn start_block(
        &mut self,
        index: i32,
        source: &Object<'_>,
        events: &mut Vec<AnthropicEvent>,
    ) -> Result<(), ParserError> {
        let kind = string(source, "type")?;
        match kind.as_str() {
            "text" => {
                self.blocks.insert(
                    index,
                    Block {
                        kind,
                        ..Block::default()
                    },
                );
                self.append(index, string(source, "text")?, false, events);
            }
            "thinking" => {
                self.blocks.insert(
                    index,
                    Block {
                        kind,
                        ..Block::default()
                    },
                );
                let signature = string(source, "signature")?;
                self.blocks
                    .get_mut(&index)
                    .unwrap()
                    .signature
                    .push_str(&signature);
                self.append(index, string(source, "thinking")?, true, events);
            }
            "redacted_thinking" => {
                let block = Block {
                    kind,
                    data: string(source, "data")?,
                    ..Block::default()
                };
                self.blocks.insert(index, block);
            }
            "tool_use" => {
                let block = Block {
                    kind,
                    id: string(source, "id")?,
                    name: string(source, "name")?,
                    initial_input: get(source, "input")?
                        .map_or("{}", |raw| raw.get())
                        .to_owned(),
                    ..Block::default()
                };
                self.blocks.insert(index, block);
            }
            _ => {}
        }
        Ok(())
    }

    fn continue_block(
        &mut self,
        index: i32,
        delta: &Object<'_>,
        events: &mut Vec<AnthropicEvent>,
    ) -> Result<(), ParserError> {
        if !self.blocks.contains_key(&index) {
            return Ok(());
        }
        match string(delta, "type")?.as_str() {
            "text_delta" => self.append(index, string(delta, "text")?, false, events),
            "thinking_delta" => self.append(index, string(delta, "thinking")?, true, events),
            "signature_delta" => self
                .blocks
                .get_mut(&index)
                .unwrap()
                .signature
                .push_str(&string(delta, "signature")?),
            "input_json_delta" => self
                .blocks
                .get_mut(&index)
                .unwrap()
                .text
                .push_str(&string(delta, "partial_json")?),
            _ => {}
        }
        Ok(())
    }

    fn append(
        &mut self,
        index: i32,
        text: String,
        thinking: bool,
        events: &mut Vec<AnthropicEvent>,
    ) {
        if text.is_empty() {
            return;
        }
        self.blocks.get_mut(&index).unwrap().text.push_str(&text);
        if thinking {
            events.push(AnthropicEvent::ReasoningDelta {
                text,
                part_id: self.part_id(index),
                completed: false,
            });
        } else {
            self.assistant_text.push_str(&text);
            events.push(AnthropicEvent::TextDelta { text });
        }
    }
}

fn get<'a>(scope: &Object<'a>, name: &str) -> Result<Option<&'a RawValue>, ParserError> {
    scope.get(name).map_err(ParserError::InvalidJsonString)
}

fn child_object<'a>(scope: &Object<'a>, name: &str) -> Result<Option<Object<'a>>, ParserError> {
    Ok(get(scope, name)?.and_then(RawObject::new))
}

fn read_string(scope: &Object<'_>, name: &str) -> Result<Option<String>, ParserError> {
    let Some(raw) = get(scope, name)?.filter(|raw| raw.get().starts_with('"')) else {
        return Ok(None);
    };
    selected_json_string(raw.get())
        .map(Some)
        .map_err(ParserError::InvalidJsonString)
}

fn string(scope: &Object<'_>, name: &str) -> Result<String, ParserError> {
    Ok(read_string(scope, name)?.unwrap_or_default())
}

fn read_integer(scope: &Object<'_>, name: &str) -> Result<Option<i32>, ParserError> {
    Ok(get(scope, name)?.and_then(|raw| serde_json::from_str(raw.get()).ok()))
}

fn integer(scope: &Object<'_>, name: &str) -> Result<i32, ParserError> {
    Ok(read_integer(scope, name)?.unwrap_or_default())
}

fn bound_response_body(body: &str) -> String {
    const LIMIT: usize = 64 << 10;
    const MARKER: &str = "\n… [truncated]";
    if body.len() <= LIMIT {
        return body.to_owned();
    }
    let mut end = LIMIT - MARKER.len();
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{MARKER}", &body[..end])
}
