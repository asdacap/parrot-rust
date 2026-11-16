//! Stateful adapter for the OpenAI Responses wire protocol.
//!
//! This module owns Responses-specific request/history conversion and stream
//! state. Caller values enter through borrowed views; outputs are owning
//! Responses events and replay content, not duplicate shared neutral DTOs.
mod request;
mod stream;

pub use request::{PreparedResponsesRequest, ResponsesPrepareError, prepare};
pub use stream::{
    ResponsesCompletion, ResponsesError, ResponsesEvent, ResponsesParser, ResponsesReasoningKind,
    ResponsesToolCall,
};
