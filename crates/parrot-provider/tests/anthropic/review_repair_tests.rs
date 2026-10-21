//! Focused regressions for the three independent T28 review findings.
use parrot_provider::anthropic::{AnthropicEvent, AnthropicParser, ParserError};
use serde_json::{Value, json};
use std::fmt::Write;

use super::projection;

fn stopped(parser: &mut AnthropicParser) {
    parser.consume(r#"{"type":"message_stop"}"#).unwrap();
}

#[test]
fn malformed_names_are_only_decoded_when_reverse_lookup_visits_them() {
    for raw in [
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"visible"},"\ud800":"ignored"}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"visible","\ud800":"ignored"}}"#,
    ] {
        let mut parser = AnthropicParser::new("p");
        assert!(matches!(
            parser.consume(raw),
            Err(ParserError::InvalidJsonString(_))
        ));
    }
    for raw in [
        r#"{"\ud800":"ignored","type":"content_block_start","index":0,"content_block":{"type":"text","text":"visible"}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"\ud800":"ignored","type":"text","text":"visible"}}"#,
    ] {
        let mut parser = AnthropicParser::new("p");
        assert_eq!(
            parser.consume(raw).unwrap(),
            vec![AnthropicEvent::TextDelta {
                text: "visible".into()
            }]
        );
        stopped(&mut parser);
        assert!(matches!(
            parser.finish_next().unwrap(),
            Some(AnthropicEvent::Completed(_))
        ));
    }
}

#[test]
fn too_short_unknown_usage_names_do_not_hide_valid_counts() {
    for usage in [
        r#"{"input_tokens":2,"\ud800":1}"#,
        r#"{"\ud800":1,"input_tokens":2}"#,
    ] {
        let mut parser = AnthropicParser::new("p");
        parser
            .consume(&format!(
                r#"{{"type":"message_start","message":{{"id":"m","usage":{usage}}}}}"#
            ))
            .unwrap();
        stopped(&mut parser);
        let Some(AnthropicEvent::Completed(message)) = parser.finish_next().unwrap() else {
            panic!("expected completion");
        };
        assert_eq!(message.input_tokens, 2);
    }
}

#[test]
fn all_accumulated_tools_are_published_before_serialization_failure() {
    let mut parser = AnthropicParser::new("p");
    parser.consume(r#"{"type":"content_block_start","index":9,"content_block":{"type":"tool_use","id":"last","name":"t","input":{"ok":true}}}"#).unwrap();
    parser.consume(r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"first","name":"t","input":{"x":"\ud800"}}}"#).unwrap();
    stopped(&mut parser);
    for (id, arguments) in [("first", r#"{"x":"\ud800"}"#), ("last", r#"{"ok":true}"#)] {
        let Some(AnthropicEvent::ToolCallDelta(call)) = parser.finish_next().unwrap() else {
            panic!("tool delta must precede completion serialization");
        };
        assert_eq!(call.id, id);
        assert_eq!(call.arguments_json, arguments);
    }
    assert_eq!(
        parser.finish_next(),
        Err(ParserError::Serialization(
            "The object or value could not be serialized. Path: $.".into()
        ))
    );
    assert_eq!(parser.finish_next().unwrap(), None);
}

fn replay_input() -> Value {
    let inputs: Value =
        serde_json::from_str(include_str!("fixtures/review-v4/inputs.json")).unwrap();
    inputs["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["case_id"] == "supplemental.review-v4.typed-duplicate-invalid-first")
        .unwrap()["input"]
        .clone()
}

#[tokio::test]
async fn malformed_replay_and_foreign_provider_content_rebuild_ordinary_text() {
    for (provider, raw) in [
        ("custom", "not json"),
        (
            "foreign",
            r#"[{"type":"thinking","signature":"sig","thinking":"plan"},{"type":"tool_use","input":{"x":"\ud800"}}]"#,
        ),
    ] {
        let mut input = replay_input();
        input["request"]["messages"][0]["provider_content"] =
            json!({"provider_id":provider,"content":raw});
        let observed = projection::observe("repair-replay-fallback", &input).await;
        assert_eq!(observed["error"], Value::Null);
        let encoded = &observed["encoded_json"];
        assert_eq!(
            encoded["messages"][0]["content"],
            json!([{"type":"text","text":"fallback"}])
        );
        assert!(encoded.get("thinking").is_none());
    }
}

#[tokio::test]
async fn typed_nested_duplicate_members_are_validated_before_last_wins() {
    for raw in [
        r#"[{"type":"image","source":{"type":"url","url":42,"url":"ok"}}]"#,
        r#"[{"type":"text","cache_control":{"type":42,"type":"ephemeral"}}]"#,
        r#"[{"type":"tool_result","content":[{"type":"thinking","signature":42,"signature":"sig"}]}]"#,
    ] {
        let mut input = replay_input();
        input["request"]["messages"][0]["provider_content"]["content"] = json!(raw);
        let observed = projection::observe("repair-nested-typed-duplicates", &input).await;
        assert_eq!(observed["error"], Value::Null);
        assert_eq!(
            observed["encoded_json"]["messages"][0]["content"],
            json!([{"type":"text","text":"fallback"}])
        );
    }
}

#[tokio::test]
async fn overwritten_opaque_inputs_are_not_serialized_but_final_duplicates_are_kept() {
    let mut input = replay_input();
    input["request"]["messages"][0]["provider_content"]["content"] =
        json!(r#"[{"type":"tool_use","input":{"x":"\ud800"},"input":{"x":1,"x":2}}]"#);
    let observed = projection::observe("repair-opaque-input-last-wins", &input).await;
    assert_eq!(observed["error"], Value::Null);
    let encoded = observed["encoded_hex"].as_str().unwrap();
    let needle = br#""input":{"x":1,"x":2}"#
        .iter()
        .fold(String::new(), |mut text, byte| {
            write!(text, "{byte:02x}").unwrap();
            text
        });
    assert!(
        encoded.contains(&needle),
        "opaque duplicate input members must survive encoding"
    );
}
