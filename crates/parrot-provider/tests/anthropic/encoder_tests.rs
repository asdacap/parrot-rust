use parrot_provider::anthropic::{
    ContentKind, ContentView, EncodeError, MessageView, RequestView, Role, ToolCallView,
    ToolDefinitionView, encode,
};
use serde_json::{Value, json};
use std::{cell::Cell, io};

struct Part {
    kind: ContentKind,
    text: String,
    url: String,
    bytes: Vec<u8>,
    fail: bool,
    reads: Cell<usize>,
}
impl Part {
    fn text(text: &str) -> Self {
        Self {
            kind: ContentKind::Text,
            text: text.into(),
            url: String::new(),
            bytes: vec![],
            fail: false,
            reads: Cell::new(0),
        }
    }
    fn image(url: &str, bytes: &[u8]) -> Self {
        Self {
            kind: ContentKind::Image,
            url: url.into(),
            bytes: bytes.into(),
            ..Self::text("")
        }
    }
}
impl ContentView for Part {
    fn kind(&self) -> ContentKind {
        self.kind
    }
    fn text(&self) -> &str {
        &self.text
    }
    fn image_path(&self) -> &str {
        "never-opened"
    }
    fn image_url(&self) -> &str {
        &self.url
    }
    fn media_type(&self) -> &str {
        "image/png"
    }
    fn image_width(&self) -> i32 {
        123
    }
    fn image_height(&self) -> i32 {
        456
    }
    fn read_image(&self) -> io::Result<Vec<u8>> {
        self.reads.set(self.reads.get() + 1);
        if self.fail {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "test image denied",
            ))
        } else {
            Ok(self.bytes.clone())
        }
    }
}
struct Call {
    arguments: String,
}
impl ToolCallView for Call {
    fn id(&self) -> &str {
        "call-1"
    }
    fn name(&self) -> &str {
        "lookup"
    }
    fn arguments_json(&self) -> &str {
        &self.arguments
    }
}
struct Tool {
    schema: String,
    description: String,
}
impl ToolDefinitionView for Tool {
    fn name(&self) -> &str {
        "lookup"
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters_json(&self) -> &str {
        &self.schema
    }
}
struct Message {
    role: Role,
    parts: Vec<Part>,
    calls: Vec<Call>,
    provider: String,
    replay: String,
}
impl Message {
    fn new(role: Role, text: &str) -> Self {
        Self {
            role,
            parts: vec![Part::text(text)],
            calls: vec![],
            provider: String::new(),
            replay: String::new(),
        }
    }
    fn replay(raw: &str) -> Self {
        Self {
            provider: "test".into(),
            replay: raw.into(),
            ..Self::new(Role::Assistant, "fallback")
        }
    }
}
impl MessageView for Message {
    type Content = Part;
    type ToolCall = Call;
    fn role(&self) -> Role {
        self.role
    }
    fn contents(&self) -> &[Part] {
        &self.parts
    }
    fn tool_calls(&self) -> &[Call] {
        &self.calls
    }
    fn tool_call_id(&self) -> &str {
        "call-1"
    }
    fn provider_id(&self) -> &str {
        &self.provider
    }
    fn provider_content(&self) -> &str {
        &self.replay
    }
}
#[derive(Default)]
struct Request {
    instructions: String,
    messages: Vec<Message>,
    tools: Vec<Tool>,
    effort: Option<String>,
}
impl RequestView for Request {
    type Message = Message;
    type Tool = Tool;
    fn model(&self) -> &str {
        "claude-test"
    }
    fn max_output_tokens(&self) -> i32 {
        -7
    }
    fn instructions(&self) -> &str {
        &self.instructions
    }
    fn messages(&self) -> &[Message] {
        &self.messages
    }
    fn tools(&self) -> &[Tool] {
        &self.tools
    }
    fn reasoning_effort(&self) -> Option<&str> {
        self.effort.as_deref()
    }
    fn reasoning_summary(&self) -> Option<&str> {
        Some("ignored-summary")
    }
    fn provider_preferences(&self) -> &str {
        "invalid-but-unused"
    }
    fn include_router_metadata(&self) -> bool {
        true
    }
}
fn raw(request: &Request) -> String {
    String::from_utf8(encode(request, "test").unwrap().body).unwrap()
}
fn value(request: &Request) -> Value {
    serde_json::from_str(&raw(request)).unwrap()
}

#[test]
fn compact_field_order_and_source_defaults() {
    assert_eq!(
        raw(&Request::default()),
        "{\"model\":\"claude-test\",\"max_tokens\":-7,\"messages\":[],\"stream\":true,\"cache_control\":{\"type\":\"ephemeral\"}}"
    );
}

#[test]
fn system_cache_skipped_messages_and_adjacent_roles() {
    let request = Request {
        instructions: "instructions".into(),
        messages: vec![
            Message::new(Role::User, ""),
            Message::new(Role::System, "first"),
            Message::new(Role::System, "second"),
            Message::new(Role::Assistant, "answer"),
            Message::new(Role::Tool, "result"),
            Message::new(Role::System, "&<>\"'"),
            Message::new(Role::User, "next"),
        ],
        ..Request::default()
    };
    let result = value(&request);
    assert_eq!(
        result["system"],
        json!([
            {"type":"text","text":"instructions"},
            {"type":"text","text":"first"},
            {"type":"text","text":"second","cache_control":{"type":"ephemeral"}}
        ])
    );
    assert_eq!(result["messages"].as_array().unwrap().len(), 2);
    assert_eq!(
        result["messages"][1],
        json!({"role":"user","content":[
            {"type":"tool_result","tool_use_id":"call-1","content":[{"type":"text","text":"result"}]},
            {"type":"text","text":"<system-update>\n&amp;&lt;&gt;&quot;&apos;\n</system-update>"},
            {"type":"text","text":"next"}
        ]})
    );
    assert!(raw(&request).contains("\\u003Csystem-update\\u003E"));
}

#[test]
fn leading_system_concatenates_only_text_and_empty_tool_result_omits_content() {
    let request = Request {
        messages: vec![
            Message {
                parts: vec![
                    Part::text("a"),
                    Part::image("", b"ignored"),
                    Part::text("b"),
                ],
                ..Message::new(Role::System, "")
            },
            Message::new(Role::Tool, ""),
        ],
        ..Request::default()
    };
    let result = value(&request);
    assert_eq!(result["system"][0]["text"], "ab");
    assert_eq!(
        result["messages"][0]["content"][0],
        json!({"type":"tool_result","tool_use_id":"call-1"})
    );
    assert_eq!(request.messages[0].parts[1].reads.get(), 0);
}

#[test]
fn lazy_images_url_inline_and_base64_padding() {
    for (bytes, encoded) in [
        (b"".as_slice(), ""),
        (b"f", "Zg=="),
        (b"fo", "Zm8="),
        (b"foo", "Zm9v"),
        (&[0xfb, 0xff, 0xff], "+///"),
    ] {
        let request = Request {
            messages: vec![Message {
                parts: vec![
                    Part::image("https://image", b"never"),
                    Part::image("", bytes),
                ],
                ..Message::new(Role::User, "")
            }],
            ..Request::default()
        };
        assert!(
            request.messages[0]
                .parts
                .iter()
                .all(|part| part.reads.get() == 0)
        );
        let result = value(&request);
        assert_eq!(
            result["messages"][0]["content"][0]["source"],
            json!({"type":"url","url":"https://image"})
        );
        assert_eq!(
            result["messages"][0]["content"][1]["source"],
            json!({"type":"base64","media_type":"image/png","data":encoded})
        );
        assert_eq!(request.messages[0].parts[0].reads.get(), 0);
        assert_eq!(request.messages[0].parts[1].reads.get(), 1);
    }
}

#[test]
fn image_errors_are_preserved() {
    let request = Request {
        messages: vec![Message {
            parts: vec![Part {
                fail: true,
                ..Part::image("", b"")
            }],
            ..Message::new(Role::User, "")
        }],
        ..Request::default()
    };
    let error = encode(&request, "test").unwrap_err();
    assert_eq!(error.to_string(), "test image denied");
    assert!(
        matches!(error, EncodeError::Image(error) if error.kind() == io::ErrorKind::PermissionDenied)
    );
}

#[test]
fn provider_replay_replaces_content_and_calls_without_reading_images() {
    let request = Request {
        messages: vec![Message {
            parts: vec![Part {
                fail: true,
                ..Part::image("", b"")
            }],
            calls: vec![Call {
                arguments: "{}".into(),
            }],
            ..Message::replay(
                r#"[{"type":"thinking","thinking":"secret","signature":"signed","unknown":"ignored"},{"type":"redacted_thinking","data":"encrypted"}]"#,
            )
        }],
        ..Request::default()
    };
    let result = value(&request);
    assert_eq!(
        result["messages"][0]["content"],
        json!([
            {"type":"thinking","thinking":"secret","signature":"signed"},
            {"type":"redacted_thinking","data":"encrypted"}
        ])
    );
    assert_eq!(
        result["thinking"],
        json!({"type":"adaptive","display":"summarized","block_binding":{"prefix_mismatch_behavior":"drop_block"}})
    );
    assert!(result.get("output_config").is_none());
    assert_eq!(request.messages[0].parts[0].reads.get(), 0);
}

#[test]
fn replay_wrong_provider_malformed_and_null_fallback_but_empty_overrides() {
    for replay in ["broken", "null", "{}", r#"[{"type":17}]"#] {
        let request = Request {
            messages: vec![Message::replay(replay)],
            ..Request::default()
        };
        assert_eq!(
            value(&request)["messages"][0]["content"][0]["text"],
            "fallback"
        );
    }
    let wrong = Request {
        messages: vec![Message {
            provider: "other".into(),
            ..Message::replay(r#"[{"type":"thinking","thinking":"private"}]"#)
        }],
        ..Request::default()
    };
    assert_eq!(
        value(&wrong)["messages"][0]["content"][0]["text"],
        "fallback"
    );
    assert!(value(&wrong).get("thinking").is_none());
    let empty = Request {
        messages: vec![
            Message::replay("[]"),
            Message::new(Role::System, "still leading"),
        ],
        ..Request::default()
    };
    assert_eq!(value(&empty)["messages"], json!([]));
    assert_eq!(value(&empty)["system"][0]["text"], "still leading");
}

#[test]
fn unknown_replay_type_keeps_known_fields_and_raw_input_numbers() {
    let request = Request {
        messages: vec![Message::replay(
            r#"[{"type":"future","text":"kept","input":{"n":1e99999,"n":123456789012345678901234567890},"extra":42}]"#,
        )],
        ..Request::default()
    };
    let result = raw(&request);
    assert!(result.contains(r#"{"type":"future","text":"kept","input":{"n":1e99999,"n":123456789012345678901234567890}}"#));
    assert!(!result.contains("extra"));
}

#[test]
fn tool_inputs_require_objects_and_schemas_preserve_raw_numbers_duplicates() {
    for arguments in ["invalid", "null", "[1]", "42", "\"text\""] {
        let request = Request {
            messages: vec![Message {
                calls: vec![Call {
                    arguments: arguments.into(),
                }],
                ..Message::new(Role::Assistant, "")
            }],
            ..Request::default()
        };
        assert_eq!(
            value(&request)["messages"][0]["content"][0]["input"],
            json!({})
        );
    }
    let input = r#"{ "n":1e99999, "n":999999999999999999999999, "s":"<+é" }"#;
    let request = Request {
        messages: vec![Message {
            calls: vec![Call {
                arguments: input.into(),
            }],
            ..Message::new(Role::Assistant, "")
        }],
        tools: vec![
            Tool {
                schema: input.into(),
                description: String::new(),
            },
            Tool {
                schema: String::new(),
                description: "described".into(),
            },
        ],
        ..Request::default()
    };
    let result = raw(&request);
    assert_eq!(
        result
            .matches(r#"{"n":1e99999,"n":999999999999999999999999,"s":"\u003C\u002B\u00E9"}"#)
            .count(),
        2
    );
    assert!(result.contains(r#"{"name":"lookup","description":"described","input_schema":{}}"#));
    let malformed = Request {
        tools: vec![Tool {
            schema: "{".into(),
            description: String::new(),
        }],
        ..Request::default()
    };
    assert!(matches!(
        encode(&malformed, "test"),
        Err(EncodeError::Json(_))
    ));
}

#[test]
fn effort_enables_thinking_and_output_config_and_unsupported_options_stay_unused() {
    let request = Request {
        effort: Some("high".into()),
        ..Request::default()
    };
    let result = value(&request);
    assert_eq!(result["output_config"], json!({"effort":"high"}));
    assert_eq!(
        result["thinking"]["block_binding"]["prefix_mismatch_behavior"],
        "drop_block"
    );
    assert!(result.get("reasoning_summary").is_none());
    assert!(result.get("provider").is_none());
    assert!(result.get("include_router_metadata").is_none());
    let empty = Request {
        effort: Some(String::new()),
        ..Request::default()
    };
    assert!(value(&empty).get("thinking").is_none());
}
