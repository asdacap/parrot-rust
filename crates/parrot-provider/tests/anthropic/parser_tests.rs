use parrot_provider::anthropic::{
    AnthropicEvent, AnthropicParser, CompletedMessage, ParserError, ToolCall,
};

fn consume(parser: &mut AnthropicParser, record: &str) -> Vec<AnthropicEvent> {
    parser.consume(record).unwrap()
}

fn completed(parser: &mut AnthropicParser) -> CompletedMessage {
    consume(parser, r#"{"type":"message_stop"}"#);
    let mut events = parser.finish().unwrap();
    let AnthropicEvent::Completed(message) = events.pop().unwrap() else {
        panic!("completion must be last")
    };
    message
}

#[test]
fn text_usage_and_completion_lifecycle() {
    let mut parser = AnthropicParser::new("anthropic-test");
    assert!(consume(&mut parser, r#"{"type":"message_start","message":{"id":"m","usage":{"input_tokens":7,"cache_read_input_tokens":3,"cache_creation_input_tokens":5,"output_tokens":1}}}"#).is_empty());
    assert_eq!(
        consume(
            &mut parser,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"hello"}}"#
        ),
        vec![AnthropicEvent::TextDelta {
            text: "hello".into()
        }]
    );
    assert_eq!(
        consume(
            &mut parser,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"!"}}"#
        ),
        vec![AnthropicEvent::TextDelta { text: "!".into() }]
    );
    consume(
        &mut parser,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":-1,"cache_read_input_tokens":"10","cache_creation_input_tokens":2.0,"output_tokens":9}}"#,
    );
    let message = completed(&mut parser);
    assert_eq!(message.finish_reason, "stop");
    assert_eq!(
        (
            message.input_tokens,
            message.cached_input_tokens,
            message.output_tokens
        ),
        (15, 3, 9)
    );
    assert_eq!(message.assistant_text, "hello!");
    assert_eq!(message.provider_id, "anthropic-test");
    assert_eq!(
        message.provider_content,
        r#"[{"type":"text","text":"hello!"}]"#
    );
    assert!(parser.is_stopped());
    assert!(
        parser
            .consume("invalid JSON after stop")
            .unwrap()
            .is_empty()
    );
    assert!(parser.finish().unwrap().is_empty());
}

#[test]
fn thinking_signatures_empty_completion_and_redacted_content() {
    let mut parser = AnthropicParser::new("p");
    consume(
        &mut parser,
        r#"{"type":"message_start","message":{"id":"msg"}}"#,
    );
    assert!(consume(&mut parser, r#"{"type":"content_block_start","index":2,"content_block":{"type":"thinking","signature":"a"}}"#).is_empty());
    assert_eq!(
        consume(&mut parser, r#"{"type":"content_block_stop","index":2}"#),
        vec![AnthropicEvent::ReasoningDelta {
            text: String::new(),
            part_id: "msg:2".into(),
            completed: true
        }]
    );
    assert_eq!(
        consume(
            &mut parser,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"thinking_delta","thinking":"reason"}}"#
        ),
        vec![AnthropicEvent::ReasoningDelta {
            text: "reason".into(),
            part_id: "msg:2".into(),
            completed: false
        }]
    );
    consume(
        &mut parser,
        r#"{"type":"content_block_delta","index":2,"delta":{"type":"signature_delta","signature":"b"}}"#,
    );
    consume(
        &mut parser,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"hidden"}}"#,
    );
    let message = completed(&mut parser);
    assert_eq!(message.assistant_text, "");
    assert_eq!(
        message.provider_content,
        r#"[{"type":"redacted_thinking","data":"hidden"},{"type":"thinking","thinking":"reason","signature":"ab"}]"#
    );
}

#[test]
fn blocks_replace_but_assistant_text_keeps_already_delivered_text() {
    let mut parser = AnthropicParser::new("p");
    consume(
        &mut parser,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"old"}}"#,
    );
    consume(
        &mut parser,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"unknown","text":"ignored"}}"#,
    );
    consume(
        &mut parser,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"mismatch"}}"#,
    );
    consume(
        &mut parser,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"new"}}"#,
    );
    consume(
        &mut parser,
        r#"{"type":"content_block_start","index":4,"content_block":{"type":"text"}}"#,
    );
    let message = completed(&mut parser);
    assert_eq!(message.assistant_text, "oldnew");
    assert_eq!(
        message.provider_content,
        r#"[{"type":"text","text":"new"}]"#
    );
}

#[test]
fn deltas_follow_existing_block_not_declared_kind() {
    let mut parser = AnthropicParser::new("p");
    assert!(consume(&mut parser, r#"{"type":"content_block_delta","index":8,"delta":{"type":"text_delta","text":"lost"}}"#).is_empty());
    consume(
        &mut parser,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"a"}}"#,
    );
    assert_eq!(
        consume(
            &mut parser,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"b"}}"#
        ),
        vec![AnthropicEvent::TextDelta { text: "b".into() }]
    );
    let message = completed(&mut parser);
    assert_eq!(message.assistant_text, "b");
    assert_eq!(
        message.provider_content,
        r#"[{"type":"thinking","thinking":"ab","signature":""}]"#
    );
}

#[test]
fn tool_calls_are_deferred_and_sorted_and_keep_raw_arguments() {
    let mut parser = AnthropicParser::new("p");
    consume(
        &mut parser,
        r#"{"type":"content_block_start","index":9,"content_block":{"type":"tool_use","id":"nine","name":"last","input":{ "big": 1234567890123456789012345678901234567890, "huge":1e999 }}}"#,
    );
    consume(
        &mut parser,
        r#"{"type":"content_block_start","index":-2,"content_block":{"type":"tool_use","id":"first","name":"bad","input":{"initial":true}}}"#,
    );
    assert!(consume(&mut parser, r#"{"type":"content_block_delta","index":-2,"delta":{"type":"input_json_delta","partial_json":"not json"}}"#).is_empty());
    consume(&mut parser, r#"{"type":"message_stop"}"#);
    let events = parser.finish().unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(
        events[0],
        AnthropicEvent::ToolCallDelta(ToolCall {
            id: "first".into(),
            name: "bad".into(),
            arguments_json: "not json".into()
        })
    );
    let AnthropicEvent::ToolCallDelta(second) = &events[1] else {
        panic!("expected second tool")
    };
    assert_eq!(
        second.arguments_json,
        r#"{ "big": 1234567890123456789012345678901234567890, "huge":1e999 }"#
    );
    let AnthropicEvent::Completed(message) = &events[2] else {
        panic!("expected completion")
    };
    assert_eq!(message.tool_calls[1], *second);
    assert_eq!(
        message.provider_content,
        r#"[{"type":"tool_use","id":"first","name":"bad","input":{}},{"type":"tool_use","id":"nine","name":"last","input":{"big":1234567890123456789012345678901234567890,"huge":1e999}}]"#
    );
}

#[test]
fn nonobject_tool_inputs_fall_back_only_for_provider_content() {
    for input in ["null", "[]", "123", "\"text\""] {
        let mut parser = AnthropicParser::new("p");
        consume(
            &mut parser,
            &format!(
                r#"{{"type":"content_block_start","content_block":{{"type":"tool_use","input":{input}}}}}"#
            ),
        );
        let message = completed(&mut parser);
        assert_eq!(message.tool_calls[0].arguments_json, input);
        assert_eq!(
            message.provider_content,
            r#"[{"type":"tool_use","id":"","name":"","input":{}}]"#
        );
    }
}

#[test]
fn stop_reason_mapping_and_tolerant_fields() {
    for (reason, expected) in [
        ("end_turn", "stop"),
        ("stop_sequence", "stop"),
        ("tool_use", "tool_calls"),
        ("max_tokens", "length"),
        ("model_context_window_exceeded", "length"),
        ("refusal", "content_filter"),
        ("new_reason", "incomplete"),
    ] {
        let mut parser = AnthropicParser::new("p");
        for ignored in [
            "null",
            "[]",
            "true",
            "42",
            "\"ignored\"",
            r#"{"type":3}"#,
            r#"{"type":"content_block_start","content_block":[]}"#,
        ] {
            assert!(consume(&mut parser, ignored).is_empty());
        }
        consume(
            &mut parser,
            &format!(r#"{{"type":"message_delta","delta":{{"stop_reason":"{reason}"}}}}"#),
        );
        consume(
            &mut parser,
            r#"{"type":"message_delta","delta":{"stop_reason":null}}"#,
        );
        assert_eq!(completed(&mut parser).finish_reason, expected);
    }
}

#[test]
fn indices_and_usage_require_int32_and_usage_is_cumulative() {
    let mut parser = AnthropicParser::new("p");
    consume(
        &mut parser,
        r#"{"type":"message_start","message":{"usage":{"input_tokens":2,"cache_read_input_tokens":4,"cache_creation_input_tokens":6,"output_tokens":8}}}"#,
    );
    consume(
        &mut parser,
        r#"{"type":"message_delta","usage":{"input_tokens":0,"cache_read_input_tokens":2147483648,"cache_creation_input_tokens":false,"output_tokens":3}}"#,
    );
    consume(
        &mut parser,
        r#"{"type":"content_block_start","index":1.0,"content_block":{"type":"thinking"}}"#,
    );
    assert_eq!(
        consume(
            &mut parser,
            r#"{"type":"content_block_stop","index":"wrong"}"#
        ),
        vec![AnthropicEvent::ReasoningDelta {
            text: "".into(),
            part_id: ":0".into(),
            completed: true
        }]
    );
    let message = completed(&mut parser);
    assert_eq!(
        (
            message.input_tokens,
            message.cached_input_tokens,
            message.output_tokens
        ),
        (10, 4, 3)
    );
}

#[test]
fn eof_and_json_errors_do_not_generate_completion() {
    let mut parser = AnthropicParser::new("p");
    assert_eq!(
        consume(
            &mut parser,
            r#"{"type":"content_block_start","content_block":{"type":"text","text":"delivered"}}"#
        )
        .len(),
        1
    );
    assert!(matches!(parser.consume("{"), Err(ParserError::Json(_))));
    assert_eq!(
        parser.finish(),
        Err(ParserError::Wire(
            "anthropic: unexpected provider EOF (stream ended without message_stop)".into()
        ))
    );
}

#[test]
fn structured_errors_preserve_body_and_bound_utf8() {
    let mut parser = AnthropicParser::new("p");
    let body =
        r#"{"type":"error","error":{"type":"overloaded_error","message":"busy","code":"ignored"}}"#;
    assert_eq!(
        parser.consume(body),
        Err(ParserError::ProviderResponse {
            error_type: "overloaded_error".into(),
            error_code: "".into(),
            detail: "busy".into(),
            response_body: body.into()
        })
    );
    let body = format!(r#"{{"type":"error","message":"{}"}}"#, "猫".repeat(30000));
    let Err(ParserError::ProviderResponse {
        response_body,
        error_type,
        ..
    }) = parser.consume(&body)
    else {
        panic!("expected provider error")
    };
    assert_eq!(error_type, "error");
    assert!(response_body.len() <= 64 << 10);
    assert!(response_body.ends_with("\n… [truncated]"));
    assert!(body.starts_with(response_body.trim_end_matches("\n… [truncated]")));
}

#[test]
fn selected_lone_surrogates_report_native_string_errors() {
    let high = "Cannot read incomplete UTF-16 JSON text as string with missing low surrogate.";
    let low = "Cannot read invalid UTF-16 JSON text as string. Invalid surrogate value: '0xDC00'.";
    for (record, expected) in [
        (r#"{"type":"\uD800"}"#, high),
        (r#"{"type":"\uDC00"}"#, low),
        (
            r#"{"type":"message_start","message":{"id":"\uD800"}}"#,
            high,
        ),
        (
            r#"{"type":"content_block_start","content_block":{"type":"\uDC00"}}"#,
            low,
        ),
        (
            r#"{"type":"content_block_start","content_block":{"type":"text","text":"\uD800"}}"#,
            high,
        ),
        (
            r#"{"type":"content_block_start","content_block":{"type":"thinking","signature":"\uDC00","thinking":"\uD800"}}"#,
            low,
        ),
        (
            r#"{"type":"content_block_start","content_block":{"type":"thinking","thinking":"\uD800"}}"#,
            high,
        ),
        (
            r#"{"type":"content_block_start","content_block":{"type":"redacted_thinking","data":"\uDC00"}}"#,
            low,
        ),
        (
            r#"{"type":"content_block_start","content_block":{"type":"tool_use","id":"\uD800","name":"\uDC00"}}"#,
            high,
        ),
        (
            r#"{"type":"content_block_start","content_block":{"type":"tool_use","name":"\uDC00"}}"#,
            low,
        ),
        (
            r#"{"type":"message_delta","delta":{"stop_reason":"\uD800"}}"#,
            high,
        ),
        (
            r#"{"type":"error","error":{"type":"\uDC00","message":"\uD800"}}"#,
            low,
        ),
        (r#"{"type":"error","error":{"message":"\uD800"}}"#, high),
    ] {
        let mut parser = AnthropicParser::new("p");
        let error = parser.consume(record).unwrap_err();
        assert_eq!(
            error,
            ParserError::InvalidJsonString(expected.into()),
            "{record}"
        );
        assert_eq!(error.to_string(), expected);
    }
}

#[test]
fn selected_delta_strings_propagate_after_previous_delivery() {
    let expected = "Cannot read incomplete UTF-16 JSON text as string with missing low surrogate.";
    for delta in [
        r#"{"type":"\uD800"}"#,
        r#"{"type":"text_delta","text":"\uD800"}"#,
        r#"{"type":"thinking_delta","thinking":"\uD800"}"#,
        r#"{"type":"signature_delta","signature":"\uD800"}"#,
        r#"{"type":"input_json_delta","partial_json":"\uD800"}"#,
    ] {
        let mut parser = AnthropicParser::new("p");
        assert_eq!(
            consume(
                &mut parser,
                r#"{"type":"content_block_start","content_block":{"type":"text","text":"prior"}}"#
            ),
            vec![AnthropicEvent::TextDelta {
                text: "prior".into()
            }]
        );
        let record = format!(r#"{{"type":"content_block_delta","delta":{delta}}}"#);
        assert_eq!(
            parser.consume(&record),
            Err(ParserError::InvalidJsonString(expected.into()))
        );
    }
}

#[test]
fn unused_lone_surrogates_remain_opaque() {
    let mut parser = AnthropicParser::new("p");
    for record in [
        r#"{"type":"ping","unused":"\uD800"}"#,
        r#"{"type":"content_block_start","content_block":{"type":"unknown","text":"\uD800","id":"\uDC00"}}"#,
        r#"{"type":"content_block_start","content_block":{"type":"text","id":"\uD800","name":"\uDC00","signature":"\uD800","data":"\uDC00"}}"#,
        r#"{"type":"content_block_delta","index":99,"delta":{"type":"\uD800","text":"\uDC00"}}"#,
        r#"{"type":"content_block_delta","index":99,"delta":{"type":"text_delta","text":"\uD800"}}"#,
        r#"{"type":"content_block_delta","delta":{"type":"unknown","text":"\uD800"}}"#,
        r#"{"type":"message_start","message":{"usage":{"input_tokens":"\uD800"},"unused":"\uDC00"}}"#,
    ] {
        assert!(consume(&mut parser, record).is_empty(), "{record}");
    }
    assert_eq!(completed(&mut parser).provider_content, "[]");
}

#[test]
fn valid_surrogate_pairs_and_escaped_backslashes_decode_normally() {
    let mut parser = AnthropicParser::new("p");
    assert_eq!(
        consume(
            &mut parser,
            r#"{"type":"content_block_start","content_block":{"type":"text","text":"\uD83D\uDE00\\uD800"}}"#
        ),
        vec![AnthropicEvent::TextDelta {
            text: "😀\\uD800".into()
        }]
    );
    assert_eq!(completed(&mut parser).assistant_text, "😀\\uD800");
}
