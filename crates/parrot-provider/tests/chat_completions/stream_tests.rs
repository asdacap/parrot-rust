use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::{
    chat_completions::*,
    http_stream::{
        ByteStream, Endpoint, HttpError, HttpEventStream, HttpResponse, HttpTransport,
        ResponseByteCounter, SseEvent, StreamOptions,
    },
};
use std::{future::pending, sync::Arc, time::Duration};

fn feed(state: &mut ParseState, data: &str) -> Vec<ChatEvent> {
    parse_data(
        &SseEvent {
            event: String::new(),
            data: data.into(),
            id: String::new(),
        },
        state,
    )
    .unwrap()
}

#[test]
fn text_reasoning_finish_and_usage_are_separate_and_complete() {
    let mut state = ParseState::default();
    let mut events = feed(
        &mut state,
        r#"{"choices":[{"delta":{"content":"hello ","reasoning_content":"think"}}]}"#,
    );
    events.extend(feed(&mut state,r#"{"choices":[{"delta":{"content":"world","reasoning":"alternate"},"finish_reason":"stop"}],"usage":{"prompt_tokens":11,"prompt_tokens_details":{"cached_tokens":5},"completion_tokens":3}}"#));
    assert_eq!(events[0], ChatEvent::TextDelta("hello ".into()));
    assert_eq!(
        events[1],
        ChatEvent::ReasoningDelta {
            text: "think".into(),
            kind: ReasoningKind::Raw
        }
    );
    assert_eq!(events[2], ChatEvent::TextDelta("world".into()));
    assert_eq!(
        events[3],
        ChatEvent::ReasoningDelta {
            text: "alternate".into(),
            kind: ReasoningKind::Raw
        }
    );
    let final_state = state.completion("p");
    assert_eq!(final_state.assistant_text, "hello world");
    assert_eq!(final_state.finish_reason, "stop");
    assert_eq!(
        (
            final_state.input_tokens,
            final_state.cached_input_tokens,
            final_state.output_tokens
        ),
        (11, 5, 3)
    );
    assert_eq!(
        final_state
            .provider_content
            .as_ref()
            .unwrap()
            .reasoning_content
            .as_deref(),
        Some("think")
    );
    assert_eq!(
        final_state
            .provider_content
            .as_ref()
            .unwrap()
            .reasoning
            .as_deref(),
        Some("alternate")
    );
}

#[test]
fn null_and_empty_native_reasoning_do_not_emit_events_or_replay() {
    let mut state = ParseState::default();
    let events = feed(
        &mut state,
        r#"{"choices":[{"delta":{"reasoning_content":null,"reasoning":""}}]}"#,
    );
    assert!(events.is_empty());
    let events = feed(
        &mut state,
        r#"{"choices":[{"delta":{"content":"answer"},"finish_reason":"stop"}]}"#,
    );
    assert_eq!(events, vec![ChatEvent::TextDelta("answer".into())]);
    let completion = state.completion("test");
    assert!(completion.provider_content.is_none());
}

#[test]
fn reasoning_display_prefers_reasoning_content_but_retains_both_replay_fields() {
    let mut state = ParseState::default();
    let events = feed(
        &mut state,
        r#"{"choices":[{"delta":{"reasoning_content":"first","reasoning":"other"}}]}"#,
    );
    assert_eq!(
        events,
        vec![ChatEvent::ReasoningDelta {
            text: "first".into(),
            kind: ReasoningKind::Raw
        }]
    );
    feed(
        &mut state,
        r#"{"choices":[{"delta":{"reasoning_content":" second","reasoning":" field","content":"answer"},"finish_reason":"stop"}]}"#,
    );
    let replay = state.completion("provider").provider_content.unwrap();
    assert_eq!(replay.reasoning_content.as_deref(), Some("first second"));
    assert_eq!(replay.reasoning.as_deref(), Some("other field"));
}

#[test]
fn fragmented_interleaved_calls_keep_index_order_and_raw_partial_arguments() {
    let mut state = ParseState::default();
    let mut events = feed(
        &mut state,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_b","function":{"name":"beta","arguments":"{\"x\":"}},{"index":0,"id":"call_a","function":{"name":"alpha","arguments":"{\"command\":"}}]}}]}"#,
    );
    events.extend(feed(&mut state,r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"2}"}},{"index":0,"function":{"arguments":"\"echo hi\"}"}}]},"finish_reason":"tool_calls"}]}"#));
    assert!(events.is_empty());
    let toolcalls = state.completion("p").tool_calls;
    assert_eq!(
        toolcalls
            .iter()
            .map(|call| call.id.as_str())
            .collect::<Vec<_>>(),
        vec!["call_a", "call_b"]
    );
    assert_eq!(toolcalls[0].arguments_json, r#"{"command":"echo hi"}"#);
    assert_eq!(toolcalls[1].arguments_json, r#"{"x":2}"#);
}

#[test]
fn call_delta_emitted_only_for_observed_call_and_is_never_json_repaired() {
    let mut state = ParseState::default();
    feed(
        &mut state,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":4,"id":"partial","function":{"name":"go","arguments":"{\"x\":"}}]},"finish_reason":"tool_calls"}]}"#,
    );
    let finals = state.terminal_events("p");
    assert_eq!(finals.len(), 2);
    assert_eq!(
        finals[0],
        ChatEvent::ToolCallDelta(ToolCall {
            id: "partial".into(),
            name: "go".into(),
            arguments_json: "{\"x\":".into()
        })
    );
    let ChatEvent::Completed(done) = &finals[1] else {
        panic!()
    };
    assert_eq!(done.tool_calls.len(), 1);
    assert_eq!(done.tool_calls[0].arguments_json, "{\"x\":");
    let mut empty = ParseState::default();
    feed(
        &mut empty,
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
    );
    assert_eq!(empty.terminal_events("p").len(), 1);
    assert!(empty.completion("p").tool_calls.is_empty());
}

#[test]
fn finish_reason_mapping_and_usage_only_defaults_are_source_shaped() {
    let cases = [
        ("stop", "stop"),
        ("tool_calls", "tool_calls"),
        ("function_call", "tool_calls"),
        ("length", "length"),
        ("max_tokens", "length"),
        ("content_filter", "content_filter"),
        ("future", "incomplete"),
    ];
    for (input, expected) in cases {
        let mut state = ParseState::default();
        feed(
            &mut state,
            &format!(
                r#"{{"choices":[{{"finish_reason":"{input}","delta":{{}}}}],"usage":{{"prompt_tokens":7,"completion_tokens":2}}}}"#
            ),
        );
        let result = state.completion("p");
        assert_eq!(result.finish_reason, expected);
        assert_eq!(
            (
                result.input_tokens,
                result.cached_input_tokens,
                result.output_tokens
            ),
            (7, 0, 2)
        );
    }
}

#[test]
fn usage_values_replace_previous_values_and_bad_integer_fails() {
    let mut state = ParseState::default();
    feed(
        &mut state,
        r#"{"usage":{"prompt_tokens":10,"completion_tokens":4}}"#,
    );
    feed(
        &mut state,
        r#"{"usage":{"prompt_tokens":2,"prompt_tokens_details":{"cached_tokens":1},"completion_tokens":3}}"#,
    );
    assert_eq!(
        (
            state.completion("p").input_tokens,
            state.completion("p").cached_input_tokens,
            state.completion("p").output_tokens
        ),
        (2, 1, 3)
    );
    assert!(matches!(
        parse_data(
            &SseEvent {
                event: String::new(),
                id: String::new(),
                data: r#"{"usage":{"prompt_tokens":2147483648}}"#.into()
            },
            &mut state
        ),
        Err(ChatStreamError::InvalidInteger(_))
    ));
}

#[test]
fn structured_success_status_error_preserves_fields_body_and_numeric_code_lexeme() {
    let data = r#"{"error":{"type":"server_error","code":1e999,"message":"boom"}}"#;
    let mut state = ParseState::default();
    let error = parse_data(
        &SseEvent {
            event: String::new(),
            id: String::new(),
            data: data.into(),
        },
        &mut state,
    )
    .unwrap_err();
    let ChatStreamError::Provider(error) = error else {
        panic!("provider error expected")
    };
    assert_eq!(error.error_type, "server_error");
    assert_eq!(error.error_code, "1e999");
    assert_eq!(error.message, "boom");
    assert_eq!(error.response_body, data);

    let boolean_code = r#"{"error":{"type":"server_error","code":true,"message":"boom"}}"#;
    let error = parse_data(
        &SseEvent {
            event: String::new(),
            id: String::new(),
            data: boolean_code.into(),
        },
        &mut ParseState::default(),
    )
    .unwrap_err();
    let ChatStreamError::Provider(provider_error) = &error else {
        panic!("provider error expected")
    };
    assert_eq!(provider_error.error_code, "");
    assert_eq!(provider_error.message, "boom");
    assert_eq!(error.to_string(), "boom (server_error)");
}

#[test]
fn nonobject_records_are_ignored_and_invalid_json_fails() {
    assert!(
        matches!(parse_data(&SseEvent{event:String::new(),id:String::new(),data:"[]".into()},&mut ParseState::default()),Ok(events) if events.is_empty())
    );
    assert!(matches!(
        parse_data(
            &SseEvent {
                event: String::new(),
                id: String::new(),
                data: "{".into()
            },
            &mut ParseState::default()
        ),
        Err(ChatStreamError::InvalidJson(_))
    ));
}

#[test]
fn each_choice_is_processed_in_order_and_missing_index_uses_zero() {
    let mut state = ParseState::default();
    let events = feed(
        &mut state,
        r#"{"choices":[{"delta":{"content":"a"}},{"delta":{"content":"b","tool_calls":[{"function":{"arguments":"{}"}}]}}]}"#,
    );
    assert_eq!(
        events,
        vec![
            ChatEvent::TextDelta("a".into()),
            ChatEvent::TextDelta("b".into())
        ]
    );
    let calls = state.completion("p").tool_calls;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "");
}

struct Never;
impl Cancellation for Never {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(pending())
    }
}
struct Body {
    bytes: Vec<u8>,
    position: usize,
    chunk: usize,
}
impl ByteStream for Body {
    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        cancel: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(HttpError::Cancelled);
            }
            let n = buffer
                .len()
                .min(self.chunk)
                .min(self.bytes.len() - self.position);
            buffer[..n].copy_from_slice(&self.bytes[self.position..self.position + n]);
            self.position += n;
            Ok(n)
        })
    }
}
struct Fake {
    bytes: Vec<u8>,
    calls: std::sync::atomic::AtomicUsize,
}
impl HttpTransport for Fake {
    fn send<'a>(
        &'a self,
        _request: parrot_provider::http_stream::HttpRequest,
        _cancel: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(HttpResponse {
                status: 200,
                reason: "OK".into(),
                headers: vec![("x-codex-turn-state".into(), "turn-1".into())],
                body: Box::new(Body {
                    bytes: self.bytes.clone(),
                    position: 0,
                    chunk: 3,
                }),
            })
        })
    }
}
fn http(bytes: &str) -> (HttpEventStream, Arc<Fake>) {
    let fake = Arc::new(Fake {
        bytes: bytes.as_bytes().to_vec(),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let count = ResponseByteCounter::default();
    let stream = HttpEventStream::new(
        fake.clone(),
        Endpoint::parse("https://example.test/chat").unwrap(),
        b"{}".to_vec(),
        vec![],
        StreamOptions {
            header_timeout: Duration::ZERO,
            idle_timeout: Duration::ZERO,
            maximum_request_bytes: 10,
        },
        count,
    )
    .unwrap();
    (stream, fake)
}

#[tokio::test]
async fn owning_http_stream_preserves_lifecycle_order_fragmentation_bytes_and_terminal_events() {
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let (http, fake) = http(body);
    let mut stream = ChatStream::new(http, "chat");
    let never = Never;
    assert_eq!(
        stream.next(&never).await.unwrap(),
        Some(ChatEvent::RequestStarted)
    );
    assert_eq!(
        stream.next(&never).await.unwrap(),
        Some(ChatEvent::ResponseHeadersReceived)
    );
    assert_eq!(stream.turn_state(), Some("turn-1"));
    assert_eq!(
        stream.next(&never).await.unwrap(),
        Some(ChatEvent::TextDelta("hi".into()))
    );
    assert!(matches!(
        stream.next(&never).await.unwrap(),
        Some(ChatEvent::Completed(_))
    ));
    assert_eq!(stream.next(&never).await.unwrap(), None);
    assert_eq!(fake.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(stream.response_bytes(), body.len() as u64);
}

#[tokio::test]
async fn missing_finish_is_not_promoted_to_successful_completion() {
    let (http, _) = http(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"partial\",\"function\":{\"name\":\"run\",\"arguments\":\"{\\\"x\\\":\"}}]}}]}\n\n",
    );
    let mut stream = ChatStream::new(http, "chat");
    let never = Never;
    assert_eq!(
        stream.next(&never).await.unwrap(),
        Some(ChatEvent::RequestStarted)
    );
    assert_eq!(
        stream.next(&never).await.unwrap(),
        Some(ChatEvent::ResponseHeadersReceived)
    );
    assert!(matches!(
        stream.next(&never).await,
        Err(ChatStreamError::UnexpectedEof)
    ));
    assert_eq!(stream.next(&never).await.unwrap(), None);
}
