use crate::http_stream::{HttpError, HttpEventStream, HttpStreamEvent, SseEvent};
use parrot_domain::contracts::async_ops::Cancellation;
use serde_json::value::RawValue;
use std::collections::{BTreeMap, VecDeque};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChatEvent {
    RequestStarted,
    ResponseHeadersReceived,
    TextDelta(String),
    ReasoningDelta { text: String, kind: ReasoningKind },
    ToolCallDelta(ToolCall),
    Completed(ChatCompletion),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReasoningKind {
    Raw,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments_json: String,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatReplay {
    pub provider_id: String,
    pub version: i64,
    pub protocol: String,
    pub reasoning_content: Option<String>,
    pub reasoning: Option<String>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatCompletion {
    pub finish_reason: String,
    pub input_tokens: i32,
    pub cached_input_tokens: i32,
    pub output_tokens: i32,
    pub assistant_text: String,
    pub tool_calls: Vec<ToolCall>,
    pub provider_content: Option<ChatReplay>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderFailure {
    pub error_type: String,
    pub error_code: String,
    pub message: String,
    pub response_body: String,
}
#[derive(Debug)]
pub enum ChatStreamError {
    Http(HttpError),
    InvalidJson(serde_json::Error),
    InvalidInteger(String),
    Provider(ProviderFailure),
    UnexpectedEof,
}
impl std::fmt::Display for ChatStreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(error) => error.fmt(f),
            Self::InvalidJson(error) => error.fmt(f),
            Self::InvalidInteger(field) => {
                write!(f, "invalid integer in Chat Completions field {field}")
            }
            Self::Provider(error) => {
                let mut message = if error.message.is_empty() {
                    "provider error".to_owned()
                } else {
                    error.message.clone()
                };
                if !error.error_type.is_empty() {
                    message.push_str(&format!(" ({})", error.error_type));
                }
                if !error.error_code.is_empty() {
                    message.push_str(&format!(" [{}]", error.error_code));
                }
                f.write_str(&message)
            }
            Self::UnexpectedEof => f.write_str(
                "chatcompletions: unexpected provider EOF (stream ended without a terminal event)",
            ),
        }
    }
}
impl std::error::Error for ChatStreamError {}
impl From<HttpError> for ChatStreamError {
    fn from(error: HttpError) -> Self {
        Self::Http(error)
    }
}

#[derive(Default)]
struct Assembly {
    id: String,
    name: String,
    arguments: String,
}
#[derive(Default)]
struct State {
    tools: BTreeMap<i32, Assembly>,
    finish_seen: bool,
    finish_reason: String,
    input: i32,
    cached_input: i32,
    output: i32,
    text: String,
    reasoning_content: String,
    reasoning: String,
}
impl State {
    fn complete(&self, provider_id: &str) -> ChatCompletion {
        let provider_content = if self.reasoning_content.is_empty() && self.reasoning.is_empty() {
            None
        } else {
            Some(ChatReplay {
                provider_id: provider_id.into(),
                version: 1,
                protocol: "chat-completions".into(),
                reasoning_content: (!self.reasoning_content.is_empty())
                    .then(|| self.reasoning_content.clone()),
                reasoning: (!self.reasoning.is_empty()).then(|| self.reasoning.clone()),
            })
        };
        ChatCompletion {
            finish_reason: self.finish_reason.clone(),
            input_tokens: self.input,
            cached_input_tokens: self.cached_input,
            output_tokens: self.output,
            assistant_text: self.text.clone(),
            tool_calls: self
                .tools
                .values()
                .map(|t| ToolCall {
                    id: t.id.clone(),
                    name: t.name.clone(),
                    arguments_json: t.arguments.clone(),
                })
                .collect(),
            provider_content,
        }
    }
    fn final_events(&self, provider_id: &str) -> VecDeque<ChatEvent> {
        let mut events = self
            .tools
            .values()
            .map(|t| {
                ChatEvent::ToolCallDelta(ToolCall {
                    id: t.id.clone(),
                    name: t.name.clone(),
                    arguments_json: t.arguments.clone(),
                })
            })
            .collect::<VecDeque<_>>();
        events.push_back(ChatEvent::Completed(self.complete(provider_id)));
        events
    }
}

/// Incremental provider stream adapter. HTTP start/header lifecycle events are
/// consumed internally; only Chat Completions payload events are returned.
pub struct ChatStream {
    http: HttpEventStream,
    provider_id: String,
    state: ParseState,
    pending: VecDeque<ChatEvent>,
    finished: bool,
}
impl ChatStream {
    pub fn new(http: HttpEventStream, provider_id: impl Into<String>) -> Self {
        Self {
            http,
            provider_id: provider_id.into(),
            state: ParseState::default(),
            pending: VecDeque::new(),
            finished: false,
        }
    }
    pub fn response_bytes(&self) -> u64 {
        self.http.response_bytes()
    }
    pub fn turn_state(&self) -> Option<&str> {
        self.http.turn_state()
    }
    pub fn finish_seen(&self) -> bool {
        self.state.finish_seen()
    }
    pub async fn next(
        &mut self,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<ChatEvent>, ChatStreamError> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(Some(event));
        }
        if self.finished {
            return Ok(None);
        }
        let result = self.next_inner(cancellation).await;
        if result.is_err() {
            self.finished = true;
            self.pending.clear();
        }
        result
    }
    async fn next_inner(
        &mut self,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<ChatEvent>, ChatStreamError> {
        loop {
            match self.http.next(cancellation).await? {
                Some(HttpStreamEvent::RequestStarted) => {
                    return Ok(Some(ChatEvent::RequestStarted));
                }
                Some(HttpStreamEvent::ResponseHeadersReceived) => {
                    return Ok(Some(ChatEvent::ResponseHeadersReceived));
                }
                Some(HttpStreamEvent::Event(event)) if event.data == "[DONE]" => {
                    if !self.state.0.finish_seen {
                        return Err(ChatStreamError::UnexpectedEof);
                    }
                    self.finished = true;
                    self.pending = self.state.terminal_events(&self.provider_id);
                    return Ok(self.pending.pop_front());
                }
                Some(HttpStreamEvent::Event(event)) => {
                    let events = parse_data(&event, &mut self.state)?;
                    if let Some(first) = events.first().cloned() {
                        self.pending.extend(events.into_iter().skip(1));
                        return Ok(Some(first));
                    }
                }
                None => {
                    if !self.state.0.finish_seen {
                        return Err(ChatStreamError::UnexpectedEof);
                    }
                    self.finished = true;
                    self.pending = self.state.terminal_events(&self.provider_id);
                    return Ok(self.pending.pop_front());
                }
            }
        }
    }
}

/// Consumes one already-decoded SSE event. This is also the deterministic
/// seam used to compare source fragments without constructing an HTTP request.
pub fn parse_data(
    event: &SseEvent,
    state: &mut ParseState,
) -> Result<Vec<ChatEvent>, ChatStreamError> {
    parse_data_text(&event.data, state)
}

// Public because callers may incrementally feed SSE events, but state remains
// explicit and owned by each independent conversation.
#[derive(Default)]
pub struct ParseState(State);
impl ParseState {
    pub fn finish_seen(&self) -> bool {
        self.0.finish_seen
    }
    pub fn finish_reason(&self) -> &str {
        &self.0.finish_reason
    }
    pub fn completion(&self, provider_id: &str) -> ChatCompletion {
        self.0.complete(provider_id)
    }
    pub fn terminal_events(&self, provider_id: &str) -> VecDeque<ChatEvent> {
        self.0.final_events(provider_id)
    }
}
fn parse_data_text(data: &str, state: &mut ParseState) -> Result<Vec<ChatEvent>, ChatStreamError> {
    let root_raw: Box<RawValue> =
        serde_json::from_str(data).map_err(ChatStreamError::InvalidJson)?;
    let Some(root) = raw_object(root_raw.get()) else {
        return Ok(Vec::new());
    };
    if let Some(error_raw) = root.get("error").filter(|raw| raw.get().starts_with('{')) {
        let error = raw_object(error_raw.get()).expect("valid raw JSON object");
        return Err(ChatStreamError::Provider(ProviderFailure {
            error_type: raw_string(&error, "type"),
            error_code: raw_scalar(data, "code"),
            message: raw_string(&error, "message"),
            response_body: bound_body(data),
        }));
    }
    let mut events = Vec::new();
    if let Some(choices_raw) = root.get("choices").filter(|raw| raw.get().starts_with('[')) {
        let choices: Vec<Box<RawValue>> =
            serde_json::from_str(choices_raw.get()).map_err(ChatStreamError::InvalidJson)?;
        for choice_raw in choices {
            let Some(choice) = raw_object(choice_raw.get()) else {
                continue;
            };
            if let Some(delta_raw) = choice.get("delta").filter(|raw| raw.get().starts_with('{')) {
                let delta = raw_object(delta_raw.get()).expect("valid raw JSON object");
                let content = raw_string(&delta, "content");
                if !content.is_empty() {
                    state.0.text.push_str(&content);
                    events.push(ChatEvent::TextDelta(content));
                }
                let native_content = raw_string(&delta, "reasoning_content");
                let native_reasoning = raw_string(&delta, "reasoning");
                state.0.reasoning_content.push_str(&native_content);
                state.0.reasoning.push_str(&native_reasoning);
                let reasoning = if !native_content.is_empty() {
                    native_content
                } else {
                    native_reasoning
                };
                if !reasoning.is_empty() {
                    events.push(ChatEvent::ReasoningDelta {
                        text: reasoning,
                        kind: ReasoningKind::Raw,
                    });
                }
                if let Some(calls_raw) = delta
                    .get("tool_calls")
                    .filter(|raw| raw.get().starts_with('['))
                {
                    let calls: Vec<Box<RawValue>> = serde_json::from_str(calls_raw.get())
                        .map_err(ChatStreamError::InvalidJson)?;
                    for call_raw in calls {
                        let Some(call) = raw_object(call_raw.get()) else {
                            continue;
                        };
                        let index = raw_int(&call, "index")?;
                        let assembly = state.0.tools.entry(index).or_default();
                        let id = raw_string(&call, "id");
                        if !id.is_empty() {
                            assembly.id = id;
                        }
                        if let Some(function_raw) = call
                            .get("function")
                            .filter(|raw| raw.get().starts_with('{'))
                        {
                            let function =
                                raw_object(function_raw.get()).expect("valid raw JSON object");
                            let name = raw_string(&function, "name");
                            if !name.is_empty() {
                                assembly.name = name;
                            }
                            assembly
                                .arguments
                                .push_str(&raw_string(&function, "arguments"));
                        }
                    }
                }
            }
            if let Some(finish) = choice
                .get("finish_reason")
                .and_then(|raw| raw_string_value(raw.get()))
            {
                state.0.finish_seen = true;
                state.0.finish_reason = map_finish(&finish).to_owned();
            }
        }
    }
    if let Some(usage_raw) = root.get("usage").filter(|raw| raw.get().starts_with('{')) {
        let usage = raw_object(usage_raw.get()).expect("valid raw JSON object");
        state.0.input = raw_int(&usage, "prompt_tokens")?;
        state.0.cached_input = usage
            .get("prompt_tokens_details")
            .filter(|raw| raw.get().starts_with('{'))
            .and_then(|raw| raw_object(raw.get()))
            .map(|details| raw_int(&details, "cached_tokens"))
            .transpose()?
            .unwrap_or(0);
        state.0.output = raw_int(&usage, "completion_tokens")?;
    }
    Ok(events)
}
fn raw_object(raw: &str) -> Option<BTreeMap<String, Box<RawValue>>> {
    if !raw.starts_with('{') {
        return None;
    }
    serde_json::from_str(raw).ok()
}
fn raw_string_value(raw: &str) -> Option<String> {
    if !raw.starts_with('"') {
        return None;
    }
    serde_json::from_str(raw).ok()
}
fn raw_string(object: &BTreeMap<String, Box<RawValue>>, field: &str) -> String {
    object
        .get(field)
        .and_then(|raw| raw_string_value(raw.get()))
        .unwrap_or_default()
}
fn raw_int(object: &BTreeMap<String, Box<RawValue>>, field: &str) -> Result<i32, ChatStreamError> {
    let Some(raw) = object.get(field) else {
        return Ok(0);
    };
    let token = raw.get();
    if token == "null"
        || !token
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_digit() || *byte == b'-')
    {
        return Ok(0);
    }
    token
        .parse::<i32>()
        .map_err(|_| ChatStreamError::InvalidInteger(field.to_owned()))
}
fn map_finish(reason: &str) -> &'static str {
    match reason {
        "stop" => "stop",
        "tool_calls" | "function_call" => "tool_calls",
        "length" | "max_tokens" => "length",
        "content_filter" => "content_filter",
        _ => "incomplete",
    }
}
fn raw_scalar(body: &str, name: &str) -> String {
    let Ok(root) = serde_json::from_str::<BTreeMap<String, Box<RawValue>>>(body) else {
        return String::new();
    };
    let Some(error) = root.get("error").and_then(|raw| raw_object(raw.get())) else {
        return String::new();
    };
    let Some(raw) = error.get(name) else {
        return String::new();
    };
    let token = raw.get();
    if let Some(value) = raw_string_value(token) {
        value
    } else if token
        .as_bytes()
        .first()
        .is_some_and(|byte| byte.is_ascii_digit() || *byte == b'-')
    {
        token.to_owned()
    } else {
        String::new()
    }
}

fn bound_body(body: &str) -> String {
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
