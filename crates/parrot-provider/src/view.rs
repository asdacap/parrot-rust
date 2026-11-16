//! Borrowed, adapter-local views into caller-owned request history.
//! Image bytes remain lazy until request encoding needs inline image data.
//! All three wire adapters (Anthropic Messages, Chat Completions, Responses)
//! consume this one family; unused members stay untouched by each adapter.

use parrot_core::Role;

pub trait RequestView {
    type Message: MessageView;
    type Tool: ToolView;

    fn model(&self) -> &str;
    fn max_output_tokens(&self) -> i32;
    fn instructions(&self) -> &str;
    fn messages(&self) -> &[Self::Message];
    fn tools(&self) -> &[Self::Tool];
    fn reasoning_effort(&self) -> Option<&str>;
    /// Empty means unset.
    fn reasoning_summary(&self) -> &str {
        ""
    }
    /// Empty means unset; non-empty must be an opaque JSON object.
    fn provider_preferences(&self) -> &str {
        ""
    }
    /// Empty means standard routing; otherwise request this inference tier.
    fn service_tier(&self) -> &str {
        ""
    }
    fn include_router_metadata(&self) -> bool {
        false
    }
}

pub trait MessageView {
    type Content: ContentView;
    type ToolCall: ToolCallView;

    fn role(&self) -> Role;
    /// The source's concatenated text projection of the message contents.
    fn text(&self) -> &str {
        ""
    }
    fn contents(&self) -> &[Self::Content];
    fn tool_calls(&self) -> &[Self::ToolCall];
    fn tool_call_id(&self) -> &str;
    fn provider_id(&self) -> &str;
    fn provider_content(&self) -> &str;
}

pub trait ContentView {
    fn is_text(&self) -> bool;
    fn text(&self) -> &str;
    fn image_url(&self) -> &str;
    fn media_type(&self) -> &str;
    /// Reads image bytes only when this adapter encodes an inline image.
    fn read_image(&self) -> std::io::Result<Vec<u8>>;
}

pub trait ToolView {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// Raw JSON schema, not a round-tripped `Value`; unknown numeric literals
    /// and object member order remain under the caller's source representation.
    fn parameters_json(&self) -> &str;
}

pub trait ToolCallView {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn arguments_json(&self) -> &str;
}
