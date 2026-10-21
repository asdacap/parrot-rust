//! Minimal borrowed caller views consumed by the Responses wire adapter.
use std::{io, path::Path};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesContentKind {
    Text,
    Image,
}

pub trait ResponsesContentView {
    fn kind(&self) -> ResponsesContentKind;
    fn text(&self) -> &str;
    fn image_path(&self) -> &Path;
    fn image_url(&self) -> &str;
    fn image_media_type(&self) -> &str;
    /// Reads image bytes only when this adapter encodes an inline image.
    fn read_image(&self) -> Result<Vec<u8>, io::Error>;
}

pub trait ResponsesToolCallView {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn arguments_json(&self) -> &str;
}

pub trait ResponsesMessageView {
    type Content: ResponsesContentView;
    type ToolCall: ResponsesToolCallView;

    fn role(&self) -> ResponsesRole;
    fn contents(&self) -> &[Self::Content];
    fn tool_calls(&self) -> &[Self::ToolCall];
    fn tool_call_id(&self) -> &str;
    fn content(&self) -> &str;
    fn provider_id(&self) -> &str;
    fn provider_content(&self) -> &str;
}

pub trait ResponsesToolDefinitionView {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// Raw JSON schema, not a round-tripped `Value`; unknown numeric literals
    /// and object member order remain under the caller's source representation.
    fn parameters_json(&self) -> &str;
}

pub trait ResponsesRequestView {
    type Message: ResponsesMessageView;
    type Tool: ResponsesToolDefinitionView;

    fn model(&self) -> &str;
    fn messages(&self) -> &[Self::Message];
    fn tools(&self) -> &[Self::Tool];
    fn instructions(&self) -> &str;
    fn max_output_tokens(&self) -> i32;
    fn reasoning_effort(&self) -> &str;
    fn reasoning_summary(&self) -> &str;
    /// Empty means unset; non-empty must be an opaque JSON object.
    fn provider_preferences(&self) -> &str;
    fn include_router_metadata(&self) -> bool;
}
