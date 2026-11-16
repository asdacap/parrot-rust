//! OpenAI-compatible Chat Completions wire encoding and SSE adaptation.

mod request;
mod stream;

pub use request::{PreparedRequest, RequestError, prepare};
pub use stream::{
    ChatCompletion, ChatEvent, ChatReplay, ChatStream, ChatStreamError, ParseState,
    ProviderFailure, ReasoningKind, ToolCall, ToolCallReplay, parse_data,
};
