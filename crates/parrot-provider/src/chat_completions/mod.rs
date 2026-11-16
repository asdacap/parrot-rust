//! OpenAI-compatible Chat Completions wire encoding and SSE adaptation.

mod request;
mod stream;

pub use request::{PreparedRequest, RequestError, prepare, prepare_batch_body};
pub use stream::{
    ChatCompletion, ChatEvent, ChatReplay, ChatStream, ChatStreamError, ParseState,
    ProviderFailure, ReasoningKind, ToolCall, ToolCallReplay, decode_completion_body, parse_data,
};
