use super::projection::observe_stream_async;
use serde_json::{Value, json};
use std::fmt::Write;

#[test]
fn stream_regressions_preserve_event_order_content_and_error_scope() {
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/stream/cases.json")).unwrap();
    assert_eq!(cases.len(), 58);
    for case in cases {
        let id = case["input"]["case_id"].as_str().unwrap();
        let (events, exception) = observe_stream_async(&case["input"]);
        let expected = case["events"].as_array().unwrap();
        assert_eq!(events.len(), expected.len(), "{id}: event count");
        for (event, expected) in events.iter().zip(expected) {
            for (field, value) in expected.as_object().unwrap() {
                if field == "Output" {
                    assert_eq!(provider_output(event), *value, "{id}: provider output");
                } else {
                    assert_eq!(event[field], *value, "{id}: {field}");
                }
            }
        }
        assert_eq!(
            exception.is_none(),
            case["error"].is_null(),
            "{id}: error scope"
        );
        if let Some(exception) = exception {
            assert_eq!(exception, case["error"], "{id}");
        }
    }
}

fn observe_records(records: &[Value]) -> (Vec<Value>, Option<Value>) {
    let mut sse = String::new();
    for record in records {
        write!(sse, "data: {record}\n\n").unwrap();
    }
    observe_stream_async(&json!({"sse": sse}))
}

fn completed(output: Value) -> Value {
    json!({"type":"response.completed", "response":{"id":"r1", "output":output}})
}

fn provider_output(event: &Value) -> Value {
    if event["ProviderContent"]["Content"] == "" {
        assert_eq!(event["ProviderContent"]["ProviderId"], "");
        return Value::Null;
    }
    assert_eq!(event["ProviderContent"]["ProviderId"], "oracle-provider");
    let content: Value =
        serde_json::from_str(event["ProviderContent"]["Content"].as_str().unwrap()).unwrap();
    assert_eq!(content["Version"], 1);
    assert_eq!(content["Protocol"], "responses");
    content["Output"].clone()
}

#[test]
fn nonobject_items_are_ignored_without_poisoning_completion() {
    for item in [
        json!(null),
        json!(false),
        json!(0),
        json!("x"),
        json!([]),
        json!([{}]),
    ] {
        for kind in ["response.output_item.added", "response.output_item.done"] {
            let (events, exception) = observe_records(&[
                json!({"type":kind, "output_index":0, "item":item}),
                completed(json!([])),
            ]);
            assert!(exception.is_none(), "{kind}: {item}: {exception:?}");
            assert_eq!(events.len(), 1, "{kind}: {item}");
            assert_eq!(events[0]["Kind"], "Completed");
            assert_eq!(events[0]["FinishReason"], "stop");
            assert_eq!(events[0]["AssistantText"], "");
            assert_eq!(events[0]["ToolCalls"], json!([]));
            assert_eq!(provider_output(&events[0]), Value::Null);
        }
    }
}

#[test]
fn terminal_nonobjects_preserve_following_items_and_reasoning_order() {
    let message = json!({"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"kept"}]});
    let (events, exception) = observe_records(&[completed(json!([null, message]))]);
    assert!(exception.is_none());
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["Kind"], "Completed");
    assert_eq!(events[0]["AssistantText"], "");
    assert_eq!(provider_output(&events[0]), json!([message]));

    let reasoning =
        json!({"type":"reasoning", "encrypted_content":"cipher", "summary":[], "id":"r1"});
    let without_id = json!({"type":"reasoning", "encrypted_content":"cipher", "summary":[]});
    for (records, expected_output) in [
        (
            vec![completed(json!([false, reasoning]))],
            json!([reasoning]),
        ),
        (
            vec![
                json!({"type":"response.output_item.done", "output_index":0, "item":null}),
                json!({"type":"response.output_item.done", "output_index":0, "item":reasoning}),
                completed(json!([])),
            ],
            json!([reasoning]),
        ),
        (
            vec![
                json!({"type":"response.output_item.done", "output_index":1, "item":without_id}),
                completed(json!([null, without_id])),
            ],
            json!([without_id]),
        ),
    ] {
        let (events, exception) = observe_records(&records);
        assert!(exception.is_none(), "{exception:?}");
        assert_eq!(events.len(), 2, "reasoning is emitted only once");
        assert_eq!(events[0]["Kind"], "EncryptedReasoningReceived");
        assert_eq!(events[0]["IsVisibleOutput"], false);
        assert_eq!(events[1]["Kind"], "Completed");
        assert_eq!(events[1]["FinishReason"], "stop");
        assert_eq!(provider_output(&events[1]), expected_output);
    }
}

fn error_body_prefix(failed: bool) -> &'static str {
    if failed {
        r#"{"type":"response.failed","response":{"id":"r1","error":{"type":"server_error","code":"overloaded","message":"Try later"}},"padding":""#
    } else {
        r#"{"type":"error","error":{"type":"server_error","code":"overloaded","message":"Try later"},"padding":""#
    }
}

fn observe_error_body(body: &str) -> Value {
    let (events, exception) = observe_stream_async(&json!({"sse":format!("data: {body}\n\n")}));
    assert!(events.is_empty());
    let exception = exception.unwrap();
    assert_eq!(
        exception["type"],
        "Parrot.Llm.Wire.ProviderResponseException"
    );
    assert_eq!(
        exception["message"],
        "Try later (server_error) [overloaded]"
    );
    assert_eq!(exception["error_type"], "server_error");
    assert_eq!(exception["error_code"], "overloaded");
    assert_eq!(exception["detail"], "Try later");
    exception
}

#[test]
fn error_and_failed_bodies_preserve_the_byte_limit_and_utf8_boundaries() {
    for failed in [false, true] {
        for length in [65_535, 65_536, 65_537, 70_089] {
            let prefix = if length == 65_537 {
                error_body_prefix(failed).replace(",\"padding\"", ", \"padding\"")
            } else {
                error_body_prefix(failed).to_owned()
            };
            let body = format!("{prefix}{}\"}}", "A".repeat(length - prefix.len() - 2));
            let exception = observe_error_body(&body);
            let retained = exception["response_body"].as_str().unwrap();
            if length <= 65_536 {
                assert_eq!(retained, body);
                assert!(!retained.contains("[truncated]"));
            } else {
                assert_eq!(retained.len(), 65_536);
                assert!(retained.starts_with(prefix.as_str()));
                assert!(retained.ends_with("\n… [truncated]"));
                assert!(
                    retained[prefix.len()..65_520]
                        .bytes()
                        .all(|byte| byte == b'A')
                );
            }
        }

        for (rune_at, retained_bytes, rune_kept) in
            [(65_516, 65_536, true), (65_519, 65_535, false)]
        {
            let prefix = error_body_prefix(failed);
            let body = format!(
                "{prefix}{}😀{}\"}}",
                "A".repeat(rune_at - prefix.len()),
                "B".repeat(65_537 - rune_at - 4 - 2),
            );
            let exception = observe_error_body(&body);
            let retained = exception["response_body"].as_str().unwrap();
            assert_eq!(retained.len(), retained_bytes);
            assert!(retained.starts_with(prefix));
            assert!(retained.ends_with("\n… [truncated]"));
            assert_eq!(retained.contains('😀'), rune_kept);
            assert!(!retained.contains('�'));
            assert!(!retained.contains('B'));
        }
    }
}

#[test]
fn error_metadata_is_read_beyond_the_retained_body() {
    for failed in [false, true] {
        let prefix = if failed {
            r#"{"type":"response.failed","padding":""#
        } else {
            r#"{"type":"error","padding":""#
        };
        let suffix = if failed {
            r#"","response":{"id":"r1","error":{"type":"server_error","code":"overloaded","message":"Try later"}}}"#
        } else {
            r#"","error":{"type":"server_error","code":"overloaded","message":"Try later"}}"#
        };
        let body = format!("{prefix}{}{suffix}", "Z".repeat(70_000));
        let exception = observe_error_body(&body);
        let retained = exception["response_body"].as_str().unwrap();
        assert_eq!(retained.len(), 65_536);
        assert!(retained.starts_with(prefix));
        assert!(retained.ends_with("\n… [truncated]"));
        assert!(!retained.contains("Try later"));
    }
}

#[test]
fn malformed_item_json_still_reports_its_original_position() {
    let (events, exception) = observe_stream_async(&json!({
        "sse":"data:  {\"type\":\"response.output_item.done\",\"item\":{broken}\n\n"
    }));
    assert!(events.is_empty());
    let exception = exception.unwrap();
    assert_eq!(exception["type"], "System.Text.Json.JsonReaderException");
    assert_eq!(exception["line_number"], 0);
    assert_eq!(exception["byte_position_in_line"], 45);
}
