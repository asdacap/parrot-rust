//! Caller-owned session values and borrowed protocol views. Opaque JSON stays raw.
use crate::{anthropic, chat_completions, responses, responses_websocket};
use std::{io, path::Path, time::Duration};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Request {
    pub model: String,
    pub instructions: String,
    pub messages: Vec<Message>,
    pub tools: Vec<Tool>,
    pub max_output_tokens: i32,
    pub reasoning: Option<Reasoning>,
    pub provider_preferences: String,
    pub include_router_metadata: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Reasoning {
    pub effort: String,
    pub summary: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Role {
    System,
    #[default]
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Content {
    Text(String),
    Image {
        path: String,
        url: String,
        media_type: String,
        width: i32,
        height: i32,
        /// Synthetic or already-owned bytes; absent means read the file lazily.
        bytes: Option<Vec<u8>>,
    },
}
impl Content {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(text.into())
    }
    pub fn image_file(
        path: impl Into<String>,
        media_type: impl Into<String>,
        width: i32,
        height: i32,
    ) -> Self {
        Self::Image {
            path: path.into(),
            url: String::new(),
            media_type: media_type.into(),
            width,
            height,
            bytes: None,
        }
    }
    pub fn image_link(
        path: impl Into<String>,
        url: impl Into<String>,
        media_type: impl Into<String>,
        width: i32,
        height: i32,
    ) -> Self {
        Self::Image {
            path: path.into(),
            url: url.into(),
            media_type: media_type.into(),
            width,
            height,
            bytes: None,
        }
    }
    pub fn text_value(&self) -> &str {
        match self {
            Self::Text(text) => text,
            Self::Image { .. } => "",
        }
    }
    pub fn image_path(&self) -> &str {
        match self {
            Self::Image { path, .. } => path,
            Self::Text(_) => "",
        }
    }
    pub fn image_url(&self) -> &str {
        match self {
            Self::Image { url, .. } => url,
            Self::Text(_) => "",
        }
    }
    pub fn media_type(&self) -> &str {
        match self {
            Self::Image { media_type, .. } => media_type,
            Self::Text(_) => "",
        }
    }
    pub fn image_width(&self) -> i32 {
        match self {
            Self::Image { width, .. } => *width,
            Self::Text(_) => 0,
        }
    }
    pub fn image_height(&self) -> i32 {
        match self {
            Self::Image { height, .. } => *height,
            Self::Text(_) => 0,
        }
    }
    pub fn read_image(&self) -> io::Result<Vec<u8>> {
        match self {
            Self::Image {
                bytes: Some(bytes), ..
            } => Ok(bytes.clone()),
            Self::Image { path, .. } => std::fs::read(path),
            Self::Text(_) => Ok(Vec::new()),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters_json: String,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments_json: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Message {
    pub role: Role,
    contents: Vec<Content>,
    // The borrowed adapter APIs require a slice. Keep the source's concatenated
    // text projection in sync through set_contents rather than expose mutation.
    text: String,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: String,
    pub provider_id: String,
    pub provider_content: String,
}
impl Message {
    pub fn new(role: Role, contents: Vec<Content>) -> Self {
        let text = contents.iter().map(Content::text_value).collect();
        Self {
            role,
            contents,
            text,
            ..Self::default()
        }
    }
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self::new(role, vec![Content::Text(text.into())])
    }
    pub fn user(text: impl Into<String>) -> Self {
        Self::text(Role::User, text)
    }
    pub fn system(text: impl Into<String>) -> Self {
        Self::text(Role::System, text)
    }
    pub fn assistant(text: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            tool_calls,
            ..Self::text(Role::Assistant, text)
        }
    }
    pub fn tool_result(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            tool_call_id: id.into(),
            ..Self::text(Role::Tool, text)
        }
    }
    pub fn contents(&self) -> &[Content] {
        &self.contents
    }
    pub fn set_contents(&mut self, contents: Vec<Content>) {
        self.text = contents.iter().map(Content::text_value).collect();
        self.contents = contents;
    }
    pub fn content(&self) -> &str {
        &self.text
    }
}

impl chat_completions::RequestView for Request {
    type Message = Message;
    type Tool = Tool;
    fn model(&self) -> &str {
        &self.model
    }
    fn instructions(&self) -> &str {
        &self.instructions
    }
    fn messages(&self) -> &[Message] {
        &self.messages
    }
    fn tools(&self) -> &[Tool] {
        &self.tools
    }
    fn max_output_tokens(&self) -> i32 {
        self.max_output_tokens
    }
    fn reasoning_effort(&self) -> Option<&str> {
        self.reasoning.as_ref().map(|r| r.effort.as_str())
    }
    fn reasoning_summary(&self) -> Option<&str> {
        self.reasoning.as_ref().map(|r| r.summary.as_str())
    }
    fn provider_preferences(&self) -> &str {
        &self.provider_preferences
    }
    fn include_router_metadata(&self) -> bool {
        self.include_router_metadata
    }
}
impl responses::ResponsesRequestView for Request {
    type Message = Message;
    type Tool = Tool;
    fn model(&self) -> &str {
        &self.model
    }
    fn instructions(&self) -> &str {
        &self.instructions
    }
    fn messages(&self) -> &[Message] {
        &self.messages
    }
    fn tools(&self) -> &[Tool] {
        &self.tools
    }
    fn max_output_tokens(&self) -> i32 {
        self.max_output_tokens
    }
    fn reasoning_effort(&self) -> &str {
        self.reasoning.as_ref().map_or("", |r| &r.effort)
    }
    fn reasoning_summary(&self) -> &str {
        self.reasoning.as_ref().map_or("", |r| &r.summary)
    }
    fn provider_preferences(&self) -> &str {
        &self.provider_preferences
    }
    fn include_router_metadata(&self) -> bool {
        self.include_router_metadata
    }
}
impl anthropic::RequestView for Request {
    type Message = Message;
    type Tool = Tool;
    fn model(&self) -> &str {
        &self.model
    }
    fn instructions(&self) -> &str {
        &self.instructions
    }
    fn messages(&self) -> &[Message] {
        &self.messages
    }
    fn tools(&self) -> &[Tool] {
        &self.tools
    }
    fn max_output_tokens(&self) -> i32 {
        self.max_output_tokens
    }
    fn reasoning_effort(&self) -> Option<&str> {
        self.reasoning.as_ref().map(|r| r.effort.as_str())
    }
    fn reasoning_summary(&self) -> Option<&str> {
        self.reasoning.as_ref().map(|r| r.summary.as_str())
    }
    fn provider_preferences(&self) -> &str {
        &self.provider_preferences
    }
    fn include_router_metadata(&self) -> bool {
        self.include_router_metadata
    }
}

impl chat_completions::MessageView for Message {
    type Content = Content;
    type Call = ToolCall;
    fn role(&self) -> chat_completions::MessageRole {
        match self.role {
            Role::System => chat_completions::MessageRole::System,
            Role::User => chat_completions::MessageRole::User,
            Role::Assistant => chat_completions::MessageRole::Assistant,
            Role::Tool => chat_completions::MessageRole::Tool,
        }
    }
    fn text(&self) -> &str {
        self.content()
    }
    fn contents(&self) -> &[Content] {
        &self.contents
    }
    fn tool_call_id(&self) -> &str {
        &self.tool_call_id
    }
    fn tool_calls(&self) -> &[ToolCall] {
        &self.tool_calls
    }
    fn provider_id(&self) -> &str {
        &self.provider_id
    }
    fn provider_content(&self) -> &str {
        &self.provider_content
    }
}
impl responses::ResponsesMessageView for Message {
    type Content = Content;
    type ToolCall = ToolCall;
    fn role(&self) -> responses::ResponsesRole {
        match self.role {
            Role::System => responses::ResponsesRole::System,
            Role::User => responses::ResponsesRole::User,
            Role::Assistant => responses::ResponsesRole::Assistant,
            Role::Tool => responses::ResponsesRole::Tool,
        }
    }
    fn content(&self) -> &str {
        self.content()
    }
    fn contents(&self) -> &[Content] {
        &self.contents
    }
    fn tool_call_id(&self) -> &str {
        &self.tool_call_id
    }
    fn tool_calls(&self) -> &[ToolCall] {
        &self.tool_calls
    }
    fn provider_id(&self) -> &str {
        &self.provider_id
    }
    fn provider_content(&self) -> &str {
        &self.provider_content
    }
}
impl anthropic::MessageView for Message {
    type Content = Content;
    type ToolCall = ToolCall;
    fn role(&self) -> anthropic::Role {
        match self.role {
            Role::System => anthropic::Role::System,
            Role::User => anthropic::Role::User,
            Role::Assistant => anthropic::Role::Assistant,
            Role::Tool => anthropic::Role::Tool,
        }
    }
    fn contents(&self) -> &[Content] {
        &self.contents
    }
    fn tool_call_id(&self) -> &str {
        &self.tool_call_id
    }
    fn tool_calls(&self) -> &[ToolCall] {
        &self.tool_calls
    }
    fn provider_id(&self) -> &str {
        &self.provider_id
    }
    fn provider_content(&self) -> &str {
        &self.provider_content
    }
}

impl chat_completions::ContentView for Content {
    fn is_text(&self) -> bool {
        matches!(self, Self::Text(_))
    }
    fn text(&self) -> &str {
        self.text_value()
    }
    fn image_url(&self) -> &str {
        self.image_url()
    }
    fn media_type(&self) -> &str {
        self.media_type()
    }
    fn read_image(&self) -> Result<Vec<u8>, chat_completions::RequestError> {
        Content::read_image(self)
            .map_err(|error| chat_completions::RequestError::Image(error.to_string()))
    }
}
impl responses::ResponsesContentView for Content {
    fn kind(&self) -> responses::ResponsesContentKind {
        if matches!(self, Self::Text(_)) {
            responses::ResponsesContentKind::Text
        } else {
            responses::ResponsesContentKind::Image
        }
    }
    fn text(&self) -> &str {
        self.text_value()
    }
    fn image_path(&self) -> &Path {
        Path::new(self.image_path())
    }
    fn image_url(&self) -> &str {
        self.image_url()
    }
    fn image_media_type(&self) -> &str {
        self.media_type()
    }
    fn read_image(&self) -> io::Result<Vec<u8>> {
        Content::read_image(self)
    }
}
impl anthropic::ContentView for Content {
    fn kind(&self) -> anthropic::ContentKind {
        if matches!(self, Self::Text(_)) {
            anthropic::ContentKind::Text
        } else {
            anthropic::ContentKind::Image
        }
    }
    fn text(&self) -> &str {
        self.text_value()
    }
    fn image_path(&self) -> &str {
        self.image_path()
    }
    fn image_url(&self) -> &str {
        self.image_url()
    }
    fn media_type(&self) -> &str {
        self.media_type()
    }
    fn image_width(&self) -> i32 {
        self.image_width()
    }
    fn image_height(&self) -> i32 {
        self.image_height()
    }
    fn read_image(&self) -> io::Result<Vec<u8>> {
        Content::read_image(self)
    }
}

macro_rules! tool_view {
    ($trait:path) => {
        impl $trait for Tool {
            fn name(&self) -> &str {
                &self.name
            }
            fn description(&self) -> &str {
                &self.description
            }
            fn parameters_json(&self) -> &str {
                &self.parameters_json
            }
        }
    };
}
tool_view!(chat_completions::ToolView);
tool_view!(responses::ResponsesToolDefinitionView);
tool_view!(anthropic::ToolDefinitionView);
macro_rules! call_view {
    ($trait:path) => {
        impl $trait for ToolCall {
            fn id(&self) -> &str {
                &self.id
            }
            fn name(&self) -> &str {
                &self.name
            }
            fn arguments_json(&self) -> &str {
                &self.arguments_json
            }
        }
    };
}
call_view!(chat_completions::ToolCallView);
call_view!(responses::ResponsesToolCallView);
call_view!(anthropic::ToolCallView);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReasoningKind {
    Raw,
    Summary,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Completion {
    pub response_id: String,
    pub finish_reason: String,
    pub input_tokens: i32,
    pub cached_input_tokens: i32,
    pub output_tokens: i32,
    pub assistant_text: String,
    pub tool_calls: Vec<ToolCall>,
    pub provider_id: String,
    pub provider_content: String,
}
impl Completion {
    pub fn into_message(self) -> Message {
        let mut message = Message::assistant(self.assistant_text, self.tool_calls);
        message.provider_id = self.provider_id;
        message.provider_content = self.provider_content;
        message
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    /// Source common events may carry text independently of their event kind.
    WithText {
        event: Box<Event>,
        text: String,
    },
    HttpRequestStarted,
    HttpResponseHeadersReceived,
    EncryptedReasoningReceived,
    TextDelta(String),
    ReasoningDelta {
        text: String,
        kind: ReasoningKind,
        part_id: String,
        completed: bool,
    },
    ToolCallDelta(ToolCall),
    Retry {
        attempt: i32,
        retry_after: Duration,
        reason: String,
    },
    Completed(Completion),
}
impl Event {
    pub fn with_text(self, text: impl Into<String>) -> Self {
        let event = match self {
            Self::WithText { event, .. } => event,
            event => Box::new(event),
        };
        Self::WithText {
            event,
            text: text.into(),
        }
    }
    pub fn base_event(&self) -> &Self {
        match self {
            Self::WithText { event, .. } => event.base_event(),
            event => event,
        }
    }
    pub fn text(&self) -> &str {
        match self {
            Self::WithText { text, .. }
            | Self::TextDelta(text)
            | Self::ReasoningDelta { text, .. } => text,
            Self::ToolCallDelta(call) => &call.arguments_json,
            Self::Retry { reason, .. } => reason,
            _ => "",
        }
    }
    pub fn is_visible_output(&self) -> bool {
        matches!(
            self.base_event(),
            Self::TextDelta(_)
                | Self::ReasoningDelta { .. }
                | Self::ToolCallDelta(_)
                | Self::Completed(_)
        )
    }
}

impl From<chat_completions::ToolCall> for ToolCall {
    fn from(call: chat_completions::ToolCall) -> Self {
        Self {
            id: call.id,
            name: call.name,
            arguments_json: call.arguments_json,
        }
    }
}
impl From<responses::ResponsesToolCall> for ToolCall {
    fn from(call: responses::ResponsesToolCall) -> Self {
        Self {
            id: call.id,
            name: call.name,
            arguments_json: call.arguments_json,
        }
    }
}
impl From<anthropic::ToolCall> for ToolCall {
    fn from(call: anthropic::ToolCall) -> Self {
        Self {
            id: call.id,
            name: call.name,
            arguments_json: call.arguments_json,
        }
    }
}
impl From<chat_completions::ChatCompletion> for Completion {
    fn from(completion: chat_completions::ChatCompletion) -> Self {
        let (provider_id, provider_content) = completion.provider_content.map_or_else(
            || (String::new(), String::new()),
            |replay| {
                // Chat Completions exposes its native envelope as typed fields;
                // strings are the only payload here, never opaque parsed JSON.
                #[derive(serde::Serialize)]
                #[serde(rename_all = "PascalCase")]
                struct NativeReplay {
                    version: i64,
                    protocol: String,
                    #[serde(skip_serializing_if = "Option::is_none")]
                    reasoning_content: Option<String>,
                    #[serde(skip_serializing_if = "Option::is_none")]
                    reasoning: Option<String>,
                }
                let body = serde_json::to_string(&NativeReplay {
                    version: replay.version,
                    protocol: replay.protocol,
                    reasoning_content: replay.reasoning_content,
                    reasoning: replay.reasoning,
                })
                .expect("native replay contains only JSON strings and integers");
                (replay.provider_id, body)
            },
        );
        Self {
            response_id: String::new(),
            finish_reason: completion.finish_reason,
            input_tokens: completion.input_tokens,
            cached_input_tokens: completion.cached_input_tokens,
            output_tokens: completion.output_tokens,
            assistant_text: completion.assistant_text,
            tool_calls: completion.tool_calls.into_iter().map(Into::into).collect(),
            provider_id,
            provider_content,
        }
    }
}
impl From<responses::ResponsesCompletion> for Completion {
    fn from(completion: responses::ResponsesCompletion) -> Self {
        Self {
            response_id: completion.response_id,
            finish_reason: completion.finish_reason,
            input_tokens: completion.input_tokens,
            cached_input_tokens: completion.cached_input_tokens,
            output_tokens: completion.output_tokens,
            assistant_text: completion.assistant_text,
            tool_calls: completion.tool_calls.into_iter().map(Into::into).collect(),
            provider_id: completion.provider_id,
            provider_content: completion.provider_content.unwrap_or_default(),
        }
    }
}
impl From<anthropic::CompletedMessage> for Completion {
    fn from(completion: anthropic::CompletedMessage) -> Self {
        Self {
            response_id: String::new(),
            finish_reason: completion.finish_reason,
            input_tokens: completion.input_tokens,
            cached_input_tokens: completion.cached_input_tokens,
            output_tokens: completion.output_tokens,
            assistant_text: completion.assistant_text,
            tool_calls: completion.tool_calls.into_iter().map(Into::into).collect(),
            provider_id: completion.provider_id,
            provider_content: completion.provider_content,
        }
    }
}
impl From<chat_completions::ChatEvent> for Event {
    fn from(event: chat_completions::ChatEvent) -> Self {
        match event {
            chat_completions::ChatEvent::RequestStarted => Self::HttpRequestStarted,
            chat_completions::ChatEvent::ResponseHeadersReceived => {
                Self::HttpResponseHeadersReceived
            }
            chat_completions::ChatEvent::TextDelta(text) => Self::TextDelta(text),
            chat_completions::ChatEvent::ReasoningDelta {
                text,
                kind: chat_completions::ReasoningKind::Raw,
            } => Self::ReasoningDelta {
                text,
                kind: ReasoningKind::Raw,
                part_id: String::new(),
                completed: false,
            },
            chat_completions::ChatEvent::ToolCallDelta(call) => Self::ToolCallDelta(call.into()),
            chat_completions::ChatEvent::Completed(completion) => {
                Self::Completed(completion.into())
            }
        }
    }
}
impl From<responses::ResponsesEvent> for Event {
    fn from(event: responses::ResponsesEvent) -> Self {
        match event {
            responses::ResponsesEvent::TextDelta(text) => Self::TextDelta(text),
            responses::ResponsesEvent::ReasoningDelta {
                text,
                kind,
                part_id,
                completed,
            } => Self::ReasoningDelta {
                text,
                kind: match kind {
                    responses::ResponsesReasoningKind::Raw => ReasoningKind::Raw,
                    responses::ResponsesReasoningKind::Summary => ReasoningKind::Summary,
                },
                part_id,
                completed,
            },
            responses::ResponsesEvent::EncryptedReasoningReceived => {
                Self::EncryptedReasoningReceived
            }
            responses::ResponsesEvent::ToolCallDelta(call) => Self::ToolCallDelta(call.into()),
            responses::ResponsesEvent::Completed(completion) => Self::Completed(completion.into()),
        }
    }
}
impl From<anthropic::AnthropicEvent> for Event {
    fn from(event: anthropic::AnthropicEvent) -> Self {
        match event {
            anthropic::AnthropicEvent::TextDelta { text } => Self::TextDelta(text),
            anthropic::AnthropicEvent::ReasoningDelta {
                text,
                part_id,
                completed,
            } => Self::ReasoningDelta {
                text,
                kind: ReasoningKind::Raw,
                part_id,
                completed,
            },
            anthropic::AnthropicEvent::ToolCallDelta(call) => Self::ToolCallDelta(call.into()),
            anthropic::AnthropicEvent::Completed(completion) => Self::Completed(completion.into()),
        }
    }
}
impl From<anthropic::AnthropicStreamEvent> for Event {
    fn from(event: anthropic::AnthropicStreamEvent) -> Self {
        match event {
            anthropic::AnthropicStreamEvent::HttpRequestStarted => Self::HttpRequestStarted,
            anthropic::AnthropicStreamEvent::HttpResponseHeadersReceived => {
                Self::HttpResponseHeadersReceived
            }
            anthropic::AnthropicStreamEvent::Message(event) => event.into(),
        }
    }
}
impl From<responses_websocket::SessionEvent> for Event {
    fn from(event: responses_websocket::SessionEvent) -> Self {
        match event {
            responses_websocket::SessionEvent::HttpRequestStarted => Self::HttpRequestStarted,
            responses_websocket::SessionEvent::HttpResponseHeadersReceived => {
                Self::HttpResponseHeadersReceived
            }
            responses_websocket::SessionEvent::Response(event) => event.into(),
            responses_websocket::SessionEvent::Retry { attempt, reason } => Self::Retry {
                attempt: attempt as i32,
                retry_after: Duration::ZERO,
                reason,
            },
        }
    }
}
