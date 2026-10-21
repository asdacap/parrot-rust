use parrot_provider::sessions::model::{
    Completion, Content, Event, Message, Reasoning, ReasoningKind, Request, Role, Tool, ToolCall,
};
use parrot_provider::{anthropic, chat_completions, responses, responses_websocket};
use std::time::Duration;

fn bodies(request: &Request, provider: &str) -> Vec<String> {
    vec![
        String::from_utf8(chat_completions::prepare(request, provider).unwrap().body).unwrap(),
        responses::prepare(request, provider)
            .unwrap()
            .body()
            .to_owned(),
        String::from_utf8(anthropic::encode(request, provider).unwrap().body).unwrap(),
    ]
}

#[test]
fn common_request_borrows_all_adapters_without_losing_raw_json() {
    let raw =
        r#"{"huge":123456789012345678901234567890,"exponent":1e999,"duplicate":1,"duplicate":2}"#;
    let request = Request {
        model: "model".into(),
        instructions: "instructions".into(),
        messages: vec![
            Message::user("first"),
            Message::assistant(
                "second",
                vec![ToolCall {
                    id: "call".into(),
                    name: "tool".into(),
                    arguments_json: raw.into(),
                }],
            ),
            Message::tool_result("call", "result"),
        ],
        tools: vec![Tool {
            name: "tool".into(),
            description: "description".into(),
            parameters_json: raw.into(),
        }],
        max_output_tokens: 123,
        reasoning: Some(Reasoning {
            effort: "high".into(),
            summary: "detailed".into(),
        }),
        provider_preferences: format!(r#"{{"custom":{raw}}}"#),
        include_router_metadata: true,
    };
    assert_eq!(request.clone(), request);
    assert_eq!(
        chat_completions::RequestView::provider_preferences(&request),
        request.provider_preferences
    );
    assert_eq!(
        responses::ResponsesRequestView::provider_preferences(&request),
        request.provider_preferences
    );
    assert_eq!(
        anthropic::RequestView::provider_preferences(&request),
        request.provider_preferences
    );
    assert_eq!(
        anthropic::RequestView::reasoning_summary(&request),
        Some("detailed")
    );
    assert_eq!(
        responses::ResponsesRequestView::reasoning_effort(&request),
        "high"
    );
    assert_eq!(
        chat_completions::RequestView::reasoning_effort(&request),
        Some("high")
    );
    assert_eq!(Message::system("system").role, Role::System);
    assert!(
        Content::image_file("/missing/image.png", "image/png", 1, 1)
            .read_image()
            .is_err()
    );
    for body in bodies(&request, "provider") {
        assert!(body.contains(raw), "{body}");
        for value in [
            "model",
            "instructions",
            "first",
            "second",
            "tool",
            "result",
            "123",
        ] {
            assert!(body.contains(value), "{value}: {body}");
        }
    }
    assert!(
        responses::prepare(&request, "provider")
            .unwrap()
            .include_router_metadata()
    );
}

#[test]
fn content_order_text_projection_and_images_stay_caller_owned_and_lazy() {
    let image = Content::image_link(
        "/missing/image.png",
        "https://example.test/image.png",
        "image/png",
        640,
        480,
    );
    let mut message = Message::new(
        Role::User,
        vec![Content::text("one"), image.clone(), Content::text("two")],
    );
    assert_eq!(message.content(), "onetwo");
    assert_eq!(message.contents()[1], image);
    assert_eq!(image.image_width(), 640);
    assert_eq!(image.image_height(), 480);
    assert!(image.read_image().is_err());
    let request = Request {
        model: "model".into(),
        messages: vec![message.clone()],
        ..Request::default()
    };
    for body in bodies(&request, "provider") {
        assert!(body.contains("https://example.test/image.png"));
        assert!(body.find("one").unwrap() < body.find("https://").unwrap());
        assert!(body.find("https://").unwrap() < body.find("two").unwrap());
    }
    message.set_contents(vec![Content::text("replacement")]);
    assert_eq!(message.content(), "replacement");
    assert_eq!(message.contents(), &[Content::text("replacement")]);
    let bytes = Content::Image {
        path: "/missing/image.png".into(),
        url: String::new(),
        media_type: "image/png".into(),
        width: 1,
        height: 1,
        bytes: Some(vec![0, 1, 2]),
    };
    assert_eq!(bytes.read_image().unwrap(), [0, 1, 2]);
    for body in bodies(
        &Request {
            model: "model".into(),
            messages: vec![Message::new(Role::User, vec![bytes])],
            ..Request::default()
        },
        "provider",
    ) {
        assert!(body.contains("AAEC"), "{body}");
    }
}

#[test]
fn responses_and_anthropic_native_replay_is_raw_and_provider_gated() {
    for (native, protocol) in [
        (
            r#"{"Version":1,"Protocol":"responses","Output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"native-only"}],"unknown":1e999}]}"#,
            "responses",
        ),
        (
            r#"[{"type":"tool_use","id":"native-only","name":"tool","input":{"unknown":1e999}}]"#,
            "anthropic",
        ),
    ] {
        let mut message = Message::assistant("ordinary-only", vec![]);
        message.provider_id = "origin".into();
        message.provider_content = native.into();
        let request = Request {
            model: "model".into(),
            messages: vec![message],
            ..Request::default()
        };
        let encode = |provider| {
            if protocol == "responses" {
                responses::prepare(&request, provider)
                    .unwrap()
                    .body()
                    .to_owned()
            } else {
                String::from_utf8(anthropic::encode(&request, provider).unwrap().body).unwrap()
            }
        };
        let matching = encode("origin");
        assert!(matching.contains("native-only"), "{matching}");
        assert!(matching.contains("1e999"), "{matching}");
        assert!(!matching.contains("ordinary-only"), "{matching}");
        let other = encode("other");
        assert!(other.contains("ordinary-only"), "{other}");
        assert!(!other.contains("native-only"), "{other}");
    }
}

#[test]
fn completion_mapping_retains_usage_arguments_and_opaque_replay() {
    let raw = r#"{"unknown":1e999,"duplicate":1,"duplicate":2}"#;
    let completion = Completion::from(responses::ResponsesCompletion {
        response_id: "response".into(),
        finish_reason: "stop".into(),
        assistant_text: "answer".into(),
        tool_calls: vec![responses::ResponsesToolCall {
            id: "call".into(),
            name: "name".into(),
            arguments_json: raw.into(),
        }],
        input_tokens: 10,
        cached_input_tokens: 3,
        output_tokens: 7,
        provider_id: "provider".into(),
        provider_content: Some(raw.into()),
    });
    assert_eq!(completion.response_id, "response");
    assert_eq!(
        (
            completion.input_tokens,
            completion.cached_input_tokens,
            completion.output_tokens
        ),
        (10, 3, 7)
    );
    assert_eq!(completion.finish_reason, "stop");
    let message = completion.into_message();
    assert_eq!(message.role, Role::Assistant);
    assert_eq!(message.content(), "answer");
    assert_eq!(message.tool_calls[0].arguments_json, raw);
    assert_eq!(message.provider_id, "provider");
    assert_eq!(message.provider_content, raw);
    let completion = Completion::from(anthropic::CompletedMessage {
        finish_reason: "end_turn".into(),
        input_tokens: 11,
        cached_input_tokens: 4,
        output_tokens: 8,
        assistant_text: "anthropic".into(),
        tool_calls: vec![anthropic::ToolCall {
            id: "a".into(),
            name: "b".into(),
            arguments_json: raw.into(),
        }],
        provider_id: "origin".into(),
        provider_content: raw.into(),
    });
    assert_eq!(
        (
            completion.input_tokens,
            completion.cached_input_tokens,
            completion.output_tokens
        ),
        (11, 4, 8)
    );
    assert_eq!(completion.assistant_text, "anthropic");
    assert_eq!(completion.tool_calls[0].arguments_json, raw);
    assert_eq!(completion.provider_content, raw);
}

#[test]
fn chat_completion_replay_round_trips_through_origin_only() {
    let completion = Completion::from(chat_completions::ChatCompletion {
        finish_reason: "stop".into(),
        input_tokens: 9,
        cached_input_tokens: 2,
        output_tokens: 4,
        assistant_text: "answer".into(),
        tool_calls: vec![],
        provider_content: Some(chat_completions::ChatReplay {
            provider_id: "origin".into(),
            version: 1,
            protocol: "chat-completions".into(),
            reasoning_content: Some("private-reasoning".into()),
            reasoning: None,
        }),
    });
    assert_eq!(
        (
            completion.input_tokens,
            completion.cached_input_tokens,
            completion.output_tokens
        ),
        (9, 2, 4)
    );
    let request = Request {
        model: "model".into(),
        messages: vec![completion.into_message()],
        ..Request::default()
    };
    let origin =
        String::from_utf8(chat_completions::prepare(&request, "origin").unwrap().body).unwrap();
    assert!(origin.contains("private-reasoning"), "{origin}");
    assert!(
        !String::from_utf8(chat_completions::prepare(&request, "other").unwrap().body)
            .unwrap()
            .contains("private-reasoning")
    );
}

#[test]
fn lifecycle_reasoning_tool_and_retry_events_keep_their_meaning() {
    assert_eq!(
        Event::from(responses_websocket::SessionEvent::HttpRequestStarted),
        Event::HttpRequestStarted
    );
    assert_eq!(
        Event::from(responses_websocket::SessionEvent::HttpResponseHeadersReceived),
        Event::HttpResponseHeadersReceived
    );
    assert_eq!(
        Event::from(chat_completions::ChatEvent::RequestStarted),
        Event::HttpRequestStarted
    );
    assert_eq!(
        Event::from(chat_completions::ChatEvent::ResponseHeadersReceived),
        Event::HttpResponseHeadersReceived
    );
    assert_eq!(
        Event::from(anthropic::AnthropicStreamEvent::HttpRequestStarted),
        Event::HttpRequestStarted
    );
    assert_eq!(
        Event::from(anthropic::AnthropicStreamEvent::HttpResponseHeadersReceived),
        Event::HttpResponseHeadersReceived
    );
    assert_eq!(
        Event::from(responses::ResponsesEvent::EncryptedReasoningReceived),
        Event::EncryptedReasoningReceived
    );
    for (kind, expected) in [
        (responses::ResponsesReasoningKind::Raw, ReasoningKind::Raw),
        (
            responses::ResponsesReasoningKind::Summary,
            ReasoningKind::Summary,
        ),
    ] {
        assert_eq!(
            Event::from(responses::ResponsesEvent::ReasoningDelta {
                text: "reason".into(),
                kind,
                part_id: "part".into(),
                completed: true
            }),
            Event::ReasoningDelta {
                text: "reason".into(),
                kind: expected,
                part_id: "part".into(),
                completed: true
            }
        );
    }
    assert_eq!(
        Event::from(chat_completions::ChatEvent::ReasoningDelta {
            text: "reason".into(),
            kind: chat_completions::ReasoningKind::Raw
        }),
        Event::ReasoningDelta {
            text: "reason".into(),
            kind: ReasoningKind::Raw,
            part_id: String::new(),
            completed: false
        }
    );
    assert_eq!(
        Event::from(anthropic::AnthropicEvent::ReasoningDelta {
            text: "reason".into(),
            part_id: "part".into(),
            completed: true
        }),
        Event::ReasoningDelta {
            text: "reason".into(),
            kind: ReasoningKind::Raw,
            part_id: "part".into(),
            completed: true
        }
    );
    assert_eq!(
        Event::from(responses_websocket::SessionEvent::Retry {
            attempt: 2,
            reason: "reconnect".into()
        }),
        Event::Retry {
            attempt: 2,
            retry_after: Duration::ZERO,
            reason: "reconnect".into()
        }
    );
    for event in [
        Event::HttpRequestStarted,
        Event::HttpResponseHeadersReceived,
        Event::EncryptedReasoningReceived,
        Event::Retry {
            attempt: 1,
            retry_after: Duration::from_secs(1),
            reason: "retry".into(),
        },
    ] {
        assert!(!event.is_visible_output());
    }
    for event in [
        Event::TextDelta(String::new()),
        Event::ReasoningDelta {
            text: String::new(),
            kind: ReasoningKind::Raw,
            part_id: String::new(),
            completed: false,
        },
        Event::ToolCallDelta(ToolCall::default()),
        Event::Completed(Completion::default()),
    ] {
        assert!(event.is_visible_output());
    }
}

#[test]
fn owning_deltas_preserve_accumulated_arguments_across_all_adapters() {
    let raw = r#"{"huge":1e999,"huge":2}"#;
    let expected = Event::ToolCallDelta(ToolCall {
        id: "id".into(),
        name: "name".into(),
        arguments_json: raw.into(),
    });
    let events = [
        Event::from(chat_completions::ChatEvent::ToolCallDelta(
            chat_completions::ToolCall {
                id: "id".into(),
                name: "name".into(),
                arguments_json: raw.into(),
            },
        )),
        Event::from(responses::ResponsesEvent::ToolCallDelta(
            responses::ResponsesToolCall {
                id: "id".into(),
                name: "name".into(),
                arguments_json: raw.into(),
            },
        )),
        Event::from(anthropic::AnthropicEvent::ToolCallDelta(
            anthropic::ToolCall {
                id: "id".into(),
                name: "name".into(),
                arguments_json: raw.into(),
            },
        )),
    ];
    for event in events {
        assert_eq!(event, expected);
    }
    for event in [
        Event::from(chat_completions::ChatEvent::TextDelta("text".into())),
        Event::from(responses::ResponsesEvent::TextDelta("text".into())),
        Event::from(anthropic::AnthropicStreamEvent::Message(
            anthropic::AnthropicEvent::TextDelta {
                text: "text".into(),
            },
        )),
        Event::from(responses_websocket::SessionEvent::Response(
            responses::ResponsesEvent::TextDelta("text".into()),
        )),
    ] {
        assert_eq!(event, Event::TextDelta("text".into()));
    }
}

#[test]
fn independent_common_text_payload_is_preserved_without_changing_kind_or_visibility() {
    for event in [
        Event::ToolCallDelta(ToolCall {
            arguments_json: "{}".into(),
            ..ToolCall::default()
        }),
        Event::Completed(Completion {
            input_tokens: 3,
            output_tokens: 5,
            ..Completion::default()
        }),
        Event::EncryptedReasoningReceived,
    ] {
        let visible = event.is_visible_output();
        let wrapped = event.clone().with_text("synthetic fragment");
        assert_eq!(wrapped.text(), "synthetic fragment");
        assert_eq!(wrapped.base_event(), &event);
        assert_eq!(wrapped.is_visible_output(), visible);
        let replaced = wrapped.with_text("replacement");
        assert_eq!(replaced.text(), "replacement");
        assert!(
            matches!(&replaced, Event::WithText { event, .. } if !matches!(event.as_ref(), Event::WithText { .. }))
        );
        assert_eq!(replaced.base_event(), &event);
    }
    assert_eq!(Event::TextDelta("text".into()).text(), "text");
    assert_eq!(
        Event::ToolCallDelta(ToolCall {
            arguments_json: "{\"a\":1}".into(),
            ..ToolCall::default()
        })
        .text(),
        "{\"a\":1}"
    );
    assert_eq!(
        Event::Retry {
            attempt: 1,
            retry_after: Duration::ZERO,
            reason: "retry".into()
        }
        .text(),
        "retry"
    );
}
