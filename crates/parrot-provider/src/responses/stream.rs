use super::json::{source_compact, source_quote};
use crate::http_stream::{ByteStream, HttpError, SseDecoder};
use parrot_domain::contracts::async_ops::Cancellation;
use serde_json::value::RawValue;
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fmt,
};

type RawObject = HashMap<String, Box<RawValue>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesReasoningKind {
    Summary,
    Raw,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponsesToolCall {
    pub id: String,
    pub name: String,
    pub arguments_json: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponsesCompletion {
    pub response_id: String,
    pub finish_reason: String,
    pub assistant_text: String,
    pub tool_calls: Vec<ResponsesToolCall>,
    pub input_tokens: i32,
    pub cached_input_tokens: i32,
    pub output_tokens: i32,
    pub provider_id: String,
    /// Versioned source-compatible `NativeReplay` JSON. Raw item fragments are
    /// retained rather than converted through `serde_json::Value`.
    pub provider_content: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResponsesEvent {
    TextDelta(String),
    ReasoningDelta {
        text: String,
        kind: ResponsesReasoningKind,
        part_id: String,
        completed: bool,
    },
    EncryptedReasoningReceived,
    ToolCallDelta(ResponsesToolCall),
    Completed(ResponsesCompletion),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResponsesError {
    WireProtocol(String),
    ProviderResponse {
        error_type: String,
        error_code: String,
        detail: String,
        response_body: String,
    },
    Json(String),
    InvalidInteger {
        field: String,
        value: String,
    },
}
impl fmt::Display for ResponsesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WireProtocol(message) | Self::Json(message) => f.write_str(message),
            Self::ProviderResponse {
                error_type,
                error_code,
                detail,
                ..
            } => {
                let mut message = if detail.is_empty() {
                    "provider error".to_owned()
                } else {
                    detail.clone()
                };
                if !error_type.is_empty() {
                    message.push_str(&format!(" ({error_type})"));
                }
                if !error_code.is_empty() {
                    message.push_str(&format!(" [{error_code}]"));
                }
                f.write_str(&message)
            }
            Self::InvalidInteger { field, value } => {
                write!(f, "responses: {field} must be an integer (got {value})")
            }
        }
    }
}
impl std::error::Error for ResponsesError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResponsesStreamError {
    Transport(HttpError),
    Protocol(ResponsesError),
}

/// Cancellable, owning Responses event stream over T25's bounded byte reader.
/// It buffers only events produced from already-read SSE records; once the
/// terminal response is reached it stops reading and ignores trailing records.
pub struct ResponsesEventStream<'a> {
    source: &'a mut dyn ByteStream,
    sse: SseDecoder,
    parser: ResponsesParser,
    pending: VecDeque<ResponsesEvent>,
    pending_error: Option<ResponsesStreamError>,
    eof: bool,
    terminal: bool,
}
impl<'a> ResponsesEventStream<'a> {
    pub fn new(
        source: &'a mut dyn ByteStream,
        max_event_bytes: i32,
        provider_id: impl Into<String>,
    ) -> Self {
        Self {
            source,
            sse: SseDecoder::new(max_event_bytes),
            parser: ResponsesParser::new(provider_id),
            pending: VecDeque::new(),
            pending_error: None,
            eof: false,
            terminal: false,
        }
    }

    pub async fn next(
        &mut self,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<ResponsesEvent>, ResponsesStreamError> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }
            if let Some(error) = self.pending_error.take() {
                self.terminal = true;
                return Err(error);
            }
            if self.terminal {
                return Ok(None);
            }
            if self.eof {
                let records = self.sse.finish();
                self.accept_records(records);
                if let Some(event) = self.pending.pop_front() {
                    return Ok(Some(event));
                }
                if let Some(error) = self.pending_error.take() {
                    self.terminal = true;
                    return Err(error);
                }
                self.terminal = true;
                self.parser
                    .finish()
                    .map_err(ResponsesStreamError::Protocol)?;
                return Ok(None);
            }
            let mut bytes = [0_u8; 8192];
            let read = self
                .source
                .read(&mut bytes, cancellation)
                .await
                .map_err(ResponsesStreamError::Transport)?;
            if read == 0 {
                self.eof = true;
                continue;
            }
            let records = self.sse.feed(&bytes[..read]);
            self.accept_records(records);
        }
    }

    fn accept_records(&mut self, records: Vec<Result<crate::http_stream::SseEvent, HttpError>>) {
        for record in records {
            let record = match record {
                Ok(record) => record,
                Err(error) => {
                    self.pending_error = Some(ResponsesStreamError::Transport(error));
                    break;
                }
            };
            match self.parser.push_data(&record.data) {
                Ok(events) => self.pending.extend(events),
                Err(error) => {
                    self.pending_error = Some(ResponsesStreamError::Protocol(error));
                    break;
                }
            }
            if self.parser.done {
                self.terminal = true;
                break;
            }
        }
    }
}

/// One Responses event-state machine. Call `push_data` for each complete SSE
/// data payload; `finish` signals source EOF and requires a terminal response.
#[derive(Debug)]
pub struct ResponsesParser {
    provider_id: String,
    tools: HashMap<String, ToolState>,
    aliases: HashMap<String, String>,
    tool_order: Vec<String>,
    output: Vec<String>,
    streamed_output: BTreeMap<i32, String>,
    received_reasoning_ids: HashSet<String>,
    received_reasoning_positions: HashMap<i32, String>,
    created_response_id: String,
    sequence_number: Option<i64>,
    assistant_text: String,
    response_id: String,
    finish_reason: String,
    input_tokens: i32,
    cached_input_tokens: i32,
    output_tokens: i32,
    done: bool,
    terminal_emitted: bool,
}
impl ResponsesParser {
    pub fn new(provider_id: impl Into<String>) -> Self {
        Self {
            provider_id: provider_id.into(),
            tools: HashMap::new(),
            aliases: HashMap::new(),
            tool_order: Vec::new(),
            output: Vec::new(),
            streamed_output: BTreeMap::new(),
            received_reasoning_ids: HashSet::new(),
            received_reasoning_positions: HashMap::new(),
            created_response_id: String::new(),
            sequence_number: None,
            assistant_text: String::new(),
            response_id: String::new(),
            finish_reason: String::new(),
            input_tokens: 0,
            cached_input_tokens: 0,
            output_tokens: 0,
            done: false,
            terminal_emitted: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn original_text_frame_state(&self) -> (&str, &[String]) {
        (&self.response_id, &self.output)
    }

    pub fn push_data(&mut self, data: &str) -> Result<Vec<ResponsesEvent>, ResponsesError> {
        if self.done {
            return Ok(Vec::new());
        }
        if data == "[DONE]" {
            return Err(wire(
                "responses: provider stream completed without a terminal event",
            ));
        }
        let root = serde_json::from_str::<Box<RawValue>>(data)
            .map_err(|error| ResponsesError::Json(error.to_string()))?;
        let Ok(fields) = serde_json::from_str::<RawObject>(root.get()) else {
            return Ok(Vec::new());
        };
        let mut events = Vec::new();
        let kind = string_field(&fields, "type");
        self.validate_lifecycle(&fields, &kind)?;
        match kind.as_str() {
            "response.metadata" => {}
            "response.output_text.delta" => {
                let delta = string_field(&fields, "delta");
                if !delta.is_empty() {
                    self.assistant_text.push_str(&delta);
                    events.push(ResponsesEvent::TextDelta(delta));
                }
            }
            "response.reasoning_summary_text.delta"
            | "response.reasoning_text.delta"
            | "response.output_text.annotation.added"
            | "response.reasoning_summary_text.done"
            | "response.reasoning_text.done" => {
                let delta = string_field(&fields, "delta");
                let completed = kind.ends_with(".done");
                let reasoning_kind = if kind.starts_with("response.reasoning_summary_text") {
                    ResponsesReasoningKind::Summary
                } else {
                    ResponsesReasoningKind::Raw
                };
                if !delta.is_empty() || completed {
                    events.push(ResponsesEvent::ReasoningDelta {
                        text: delta,
                        kind: reasoning_kind,
                        part_id: reasoning_part_id(&fields, &kind),
                        completed,
                    });
                }
            }
            "response.output_item.added" => {
                let item = fields
                    .get("item")
                    .filter(|raw| is_object(raw.get()))
                    .map_or_else(empty_map, |raw| parse_map(raw.get()));
                let item = item?;
                if string_field(&item, "type") == "function_call" {
                    self.add_tool(
                        string_field(&item, "id"),
                        string_field(&item, "call_id"),
                        string_field(&item, "name"),
                        string_field(&item, "arguments"),
                    );
                }
            }
            "response.function_call_arguments.delta" => {
                let item_id = string_field(&fields, "item_id");
                let call_id = string_field(&fields, "call_id");
                let index = self.find_tool(&item_id, &call_id).unwrap_or_else(|| {
                    self.add_tool(
                        item_id.clone(),
                        call_id.clone(),
                        string_field(&fields, "name"),
                        String::new(),
                    )
                });
                if let Some(tool) = self.tools.get_mut(&index) {
                    tool.arguments.push_str(&string_field(&fields, "delta"));
                }
            }
            "response.function_call_arguments.done" | "response.output_item.done" => {
                let is_done_item = kind == "response.output_item.done";
                let item = if is_done_item {
                    let item_raw = fields.get("item").filter(|raw| is_object(raw.get()));
                    let item = item_raw.map_or_else(empty_map, |raw| parse_map(raw.get()))?;
                    if let Some(raw_item) = item_raw {
                        if self.receive_encrypted(
                            raw_item.get(),
                            optional_i32(&fields, "output_index")
                                .unwrap_or(self.streamed_output.len() as i32),
                        )? {
                            events.push(ResponsesEvent::EncryptedReasoningReceived);
                        }
                        self.capture_output_item(
                            raw_item.get(),
                            optional_i32(&fields, "output_index")
                                .unwrap_or(self.streamed_output.len() as i32),
                        );
                    }
                    item
                } else {
                    HashMap::new()
                };
                let (item_id, call_id, name, arguments, is_function_call) = if is_done_item {
                    (
                        string_field(&item, "id"),
                        string_field(&item, "call_id"),
                        string_field(&item, "name"),
                        string_field(&item, "arguments"),
                        string_field(&item, "type") == "function_call",
                    )
                } else {
                    (
                        string_field(&fields, "item_id"),
                        string_field(&fields, "call_id"),
                        string_field(&fields, "name"),
                        string_field(&fields, "arguments"),
                        true,
                    )
                };
                if !is_done_item || is_function_call {
                    if let Some(index) = self.find_tool(&item_id, &call_id) {
                        if !arguments.is_empty() {
                            if let Some(tool) = self.tools.get_mut(&index) {
                                tool.arguments.clear();
                                tool.arguments.push_str(&arguments);
                            }
                        }
                    } else {
                        self.add_tool(item_id, call_id, name, arguments);
                    }
                }
            }
            "response.completed" | "response.incomplete" => {
                self.read_usage(&fields)?;
                self.finish_reason = if kind == "response.completed" {
                    if self.tools.is_empty() {
                        "stop"
                    } else {
                        "tool_calls"
                    }
                    .to_owned()
                } else {
                    incomplete_reason(&fields)
                };
                self.done = true;
                if let Some(response) = fields.get("response") {
                    self.capture_completion(response.get());
                }
                if let Some(response) = fields.get("response") {
                    for _ in 0..self.receive_terminal_encrypted(response.get())? {
                        events.push(ResponsesEvent::EncryptedReasoningReceived);
                    }
                }
                events.extend(self.complete_events());
            }
            "response.failed" | "error" => {
                let failure = if kind == "response.failed" {
                    fields
                        .get("response")
                        .and_then(|raw| parse_map(raw.get()).ok())
                        .and_then(|response| {
                            response
                                .get("error")
                                .filter(|raw| is_object(raw.get()))
                                .map(|raw| raw.get().to_owned())
                        })
                        .or_else(|| {
                            fields
                                .get("error")
                                .filter(|raw| is_object(raw.get()))
                                .map(|raw| raw.get().to_owned())
                        })
                } else {
                    fields
                        .get("error")
                        .filter(|raw| is_object(raw.get()))
                        .map(|raw| raw.get().to_owned())
                };
                let failure = failure.unwrap_or_else(|| root.get().to_owned());
                let failure = parse_map(&failure)?;
                return Err(ResponsesError::ProviderResponse {
                    error_type: string_field(&failure, "type"),
                    error_code: string_field(&failure, "code"),
                    detail: string_field(&failure, "message"),
                    response_body: bound_response_body(data),
                });
            }
            _ => {}
        }
        Ok(events)
    }

    pub fn finish(&mut self) -> Result<Vec<ResponsesEvent>, ResponsesError> {
        if self.done {
            return Ok(Vec::new());
        }
        Err(wire(
            "responses: unexpected provider EOF (stream ended without a terminal event)",
        ))
    }

    fn validate_lifecycle(&mut self, root: &RawObject, kind: &str) -> Result<(), ResponsesError> {
        if let Some(sequence) = root.get("sequence_number").filter(|value| {
            value
                .get()
                .trim_start()
                .starts_with(|c: char| c.is_ascii_digit() || c == '-')
        }) {
            let value = serde_json::from_str::<i64>(sequence.get())
                .map_err(|_| wire("responses: sequence_number must be an integer"))?;
            if self
                .sequence_number
                .is_some_and(|previous| value <= previous)
            {
                return Err(wire(
                    "responses: sequence_number must be strictly increasing",
                ));
            }
            self.sequence_number = Some(value);
        }
        if kind == "response.created" {
            self.created_response_id = nested_response_id(root);
            return Ok(());
        }
        if matches!(
            kind,
            "response.completed" | "response.incomplete" | "response.failed"
        ) {
            let id = nested_response_id(root);
            if !self.created_response_id.is_empty()
                && !id.is_empty()
                && id != self.created_response_id
            {
                return Err(wire(
                    "responses: terminal response id does not match response.created",
                ));
            }
        }
        Ok(())
    }

    fn add_tool(
        &mut self,
        item_id: String,
        call_id: String,
        name: String,
        arguments: String,
    ) -> String {
        let mut key = if !item_id.is_empty() {
            item_id.clone()
        } else {
            call_id.clone()
        };
        if let Some(existing) = self.find_tool(&item_id, &call_id) {
            if let Some(tool) = self.tools.get_mut(&existing) {
                if tool.item_id.is_empty() {
                    tool.item_id = item_id.clone();
                }
                if tool.call_id.is_empty() {
                    tool.call_id = call_id.clone();
                }
                if tool.name.is_empty() {
                    tool.name = name;
                }
                if tool.arguments.is_empty() {
                    tool.arguments.push_str(&arguments);
                }
            }
            key = existing;
        } else {
            if key.is_empty() {
                key = "missing".to_owned();
                while self.tools.contains_key(&key) {
                    key.push(':');
                }
            }
            let mut tool = ToolState {
                item_id: item_id.clone(),
                call_id: call_id.clone(),
                name,
                arguments: String::new(),
            };
            tool.arguments.push_str(&arguments);
            self.tools.insert(key.clone(), tool);
            self.tool_order.push(key.clone());
        }
        if !item_id.is_empty() {
            self.aliases.insert(item_id, key.clone());
        }
        if !call_id.is_empty() {
            self.aliases.insert(call_id, key.clone());
        }
        key
    }

    fn find_tool(&self, item_id: &str, call_id: &str) -> Option<String> {
        for candidate in [item_id, call_id] {
            if candidate.is_empty() {
                continue;
            }
            if let Some(key) = self
                .aliases
                .get(candidate)
                .filter(|key| self.tools.contains_key(*key))
            {
                return Some(key.clone());
            }
            if self.tools.contains_key(candidate) {
                return Some(candidate.to_owned());
            }
        }
        None
    }

    fn complete_events(&mut self) -> Vec<ResponsesEvent> {
        if self.terminal_emitted {
            return Vec::new();
        }
        self.terminal_emitted = true;
        let calls = self
            .tool_order
            .iter()
            .filter_map(|key| {
                self.tools.get(key).map(|tool| ResponsesToolCall {
                    id: if tool.call_id.is_empty() {
                        tool.item_id.clone()
                    } else {
                        tool.call_id.clone()
                    },
                    name: tool.name.clone(),
                    arguments_json: tool.arguments.clone(),
                })
            })
            .collect::<Vec<_>>();
        let mut events = calls
            .iter()
            .cloned()
            .map(ResponsesEvent::ToolCallDelta)
            .collect::<Vec<_>>();
        events.push(ResponsesEvent::Completed(ResponsesCompletion {
            response_id: self.response_id.clone(),
            finish_reason: self.finish_reason.clone(),
            assistant_text: self.assistant_text.clone(),
            tool_calls: calls,
            input_tokens: self.input_tokens,
            cached_input_tokens: self.cached_input_tokens,
            output_tokens: self.output_tokens,
            provider_id: self.provider_id.clone(),
            provider_content: (!self.output.is_empty()).then(|| {
                format!(
                    "{{\"Version\":1,\"Protocol\":\"responses\",\"Output\":[{}]}}",
                    self.output.join(",")
                )
            }),
        }));
        events
    }

    fn read_usage(&mut self, root: &RawObject) -> Result<(), ResponsesError> {
        let Some(response) = root.get("response") else {
            return Ok(());
        };
        let response = parse_map(response.get())?;
        let Some(usage) = response.get("usage").filter(|raw| is_object(raw.get())) else {
            return Ok(());
        };
        let usage = parse_map(usage.get())?;
        self.input_tokens = read_i32(&usage, "input_tokens")?;
        self.output_tokens = read_i32(&usage, "output_tokens")?;
        if let Some(details) = usage
            .get("input_tokens_details")
            .filter(|raw| is_object(raw.get()))
        {
            self.cached_input_tokens = read_i32(&parse_map(details.get())?, "cached_tokens")?;
        }
        Ok(())
    }

    fn receive_encrypted(&mut self, raw: &str, index: i32) -> Result<bool, ResponsesError> {
        if !is_object(raw) {
            return Ok(false);
        }
        let item = parse_map(raw)?;
        self.record_encrypted(&item, index)
    }
    fn receive_terminal_encrypted(&mut self, raw: &str) -> Result<usize, ResponsesError> {
        let response = parse_map(raw)?;
        let Some(output) = response
            .get("output")
            .filter(|raw| raw.get().trim_start().starts_with('['))
        else {
            return Ok(0);
        };
        let items: Vec<Box<RawValue>> = serde_json::from_str(output.get())
            .map_err(|error| ResponsesError::Json(error.to_string()))?;
        let mut count = 0;
        for (index, item) in items.iter().enumerate() {
            if self.receive_encrypted(item.get(), index as i32)? {
                count += 1;
            }
        }
        Ok(count)
    }
    fn record_encrypted(&mut self, item: &RawObject, index: i32) -> Result<bool, ResponsesError> {
        if string_field(item, "type") != "reasoning"
            || item.get("encrypted_content").is_none_or(|raw| {
                serde_json::from_str::<String>(raw.get())
                    .ok()
                    .is_none_or(|value| value.is_empty())
            })
        {
            return Ok(false);
        }
        let id = string_field(item, "id");
        let repeated_id = !id.is_empty() && !self.received_reasoning_ids.insert(id.clone());
        let previous = self.received_reasoning_positions.get(&index).cloned();
        let repeated_position = previous
            .as_ref()
            .is_some_and(|previous| id.is_empty() || previous.is_empty() || previous == &id);
        self.received_reasoning_positions.insert(
            index,
            if !id.is_empty() {
                id
            } else {
                previous.unwrap_or_default()
            },
        );
        Ok(!repeated_id && !repeated_position)
    }
    fn capture_output_item(&mut self, raw: &str, index: i32) {
        if replayable_item(raw) {
            if let Ok(item) = source_compact(raw) {
                self.streamed_output.insert(index, item);
            }
        }
    }
    fn capture_completion(&mut self, raw: &str) {
        let Ok(response) = parse_map(raw) else {
            return;
        };
        self.response_id = string_field(&response, "id");
        if let Some(output) = response
            .get("output")
            .filter(|raw| raw.get().trim_start().starts_with('['))
        {
            if let Ok(items) = serde_json::from_str::<Vec<Box<RawValue>>>(output.get()) {
                self.output.extend(
                    items
                        .iter()
                        .filter(|item| replayable_item(item.get()))
                        .filter_map(|item| source_compact(item.get()).ok()),
                );
            }
        }
        if self.output.is_empty() {
            self.output.extend(self.streamed_output.values().cloned());
        }
        if !self.output.is_empty() {
            self.preserve_ordinary_output();
        }
    }
    fn preserve_ordinary_output(&mut self) {
        if !self.assistant_text.is_empty()
            && !self
                .output
                .iter()
                .any(|item| string_field(&parse_map(item).unwrap_or_default(), "type") == "message")
        {
            let text = quote(&self.assistant_text);
            self.output.push(format!("{{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":{text}}}]}}"));
        }
        let calls = self
            .tool_order
            .iter()
            .filter_map(|key| self.tools.get(key))
            .collect::<Vec<_>>();
        for call in calls {
            let tool_id = if call.call_id.is_empty() {
                &call.item_id
            } else {
                &call.call_id
            };
            let already_present = self.output.iter().any(|item| {
                let fields = parse_map(item).unwrap_or_default();
                string_field(&fields, "type") == "function_call"
                    && string_field(&fields, "call_id") == *tool_id
            });
            if already_present {
                continue;
            }
            let id = if call.item_id.is_empty() {
                String::new()
            } else {
                format!("\"id\":{},", quote(&call.item_id))
            };
            self.output.push(format!(
                "{{{id}\"type\":\"function_call\",\"call_id\":{},\"name\":{},\"arguments\":{}}}",
                quote(tool_id),
                quote(&call.name),
                quote(&call.arguments)
            ));
        }
    }
}

#[derive(Debug)]
struct ToolState {
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
}
const MAX_RESPONSE_BODY_BYTES: usize = 64 * 1024;
const RESPONSE_BODY_TRUNCATION_MARKER: &str = "\n… [truncated]";

fn bound_response_body(body: &str) -> String {
    if body.len() <= MAX_RESPONSE_BODY_BYTES {
        return body.to_owned();
    }

    let prefix_limit = MAX_RESPONSE_BODY_BYTES - RESPONSE_BODY_TRUNCATION_MARKER.len();
    let mut prefix_length = 0;
    for (offset, character) in body.char_indices() {
        let end = offset + character.len_utf8();
        if end > prefix_limit {
            break;
        }
        prefix_length = end;
    }
    let mut bounded = String::with_capacity(MAX_RESPONSE_BODY_BYTES);
    bounded.push_str(&body[..prefix_length]);
    bounded.push_str(RESPONSE_BODY_TRUNCATION_MARKER);
    bounded
}

fn wire(message: &str) -> ResponsesError {
    ResponsesError::WireProtocol(message.to_owned())
}
fn parse_map(raw: &str) -> Result<RawObject, ResponsesError> {
    serde_json::from_str(raw).map_err(|error| ResponsesError::Json(error.to_string()))
}
fn empty_map() -> Result<RawObject, ResponsesError> {
    Ok(HashMap::new())
}
fn is_object(raw: &str) -> bool {
    raw.trim_start().starts_with('{')
}
fn string_field(fields: &RawObject, name: &str) -> String {
    fields
        .get(name)
        .and_then(|value| serde_json::from_str::<String>(value.get()).ok())
        .unwrap_or_default()
}
fn optional_i32(fields: &RawObject, name: &str) -> Option<i32> {
    fields
        .get(name)
        .and_then(|value| serde_json::from_str(value.get()).ok())
}
fn nested_response_id(fields: &RawObject) -> String {
    fields
        .get("response")
        .and_then(|raw| parse_map(raw.get()).ok())
        .map_or_else(String::new, |response| string_field(&response, "id"))
}
fn read_i32(fields: &RawObject, name: &str) -> Result<i32, ResponsesError> {
    let Some(raw) = fields.get(name).filter(|value| {
        value
            .get()
            .trim_start()
            .starts_with(|c: char| c.is_ascii_digit() || c == '-')
    }) else {
        return Ok(0);
    };
    serde_json::from_str::<i32>(raw.get()).map_err(|_| ResponsesError::InvalidInteger {
        field: name.to_owned(),
        value: raw.get().to_owned(),
    })
}
fn reasoning_part_id(fields: &RawObject, kind: &str) -> String {
    let item_id = string_field(fields, "item_id");
    let index_field = match kind {
        "response.reasoning_summary_text.delta" | "response.reasoning_summary_text.done" => {
            "summary_index"
        }
        "response.reasoning_text.delta" | "response.reasoning_text.done" => "content_index",
        "response.output_text.annotation.added" => "annotation_index",
        _ => "",
    };
    let index = fields
        .get(index_field)
        .filter(|raw| is_number(raw.get()))
        .map_or_else(String::new, |raw| {
            serde_json::from_str::<i64>(raw.get())
                .map_or_else(|_| raw.get().to_owned(), |value| value.to_string())
        });
    if item_id.is_empty() {
        index
    } else if index.is_empty() {
        item_id
    } else {
        format!("{item_id}:{index}")
    }
}
fn is_number(raw: &str) -> bool {
    raw.trim_start()
        .as_bytes()
        .first()
        .is_some_and(|byte| byte.is_ascii_digit() || *byte == b'-')
}
fn incomplete_reason(fields: &RawObject) -> String {
    let response = fields
        .get("response")
        .and_then(|raw| parse_map(raw.get()).ok());
    let details = response.and_then(|response| {
        response
            .get("incomplete_details")
            .and_then(|raw| parse_map(raw.get()).ok())
    });
    match details
        .map_or_else(String::new, |details| string_field(&details, "reason"))
        .as_str()
    {
        "max_output_tokens" | "max_tokens" => "length".to_owned(),
        "content_filter" => "content_filter".to_owned(),
        _ => "incomplete".to_owned(),
    }
}
fn replayable_item(raw: &str) -> bool {
    let Ok(item) = parse_map(raw) else {
        return false;
    };
    match string_field(&item, "type").as_str() {
        "reasoning" => {
            let encrypted = item.get("encrypted_content").is_none_or(|value| {
                value.get() == "null" || serde_json::from_str::<String>(value.get()).is_ok()
            });
            let summary = item
                .get("summary")
                .is_none_or(|value| value.get().trim_start().starts_with('['));
            encrypted && summary
        }
        "message" => {
            string_field(&item, "role") == "assistant"
                && item
                    .get("content")
                    .is_some_and(|value| value.get().trim_start().starts_with('['))
        }
        "function_call" => {
            !string_field(&item, "call_id").is_empty()
                && !string_field(&item, "name").is_empty()
                && item
                    .get("arguments")
                    .is_some_and(|value| serde_json::from_str::<String>(value.get()).is_ok())
        }
        _ => false,
    }
}
fn quote(text: &str) -> String {
    source_quote(text)
}
