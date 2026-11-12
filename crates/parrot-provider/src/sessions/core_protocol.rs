use super::{Completion, Content, Message, Request};
use parrot_core::FinishReason;

impl From<Request> for parrot_core::ProviderRequest {
    fn from(request: Request) -> Self {
        Self {
            model: request.model,
            instructions: request.instructions,
            messages: request.messages.into_iter().map(Into::into).collect(),
            tools: request.tools,
            max_output_tokens: request.max_output_tokens,
            reasoning: request.reasoning,
            provider_preferences: request.provider_preferences,
            include_router_metadata: request.include_router_metadata,
        }
    }
}

impl From<parrot_core::ProviderRequest> for Request {
    fn from(request: parrot_core::ProviderRequest) -> Self {
        Self {
            fast_mode: false,
            model: request.model,
            instructions: request.instructions,
            messages: request.messages.into_iter().map(Into::into).collect(),
            tools: request.tools,
            max_output_tokens: request.max_output_tokens,
            reasoning: request.reasoning,
            provider_preferences: request.provider_preferences,
            include_router_metadata: request.include_router_metadata,
        }
    }
}

impl From<Completion> for parrot_core::ProviderResponse {
    fn from(completion: Completion) -> Self {
        Self {
            response_id: completion.response_id,
            finish_reason: match completion.finish_reason.as_str() {
                "stop" => FinishReason::Stop,
                "length" => FinishReason::Length,
                "tool_calls" => FinishReason::ToolCalls,
                "content_filter" => FinishReason::ContentFilter,
                "incomplete" => FinishReason::Incomplete,
                "interrupted" => FinishReason::Interrupted,
                _ => FinishReason::Other(completion.finish_reason),
            },
            input_tokens: completion.input_tokens,
            cached_input_tokens: completion.cached_input_tokens,
            output_tokens: completion.output_tokens,
            assistant_text: completion.assistant_text,
            tool_calls: completion.tool_calls,
            provider_id: completion.provider_id,
            provider_content: completion.provider_content,
        }
    }
}

impl From<parrot_core::ProviderResponse> for Completion {
    fn from(response: parrot_core::ProviderResponse) -> Self {
        Self {
            response_id: response.response_id,
            finish_reason: match response.finish_reason {
                FinishReason::Stop => "stop".into(),
                FinishReason::Length => "length".into(),
                FinishReason::ToolCalls => "tool_calls".into(),
                FinishReason::ContentFilter => "content_filter".into(),
                FinishReason::Incomplete => "incomplete".into(),
                FinishReason::Interrupted => "interrupted".into(),
                FinishReason::Other(reason) => reason,
            },
            input_tokens: response.input_tokens,
            cached_input_tokens: response.cached_input_tokens,
            output_tokens: response.output_tokens,
            assistant_text: response.assistant_text,
            tool_calls: response.tool_calls,
            provider_id: response.provider_id,
            provider_content: response.provider_content,
        }
    }
}

impl From<Message> for parrot_core::Message {
    fn from(message: Message) -> Self {
        Self {
            role: message.role,
            contents: message.contents().iter().cloned().map(Into::into).collect(),
            tool_calls: message.tool_calls,
            tool_call_id: message.tool_call_id,
            provider_id: message.provider_id,
            provider_content: message.provider_content,
        }
    }
}

impl From<parrot_core::Message> for Message {
    fn from(message: parrot_core::Message) -> Self {
        let mut converted = Self::new(
            message.role,
            message.contents.into_iter().map(Into::into).collect(),
        );
        converted.tool_calls = message.tool_calls;
        converted.tool_call_id = message.tool_call_id;
        converted.provider_id = message.provider_id;
        converted.provider_content = message.provider_content;
        converted
    }
}

impl From<Content> for parrot_core::Content {
    fn from(content: Content) -> Self {
        match content {
            Content::Text(text) => Self::Text(text),
            Content::Image {
                path,
                url,
                media_type,
                width,
                height,
                bytes,
            } => Self::Image {
                path,
                url,
                media_type,
                width,
                height,
                bytes,
            },
        }
    }
}

impl From<parrot_core::Content> for Content {
    fn from(content: parrot_core::Content) -> Self {
        match content {
            parrot_core::Content::Text(text) => Self::Text(text),
            parrot_core::Content::Image {
                path,
                url,
                media_type,
                width,
                height,
                bytes,
            } => Self::Image {
                path,
                url,
                media_type,
                width,
                height,
                bytes,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::model::{Reasoning, Role, Tool, ToolCall};
    use parrot_core::{Content as CoreContent, Message as CoreMessage};

    #[test]
    fn request_round_trip_preserves_all_fields_and_ordered_values() {
        let mut native_message = Message::new(
            Role::Assistant,
            vec![
                Content::text("prefix "),
                Content::image_file("relative/image.png", "image/png", -4, 719),
                Content::text("suffix"),
                Content::Image {
                    path: "/opaque/image.bin".into(),
                    url: "https://example.test/image?x=1&y=2".into(),
                    media_type: "application/x-opaque".into(),
                    width: i32::MIN,
                    height: i32::MAX,
                    bytes: Some(vec![0, 1, 127, 255]),
                },
            ],
        );
        native_message.tool_calls = vec![ToolCall {
            id: "call id with spaces".into(),
            name: "tool alias".into(),
            arguments_json: " { \"unknown\" : [1.2300, -999999999999999999999] } ".into(),
        }];
        native_message.tool_call_id = "response-bound-call".into();
        native_message.provider_id = "future-provider-id".into();
        native_message.provider_content = "  {\"unknown\":\"opaque\\u003dstring\"}\n".into();
        let request = Request {
            fast_mode: false,
            model: "provider/model:variant ".into(),
            instructions: "system\nwith whitespace".into(),
            messages: vec![Message::user("first"), native_message],
            tools: vec![
                Tool {
                    name: "read".into(),
                    description: "description with whitespace  ".into(),
                    parameters_json: " { \"$schema\" : \"future\", \"limit\":1.2300 } ".into(),
                },
                Tool {
                    name: "another".into(),
                    description: "second".into(),
                    parameters_json: "[true,null,3]".into(),
                },
            ],
            max_output_tokens: -17,
            reasoning: Some(Reasoning {
                effort: "effort \n verbatim".into(),
                summary: "summary".into(),
            }),
            provider_preferences: "routing prefs {future=opaque}".into(),
            include_router_metadata: true,
        };
        let core: parrot_core::ProviderRequest = request.clone().into();
        assert_eq!(Request::from(core.clone()), request);
        assert_eq!(
            core.tools[0].parameters_json,
            request.tools[0].parameters_json
        );
        assert_eq!(
            core.messages[1].provider_content,
            request.messages[1].provider_content
        );
        assert_eq!(request.messages[1].content(), "prefix suffix");

        let no_reasoning = Request {
            model: "provider/no-reasoning".into(),
            reasoning: None,
            ..Request::default()
        };
        assert_eq!(
            Request::from(parrot_core::ProviderRequest::from(no_reasoning.clone())),
            no_reasoning
        );
    }

    #[test]
    fn completion_finish_reasons_are_typed_without_losing_native_values() {
        for (reason, expected) in [
            ("stop", FinishReason::Stop),
            ("length", FinishReason::Length),
            ("tool_calls", FinishReason::ToolCalls),
            ("content_filter", FinishReason::ContentFilter),
            ("incomplete", FinishReason::Incomplete),
            ("interrupted", FinishReason::Interrupted),
            ("", FinishReason::Other(String::new())),
            ("STOP", FinishReason::Other("STOP".into())),
            (
                "future reason\n\u{0000}",
                FinishReason::Other("future reason\n\u{0000}".into()),
            ),
        ] {
            let completion = Completion {
                finish_reason: reason.into(),
                ..Completion::default()
            };
            let core = parrot_core::ProviderResponse::from(completion.clone());
            assert_eq!(core.finish_reason, expected);
            assert_eq!(Completion::from(core), completion);
        }
    }

    #[test]
    fn completion_round_trip_preserves_signed_usage_and_unknown_provider_bytes() {
        let completion = Completion {
            response_id: "response\nunknown fields".into(),
            finish_reason: "unrecognized_finish_reason".into(),
            input_tokens: -13,
            cached_input_tokens: i32::MIN,
            output_tokens: i32::MAX,
            assistant_text: "assistant \u{0000} text".into(),
            tool_calls: vec![ToolCall {
                id: "tool-call".into(),
                name: "tool".into(),
                arguments_json: "{\"n\":123456789012345678901234567890.00100}".into(),
            }],
            provider_id: "native-v99".into(),
            provider_content: " {\"unknown\": [1.2300, \"raw\\\\text\"]} \n".into(),
        };
        let core: parrot_core::ProviderResponse = completion.clone().into();
        assert_eq!(Completion::from(core.clone()), completion);
        assert_eq!(core.input_tokens, -13);
        assert_eq!(core.cached_input_tokens, i32::MIN);
        assert_eq!(core.provider_content, completion.provider_content);
    }

    #[test]
    fn message_round_trip_preserves_role_contents_order_calls_and_cached_text() {
        let mut native = Message::new(
            Role::Assistant,
            vec![
                Content::text("prefix "),
                Content::image_file("relative/image.png", "image/png", -4, 719),
                Content::text("suffix"),
                Content::Image {
                    path: "/opaque/image.bin".into(),
                    url: "https://example.test/image?x=1&y=2".into(),
                    media_type: "application/x-opaque".into(),
                    width: i32::MIN,
                    height: i32::MAX,
                    bytes: Some(vec![0, 1, 127, 255]),
                },
            ],
        );
        native.tool_calls = vec![ToolCall {
            id: "call id with spaces".into(),
            name: "tool alias".into(),
            arguments_json: " { \"unknown\" : [1.2300, -999999999999999999999] } ".into(),
        }];
        native.tool_call_id = "response-bound-call".into();
        native.provider_id = "future-provider-id".into();
        native.provider_content = "  {\"unknown\":\"opaque\\u003dstring\"}\n".into();
        let original_projection = native.content().to_owned();
        let core: CoreMessage = native.clone().into();
        assert_eq!(core.contents.len(), 4);
        assert!(matches!(
            &core.contents[1],
            CoreContent::Image { bytes: None, .. }
        ));
        assert!(
            matches!(&core.contents[3], CoreContent::Image { bytes: Some(bytes), .. } if bytes == &[0, 1, 127, 255])
        );
        let restored = Message::from(core);
        assert_eq!(restored, native);
        assert_eq!(restored.content(), original_projection);
    }

    #[test]
    fn content_round_trip_preserves_both_image_byte_representations() {
        let contents = [
            Content::text("plain"),
            Content::image_file("path", "image/jpeg", -1, 2),
            Content::Image {
                path: String::new(),
                url: "https://e.test/raw?query=1".into(),
                media_type: "image/unknown".into(),
                width: i32::MAX,
                height: i32::MIN,
                bytes: Some(vec![0, 255, 10]),
            },
        ];
        for content in contents {
            assert_eq!(
                Content::from(parrot_core::Content::from(content.clone())),
                content
            );
        }
    }

    #[test]
    fn core_request_round_trip_does_not_retain_transport_fast_mode() {
        let request = Request {
            fast_mode: true,
            model: "model".into(),
            ..Request::default()
        };
        let restored = Request::from(parrot_core::ProviderRequest::from(request));
        assert!(!restored.fast_mode);
        assert_eq!(restored.model, "model");
    }
}
