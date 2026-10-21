//! Anthropic Messages request conversion and owning streaming adaptation.
//!
//! Caller-owned request views avoid duplicate serialized neutral models. Native
//! content blocks remain opaque outside this adapter and replay only to their
//! originating provider. Diagnostics/retry orchestration belongs to sessions.
mod encoder;
mod parser;
mod provider;
mod raw_object;
mod replay;
mod view;
mod wire_json;

pub use encoder::{EncodeError, PreparedRequest, encode};
pub use parser::{AnthropicEvent, AnthropicParser, CompletedMessage, ParserError, ToolCall};
pub use provider::{
    AnthropicOptions, AnthropicProvider, AnthropicStream, AnthropicStreamEvent, ApiKeySource,
    ProviderError,
};
pub use view::{
    ContentKind, ContentView, MessageView, RequestView, Role, ToolCallView, ToolDefinitionView,
};
