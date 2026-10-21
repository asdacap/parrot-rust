//! Stateful adapter for the OpenAI Responses wire protocol.
//!
//! This module owns Responses-specific request/history conversion and stream
//! state. Caller values enter through borrowed views; outputs are owning
//! Responses events and replay content, not duplicate shared neutral DTOs.
mod json;
mod request;
mod stream;
mod view;

pub use request::{PreparedResponsesRequest, ResponsesPrepareError, prepare};
pub use stream::{
    ResponsesCompletion, ResponsesError, ResponsesEvent, ResponsesEventStream, ResponsesParser,
    ResponsesReasoningKind, ResponsesStreamError, ResponsesToolCall,
};
pub use view::{
    ResponsesContentKind, ResponsesContentView, ResponsesMessageView, ResponsesRequestView,
    ResponsesRole, ResponsesToolCallView, ResponsesToolDefinitionView,
};
