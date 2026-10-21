//! OpenAI-compatible Chat Completions wire encoding and SSE adaptation.

mod request;
mod stream;

pub use request::{
    ContentView, MessageRole, MessageView, PreparedRequest, RequestError, RequestView,
    ToolCallView, ToolView, prepare,
};
pub use stream::{
    ChatCompletion, ChatEvent, ChatReplay, ChatStream, ChatStreamError, ParseState,
    ProviderFailure, ReasoningKind, ToolCall, parse_data,
};
