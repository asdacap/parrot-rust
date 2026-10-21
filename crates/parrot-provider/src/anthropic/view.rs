//! Adapter-local borrowed views into caller-owned request history.
//! Image bytes remain lazy until request encoding needs inline image data.

pub trait RequestView {
    type Message: MessageView;
    type Tool: ToolDefinitionView;

    fn model(&self) -> &str;
    fn max_output_tokens(&self) -> i32;
    fn instructions(&self) -> &str;
    fn messages(&self) -> &[Self::Message];
    fn tools(&self) -> &[Self::Tool];
    fn reasoning_effort(&self) -> Option<&str>;
    fn reasoning_summary(&self) -> Option<&str>;
    fn provider_preferences(&self) -> &str;
    fn include_router_metadata(&self) -> bool;
}

pub trait MessageView {
    type Content: ContentView;
    type ToolCall: ToolCallView;

    fn role(&self) -> Role;
    fn contents(&self) -> &[Self::Content];
    fn tool_calls(&self) -> &[Self::ToolCall];
    fn tool_call_id(&self) -> &str;
    fn provider_id(&self) -> &str;
    fn provider_content(&self) -> &str;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

pub trait ContentView {
    fn kind(&self) -> ContentKind;
    fn text(&self) -> &str;
    fn image_path(&self) -> &str;
    fn image_url(&self) -> &str;
    fn media_type(&self) -> &str;
    fn image_width(&self) -> i32;
    fn image_height(&self) -> i32;
    fn read_image(&self) -> std::io::Result<Vec<u8>>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentKind {
    Text,
    Image,
}

pub trait ToolDefinitionView {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_json(&self) -> &str;
}

pub trait ToolCallView {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn arguments_json(&self) -> &str;
}
