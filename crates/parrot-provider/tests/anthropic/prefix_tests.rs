//! Source-proven first-unescaped-prefix property lookup regressions.
use parrot_provider::anthropic::{AnthropicEvent, AnthropicParser, ParserError};

fn event(name: &str) -> String {
    format!(
        r#"{{"type":"content_block_start","index":0,"content_block":{{"type":"text","text":"visible"}},"{name}":"ignored"}}"#
    )
}

#[test]
fn mismatching_unescaped_prefix_leaves_malformed_suffix_unused() {
    for name in [r"x\ud800", r"abc\ud800", r"a\u0062c\ud800"] {
        let mut parser = AnthropicParser::new("p");
        assert_eq!(
            parser.consume(&event(name)).unwrap(),
            vec![AnthropicEvent::TextDelta {
                text: "visible".into()
            }],
            "{name}"
        );
        parser.consume(r#"{"type":"message_stop"}"#).unwrap();
        assert!(matches!(
            parser.finish_next().unwrap(),
            Some(AnthropicEvent::Completed(_))
        ));
    }
}

#[test]
fn matching_raw_prefix_decodes_entire_suffix_even_if_escaped_prefix_mismatches() {
    for name in [
        r"t\ud800",
        r"\u0078\ud800",
        r"\u0074\ud800",
        r"t\u0078\ud800",
        r"t\u0079\ud800",
    ] {
        let mut parser = AnthropicParser::new("p");
        assert_eq!(
            parser.consume(&event(name)),
            Err(ParserError::InvalidJsonString(
                "Cannot read incomplete UTF-16 JSON text as string with missing low surrogate."
                    .into()
            )),
            "{name}"
        );
    }
}

#[test]
fn usage_prefix_selection_preserves_reverse_order_and_escaped_selected_names() {
    for usage in [
        r#"{"input_tokens":2,"x\ud800":1}"#,
        r#"{"input_token\ud800":1,"input_tokens":2}"#,
        r#"{"\u0069nput_tokens":2}"#,
    ] {
        let mut parser = AnthropicParser::new("p");
        parser
            .consume(&format!(
                r#"{{"type":"message_start","message":{{"id":"m","usage":{usage}}}}}"#
            ))
            .unwrap();
        parser.consume(r#"{"type":"message_stop"}"#).unwrap();
        let Some(AnthropicEvent::Completed(message)) = parser.finish_next().unwrap() else {
            panic!("expected completion");
        };
        assert_eq!(message.input_tokens, 2);
    }
    let mut parser = AnthropicParser::new("p");
    assert!(matches!(
        parser.consume(
            r#"{"type":"message_start","message":{"usage":{"input_tokens":2,"input_token\ud800":1}}}"#
        ),
        Err(ParserError::InvalidJsonString(_))
    ));
}
