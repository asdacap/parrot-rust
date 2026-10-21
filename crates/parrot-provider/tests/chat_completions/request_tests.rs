use parrot_provider::chat_completions::*;
use serde_json::Value;

#[derive(Clone)]
struct Request {
    model: String,
    instructions: String,
    messages: Vec<Message>,
    tools: Vec<Tool>,
    effort: Option<String>,
    summary: Option<String>,
    preferences: String,
    metadata: bool,
    max: i32,
}
impl RequestView for Request {
    type Message = Message;
    type Tool = Tool;
    fn model(&self) -> &str {
        &self.model
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
        self.summary.as_deref()
    }
    fn provider_preferences(&self) -> &str {
        &self.preferences
    }
    fn include_router_metadata(&self) -> bool {
        self.metadata
    }
    fn max_output_tokens(&self) -> i32 {
        self.max
    }
}
#[derive(Clone)]
struct Message {
    role: MessageRole,
    text: String,
    contents: Vec<Content>,
    id: String,
    calls: Vec<Call>,
    provider_id: String,
    raw: String,
}
impl MessageView for Message {
    type Content = Content;
    type Call = Call;
    fn role(&self) -> MessageRole {
        self.role
    }
    fn text(&self) -> &str {
        &self.text
    }
    fn contents(&self) -> &[Content] {
        &self.contents
    }
    fn tool_call_id(&self) -> &str {
        &self.id
    }
    fn tool_calls(&self) -> &[Call] {
        &self.calls
    }
    fn provider_id(&self) -> &str {
        &self.provider_id
    }
    fn provider_content(&self) -> &str {
        &self.raw
    }
}
#[derive(Clone)]
enum Content {
    Text(String),
    Image {
        url: String,
        media: String,
        bytes: Vec<u8>,
    },
}
impl ContentView for Content {
    fn is_text(&self) -> bool {
        matches!(self, Self::Text(_))
    }
    fn text(&self) -> &str {
        match self {
            Self::Text(text) => text,
            _ => "",
        }
    }
    fn image_url(&self) -> &str {
        match self {
            Self::Image { url, .. } => url,
            _ => "",
        }
    }
    fn media_type(&self) -> &str {
        match self {
            Self::Image { media, .. } => media,
            _ => "",
        }
    }
    fn read_image(&self) -> Result<Vec<u8>, RequestError> {
        match self {
            Self::Image { bytes, .. } => Ok(bytes.clone()),
            _ => Ok(Vec::new()),
        }
    }
}
#[derive(Clone)]
struct Call {
    id: String,
    name: String,
    args: String,
}
impl ToolCallView for Call {
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn arguments_json(&self) -> &str {
        &self.args
    }
}
#[derive(Clone)]
struct Tool {
    name: String,
    description: String,
    schema: String,
}
impl ToolView for Tool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters_json(&self) -> &str {
        &self.schema
    }
}
fn message(role: MessageRole, text: &str) -> Message {
    Message {
        role,
        text: text.into(),
        contents: vec![Content::Text(text.into())],
        id: String::new(),
        calls: vec![],
        provider_id: String::new(),
        raw: String::new(),
    }
}
fn value(request: &Request) -> Value {
    serde_json::from_slice(&prepare(request, "test").unwrap().body).unwrap()
}

#[test]
fn request_has_source_top_level_fields_and_omits_unset_values() {
    let mut request = Request {
        model: "vendor/model".into(),
        instructions: "follow instructions".into(),
        messages: vec![message(MessageRole::User, "hello")],
        tools: vec![],
        effort: Some("xhigh".into()),
        summary: Some("ignored by chat wire".into()),
        preferences: "".into(),
        metadata: true,
        max: 4096,
    };
    let encoded = value(&request);
    assert_eq!(encoded["messages"].as_array().unwrap().len(), 2);
    assert_eq!(
        encoded["messages"][0],
        serde_json::json!({"role":"system","content":"follow instructions"})
    );
    assert_eq!(encoded["stream"], true);
    assert_eq!(encoded["stream_options"]["include_usage"], true);
    assert_eq!(encoded["reasoning_effort"], "xhigh");
    assert_eq!(encoded["max_tokens"], 4096);
    assert_eq!(encoded["include_router_metadata"], true);
    request.instructions.clear();
    request.effort = Some(String::new());
    request.metadata = false;
    request.max = 0;
    let encoded = value(&request);
    assert!(encoded.get("reasoning_effort").is_none());
    assert!(encoded.get("max_tokens").is_none());
    assert!(encoded.get("include_router_metadata").is_none());
}

#[test]
fn request_wraps_nonleading_system_and_preserves_message_call_order() {
    let mut later = message(MessageRole::System, "turn <reminder> & safe");
    later.calls = vec![
        Call {
            id: "second".into(),
            name: "b".into(),
            args: "{}".into(),
        },
        Call {
            id: "first".into(),
            name: "a".into(),
            args: "[]".into(),
        },
    ];
    let mut assistant = message(MessageRole::Assistant, "");
    assistant.calls.push(Call {
        id: "call_1".into(),
        name: "exec".into(),
        args: "{\"command\":1e999}".into(),
    });
    let request = Request {
        model: "m".into(),
        instructions: "rules".into(),
        messages: vec![assistant, later],
        tools: vec![],
        effort: None,
        summary: None,
        preferences: "".into(),
        metadata: false,
        max: 0,
    };
    let encoded = value(&request);
    assert!(encoded["messages"][1].get("content").is_none());
    assert_eq!(encoded["messages"][1]["tool_calls"][0]["id"], "call_1");
    assert_eq!(encoded["messages"][2]["role"], "user");
    assert_eq!(
        encoded["messages"][2]["content"],
        "<system-update>\nturn &lt;reminder&gt; &amp; safe\n</system-update>"
    );
    assert_eq!(encoded["messages"][2]["tool_calls"][0]["id"], "second");
}

#[test]
fn tool_schema_preferences_and_multimodal_content_keep_order_and_raw_numbers() {
    let request = Request {
        model: "m".into(),
        instructions: String::new(),
        messages: vec![Message {
            role: MessageRole::User,
            text: "ignored".into(),
            contents: vec![
                Content::Text("caption".into()),
                Content::Image {
                    url: String::new(),
                    media: "image/png".into(),
                    bytes: vec![0, 1, 255],
                },
            ],
            id: String::new(),
            calls: vec![],
            provider_id: String::new(),
            raw: String::new(),
        }],
        tools: vec![Tool {
            name: "read".into(),
            description: "".into(),
            schema: r#"{"type":"object","minimum":1e999,"x":2}"#.into(),
        }],
        effort: None,
        summary: None,
        preferences: r#" {"order": [2,1],"route":"local"} "#.into(),
        metadata: false,
        max: 10,
    };
    let bytes = prepare(&request, "test").unwrap().body;
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains("\"minimum\":1e999"));
    assert!(text.contains("\"provider\":{\"order\": [2,1],\"route\":\"local\"}"));
    let encoded: Value = serde_json::from_str(&text.replace("\"minimum\":1e999,", "")).unwrap();
    assert_eq!(encoded["messages"][0]["content"][0]["text"], "caption");
    assert_eq!(
        encoded["messages"][0]["content"][1]["image_url"]["url"],
        "data:image/png;base64,AAH/"
    );
    assert_eq!(encoded["tools"][0]["function"]["parameters"]["x"], 2);
}

#[test]
fn request_instructions_and_system_updates_have_source_wire_escaping() {
    let request = Request {
        model: "vendor/model".into(),
        instructions: "Follow repository rules.".into(),
        messages: vec![
            message(MessageRole::System, "history guidance"),
            message(MessageRole::User, "hello"),
            message(MessageRole::System, "turn <reminder> & safe"),
        ],
        tools: vec![],
        effort: None,
        summary: None,
        preferences: "".into(),
        metadata: false,
        max: 0,
    };
    let expected = concat!(
        r#"{"model":"vendor/model","messages":["#,
        r#"{"role":"system","content":"Follow repository rules."},"#,
        r#"{"role":"user","content":"\u003Csystem-update\u003E\nhistory guidance\n\u003C/system-update\u003E"},"#,
        r#"{"role":"user","content":"hello"},"#,
        r#"{"role":"user","content":"\u003Csystem-update\u003E\nturn \u0026lt;reminder\u0026gt; \u0026amp; safe\n\u003C/system-update\u003E"}],"#,
        r#""stream":true,"stream_options":{"include_usage":true}}"#,
    );
    let bytes = prepare(&request, "provider-a").unwrap().body;
    assert_eq!(bytes.len(), 411);
    assert_eq!(bytes.as_slice(), expected.as_bytes());
    let encoded: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        encoded,
        serde_json::json!({
            "model": "vendor/model",
            "messages": [
                {"role": "system", "content": "Follow repository rules."},
                {"role": "user", "content": "<system-update>\nhistory guidance\n</system-update>"},
                {"role": "user", "content": "hello"},
                {"role": "user", "content": "<system-update>\nturn &lt;reminder&gt; &amp; safe\n</system-update>"},
            ],
            "stream": true,
            "stream_options": {"include_usage": true},
        })
    );
}

#[test]
fn quoted_request_fields_escape_html_sensitive_characters() {
    let mut user = message(MessageRole::User, "<>&");
    user.id = "<>&".into();
    user.calls = vec![
        Call {
            id: "second<>&".into(),
            name: "b<>&".into(),
            args: r#"{"text":"<>&"}"#.into(),
        },
        Call {
            id: "first<>&".into(),
            name: "a<>&".into(),
            args: "{}".into(),
        },
    ];
    let request = Request {
        model: "<>&".into(),
        instructions: "<>&".into(),
        messages: vec![user],
        tools: vec![Tool {
            name: "<>&".into(),
            description: "<>&".into(),
            schema: "{}".into(),
        }],
        effort: Some("<>&".into()),
        summary: None,
        preferences: "".into(),
        metadata: false,
        max: 0,
    };
    let text = String::from_utf8(prepare(&request, "test").unwrap().body).unwrap();
    for field in [
        "model",
        "content",
        "tool_call_id",
        "name",
        "description",
        "reasoning_effort",
    ] {
        assert!(text.contains(&format!(r#""{field}":"\u003C\u003E\u0026""#)));
    }
    assert!(text.contains(r#""arguments":"{\"text\":\"\u003C\u003E\u0026\"}""#));
    assert!(!text.contains('<'));
    assert!(!text.contains('>'));
    assert!(!text.contains('&'));
    let encoded: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(encoded["model"], "<>&");
    assert_eq!(encoded["messages"][0]["role"], "system");
    assert_eq!(encoded["messages"][0]["content"], "<>&");
    assert_eq!(encoded["messages"][1]["role"], "user");
    assert_eq!(encoded["messages"][1]["content"], "<>&");
    assert_eq!(encoded["messages"][1]["tool_call_id"], "<>&");
    assert_eq!(encoded["messages"][1]["tool_calls"][0]["id"], "second<>&");
    assert_eq!(
        encoded["messages"][1]["tool_calls"][0]["function"]["name"],
        "b<>&"
    );
    assert_eq!(
        encoded["messages"][1]["tool_calls"][0]["function"]["arguments"],
        r#"{"text":"<>&"}"#
    );
    assert_eq!(encoded["messages"][1]["tool_calls"][1]["id"], "first<>&");
    assert_eq!(encoded["tools"][0]["function"]["name"], "<>&");
    assert_eq!(encoded["tools"][0]["function"]["description"], "<>&");
    assert_eq!(encoded["reasoning_effort"], "<>&");
}

#[test]
fn literal_backslash_u_sequences_remain_literal() {
    let input = r#"\u003C \u003E \u0026 \\ <>&"#;
    let request = Request {
        model: "m".into(),
        instructions: "".into(),
        messages: vec![message(MessageRole::User, input)],
        tools: vec![],
        effort: None,
        summary: None,
        preferences: "".into(),
        metadata: false,
        max: 0,
    };
    let text = String::from_utf8(prepare(&request, "test").unwrap().body).unwrap();
    assert!(text.contains(r#""content":"\\u003C \\u003E \\u0026 \\\\ \u003C\u003E\u0026""#));
    let encoded: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(encoded["messages"][0]["content"], input);
}

#[test]
fn unicode_quotes_and_controls_keep_values_and_message_order() {
    let input = "é 中文 😀 \" ' \\\n\t<>&";
    let request = Request {
        model: "m".into(),
        instructions: "".into(),
        messages: vec![
            message(MessageRole::User, input),
            message(MessageRole::System, input),
        ],
        tools: vec![],
        effort: None,
        summary: None,
        preferences: "".into(),
        metadata: false,
        max: 0,
    };
    let text = String::from_utf8(prepare(&request, "test").unwrap().body).unwrap();
    assert!(text.contains(r#""content":"é 中文 😀 \" ' \\\n\t\u003C\u003E\u0026""#));
    assert!(text.contains(
        r#""content":"\u003Csystem-update\u003E\né 中文 😀 \u0026quot; \u0026apos; \\\n\t\u0026lt;\u0026gt;\u0026amp;\n\u003C/system-update\u003E""#
    ));
    let encoded: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(encoded["messages"].as_array().unwrap().len(), 2);
    assert_eq!(encoded["messages"][0]["role"], "user");
    assert_eq!(encoded["messages"][0]["content"], input);
    assert_eq!(encoded["messages"][1]["role"], "user");
    assert_eq!(
        encoded["messages"][1]["content"],
        "<system-update>\né 中文 😀 &quot; &apos; \\\n\t&lt;&gt;&amp;\n</system-update>"
    );
}

#[test]
fn raw_schema_and_preferences_keep_verbatim_spelling() {
    let schema = r#"{"type":"object","minimum":1e+02,"enum":["<>&","\\u003C"],"order":[2,1]}"#;
    let preferences = r#" {"route":"<>&","literal":"\\u003C","order":[2,1]} "#;
    let arguments = r#"{"text":"<>&","literal":"\\u003C"}"#;
    let mut assistant = message(MessageRole::Assistant, "");
    assistant.calls.push(Call {
        id: "call_1".into(),
        name: "read".into(),
        args: arguments.into(),
    });
    let request = Request {
        model: "m".into(),
        instructions: "".into(),
        messages: vec![assistant],
        tools: vec![Tool {
            name: "read".into(),
            description: "".into(),
            schema: schema.into(),
        }],
        effort: None,
        summary: None,
        preferences: preferences.into(),
        metadata: false,
        max: 0,
    };
    let text = String::from_utf8(prepare(&request, "test").unwrap().body).unwrap();
    assert!(text.contains(&format!(r#""parameters":{schema}"#)));
    assert!(text.contains(&format!(r#""provider":{}"#, preferences.trim())));
    assert!(
        text.contains(
            r#""arguments":"{\"text\":\"\u003C\u003E\u0026\",\"literal\":\"\\\\u003C\"}""#
        )
    );
    let encoded: Value = serde_json::from_str(&text).unwrap();
    let parameters = &encoded["tools"][0]["function"]["parameters"];
    assert_eq!(parameters["minimum"].as_f64(), Some(100.0));
    assert_eq!(parameters["enum"], serde_json::json!(["<>&", r#"\u003C"#]));
    assert_eq!(parameters["order"], serde_json::json!([2, 1]));
    assert_eq!(encoded["provider"]["route"], "<>&");
    assert_eq!(encoded["provider"]["literal"], r#"\u003C"#);
    assert_eq!(encoded["provider"]["order"], serde_json::json!([2, 1]));
    assert_eq!(
        encoded["messages"][0]["tool_calls"][0]["function"]["arguments"],
        arguments
    );
}

#[test]
fn unusable_native_reasoning_payloads_fall_back_to_ordinary_history() {
    let cases = [
        (
            "other",
            r#"{"Version":1,"Protocol":"chat-completions","Reasoning":"secret"}"#,
        ),
        (
            "test",
            r#"{"Version":1,"Protocol":"responses","Reasoning":"secret"}"#,
        ),
        (
            "test",
            r#"{"Version":2,"Protocol":"chat-completions","Reasoning":"secret"}"#,
        ),
        (
            "test",
            r#"{"Version":1,"Protocol":"chat-completions","Reasoning":42}"#,
        ),
        ("test", "{"),
        ("test", "null"),
        ("test", ""),
    ];
    for (provider_id, raw) in cases {
        let mut assistant = message(MessageRole::Assistant, "answer");
        assistant.provider_id = provider_id.into();
        assistant.raw = raw.into();
        assistant.calls.push(Call {
            id: "call_1".into(),
            name: "read".into(),
            args: "{}".into(),
        });
        let request = Request {
            model: "model".into(),
            instructions: String::new(),
            messages: vec![assistant],
            tools: vec![],
            effort: None,
            summary: None,
            preferences: String::new(),
            metadata: false,
            max: 0,
        };
        let encoded = value(&request);
        let assistant = &encoded["messages"][0];
        assert_eq!(assistant["content"], "answer", "payload {raw:?}");
        assert_eq!(
            assistant["tool_calls"][0]["id"], "call_1",
            "payload {raw:?}"
        );
        assert!(assistant.get("reasoning").is_none(), "payload {raw:?}");
        assert!(
            assistant.get("reasoning_content").is_none(),
            "payload {raw:?}"
        );
    }
}

#[test]
fn reasoning_field_variants_round_trip_with_tools_outside_assistant_text() {
    for field in ["reasoning_content", "reasoning"] {
        let mut state = ParseState::default();
        for data in [
            format!(r#"{{"choices":[{{"delta":{{"{field}":null}}}}]}}"#),
            format!(r#"{{"choices":[{{"delta":{{"{field}":""}}}}]}}"#),
            format!(r#"{{"choices":[{{"delta":{{"{field}":"think "}}}}]}}"#),
            format!(
                r#"{{"choices":[{{"delta":{{"{field}":"more","content":"answer","tool_calls":[{{"index":0,"id":"call_1","function":{{"name":"read","arguments":"{{}}"}}}}]}},"finish_reason":"tool_calls"}}]}}"#
            ),
        ] {
            parse_data(
                &parrot_provider::http_stream::SseEvent {
                    event: String::new(),
                    data,
                    id: String::new(),
                },
                &mut state,
            )
            .unwrap();
        }
        let completion = state.completion("test");
        let replay = completion.provider_content.clone().unwrap();
        assert_eq!(completion.assistant_text, "answer");
        assert_eq!(completion.tool_calls[0].id, "call_1");
        assert_eq!(completion.tool_calls[0].name, "read");
        let replay_json = serde_json::json!({
            "Version": replay.version,
            "Protocol": replay.protocol,
            "ReasoningContent": replay.reasoning_content,
            "Reasoning": replay.reasoning,
        });
        let mut assistant = message(MessageRole::Assistant, &completion.assistant_text);
        assistant.provider_id = "test".into();
        assistant.raw = replay_json.to_string();
        assistant.calls = completion
            .tool_calls
            .iter()
            .map(|call| Call {
                id: call.id.clone(),
                name: call.name.clone(),
                args: call.arguments_json.clone(),
            })
            .collect();
        let mut tool_result = message(MessageRole::Tool, "read complete");
        tool_result.id = "call_1".into();
        let request = Request {
            model: "different-model".into(),
            instructions: String::new(),
            messages: vec![
                assistant,
                tool_result,
                message(MessageRole::User, "summarize that file"),
            ],
            tools: vec![],
            effort: None,
            summary: None,
            preferences: String::new(),
            metadata: false,
            max: 0,
        };
        let encoded = value(&request);
        let assistant = &encoded["messages"][0];
        assert_eq!(assistant["content"], "answer");
        assert_eq!(assistant["tool_calls"][0]["id"], "call_1");
        assert_eq!(
            assistant[field], "think more",
            "replay {replay:?}; encoded {encoded}"
        );
        let other = if field == "reasoning" {
            "reasoning_content"
        } else {
            "reasoning"
        };
        assert!(assistant.get(other).is_none());
        assert_eq!(encoded["messages"][1]["tool_call_id"], "call_1");
        assert_eq!(encoded["messages"][2]["role"], "user");
        assert_eq!(encoded["messages"][2]["content"], "summarize that file");
    }
}

#[test]
fn empty_instructions_keep_the_leading_history_system_message_unwrapped() {
    let request = Request {
        model: "model".into(),
        instructions: String::new(),
        messages: vec![
            message(MessageRole::System, "history guidance"),
            message(MessageRole::User, "hello"),
            message(MessageRole::System, "turn <reminder>"),
        ],
        tools: vec![],
        effort: None,
        summary: None,
        preferences: String::new(),
        metadata: false,
        max: 0,
    };
    let encoded = value(&request);
    assert_eq!(encoded["messages"].as_array().unwrap().len(), 3);
    assert_eq!(encoded["messages"][0]["role"], "system");
    assert_eq!(encoded["messages"][0]["content"], "history guidance");
    assert_eq!(encoded["messages"][1]["content"], "hello");
    assert_eq!(encoded["messages"][2]["role"], "user");
    assert_eq!(
        encoded["messages"][2]["content"],
        "<system-update>\nturn &lt;reminder&gt;\n</system-update>"
    );

    let request = Request {
        model: "model".into(),
        instructions: String::new(),
        messages: vec![
            message(MessageRole::User, "hello"),
            message(MessageRole::System, "turn <reminder>"),
        ],
        tools: vec![],
        effort: None,
        summary: None,
        preferences: String::new(),
        metadata: false,
        max: 0,
    };
    let encoded = value(&request);
    assert_eq!(encoded["messages"][1]["role"], "user");
    assert_eq!(
        encoded["messages"][1]["content"],
        "<system-update>\nturn &lt;reminder&gt;\n</system-update>"
    );
}

#[test]
fn provider_reasoning_replay_is_gated_and_restored_without_ordinary_fallback_merge() {
    let mut assistant = message(MessageRole::Assistant, "answer");
    assistant.provider_id = "test".into();
    assistant.raw=r#"{"Version":1,"Protocol":"chat-completions","ReasoningContent":"think","Reasoning":"other","unknown":1}"#.into();
    assistant.calls.push(Call {
        id: "call_1".into(),
        name: "read".into(),
        args: "{}".into(),
    });
    let mut request = Request {
        model: "m".into(),
        instructions: String::new(),
        messages: vec![assistant.clone()],
        tools: vec![],
        effort: None,
        summary: None,
        preferences: "".into(),
        metadata: false,
        max: 0,
    };
    let encoded = value(&request);
    assert_eq!(encoded["messages"][0]["reasoning_content"], "think");
    assert_eq!(encoded["messages"][0]["reasoning"], "other");

    assistant.raw =
        r#"{"Version":1,"Protocol":"chat-completions","Reasoning":"first","Reasoning":"second"}"#
            .into();
    request.messages = vec![assistant.clone()];
    let encoded = value(&request);
    assert_eq!(encoded["messages"][0]["reasoning"], "second");

    assistant.provider_id = "other".into();
    request.messages = vec![assistant];
    let encoded = value(&request);
    assert!(encoded["messages"][0].get("reasoning_content").is_none());
    assert!(encoded["messages"][0].get("reasoning").is_none());
    assert_eq!(encoded["messages"][0]["content"], "answer");
    assert_eq!(encoded["messages"][0]["tool_calls"][0]["id"], "call_1");
}

#[test]
fn malformed_schema_and_nonobject_preferences_are_errors() {
    let mut request = Request {
        model: "m".into(),
        instructions: String::new(),
        messages: vec![],
        tools: vec![Tool {
            name: "x".into(),
            description: String::new(),
            schema: "{".into(),
        }],
        effort: None,
        summary: None,
        preferences: String::new(),
        metadata: false,
        max: 0,
    };
    assert!(matches!(
        prepare(&request, "p"),
        Err(RequestError::InvalidSchema(_))
    ));
    request.tools.clear();
    request.preferences = "[]".into();
    assert_eq!(
        prepare(&request, "p"),
        Err(RequestError::ProviderPreferencesMustBeObject)
    );
}

#[test]
fn configured_tool_schema_descriptions_are_projected() {
    let request = Request {
        model: "vendor/model".into(),
        instructions: String::new(),
        messages: vec![],
        tools: vec![
            Tool {
                name: "path".into(),
                description: "Read configured content.".into(),
                schema: r#"{"type":"object","properties":{"path":{"type":"string","description":"Configured path prose."}}}"#.into(),
            },
            Tool {
                name: "glob".into(),
                description: "Find paths beneath an optional root with deterministic glob matching, including **. Relative roots resolve within the workspace.".into(),

                schema: r#"{"type":"object","properties":{"pattern":{"type":"string"}}}"#.into(),
            },
        ],
        effort: None,
        summary: None,
        preferences: String::new(),
        metadata: false,
        max: 0,
    };
    let encoded = value(&request);
    assert_eq!(
        encoded["tools"][0]["function"]["description"],
        "Read configured content."
    );
    assert_eq!(
        encoded["tools"][0]["function"]["parameters"]["properties"]["path"]["description"],
        "Configured path prose."
    );
    assert_eq!(
        encoded["tools"][1]["function"]["description"],
        "Find paths beneath an optional root with deterministic glob matching, including **. Relative roots resolve within the workspace."
    );
}

#[test]
fn reasoning_history_continuation_preserves_tool_result_and_user() {
    let request = Request {
        model: "model".into(),
        instructions: String::new(),
        messages: vec![
            {
                let mut assistant = message(MessageRole::Assistant, "checking");
                assistant.provider_id = "test".into();
                assistant.raw =
                    r#"{"Version":1,"Protocol":"chat-completions","Reasoning":"private thought"}"#
                        .into();
                assistant.calls.push(Call {
                    id: "call_read".into(),
                    name: "read".into(),
                    args: r#"{"path":"README.md"}"#.into(),
                });
                assistant
            },
            {
                let mut result = message(MessageRole::Tool, "read complete");
                result.id = "call_read".into();
                result
            },
            message(MessageRole::User, "summarize that file"),
        ],
        tools: vec![],
        effort: None,
        summary: None,
        preferences: String::new(),
        metadata: false,
        max: 0,
    };
    let encoded = value(&request);
    assert_eq!(encoded["messages"].as_array().unwrap().len(), 3);
    assert_eq!(encoded["messages"][0]["reasoning"], "private thought");
    assert_eq!(encoded["messages"][0]["tool_calls"][0]["id"], "call_read");
    assert_eq!(encoded["messages"][1]["role"], "tool");
    assert_eq!(encoded["messages"][1]["tool_call_id"], "call_read");
    assert_eq!(encoded["messages"][2]["role"], "user");
    assert_eq!(encoded["messages"][2]["content"], "summarize that file");
}

#[test]
fn both_reasoning_fields_round_trip_from_stream_completion() {
    let mut state = ParseState::default();
    for data in [
        r#"{"choices":[{"delta":{"reasoning_content":"think ","reasoning":"alternate ","content":"answer"}}]}"#,
        r#"{"choices":[{"delta":{"reasoning_content":"more","reasoning":"field"},"finish_reason":"stop"}]}"#,
    ] {
        parse_data(
            &parrot_provider::http_stream::SseEvent {
                event: String::new(),
                data: data.into(),
                id: String::new(),
            },
            &mut state,
        )
        .unwrap();
    }
    let completion = state.completion("test");
    let replay = completion.provider_content.unwrap();
    let assistant = {
        let mut assistant = message(MessageRole::Assistant, &completion.assistant_text);
        assistant.provider_id = replay.provider_id;
        assistant.raw = serde_json::json!({
            "Version": replay.version,
            "Protocol": replay.protocol,
            "ReasoningContent": replay.reasoning_content,
            "Reasoning": replay.reasoning,
        })
        .to_string();
        assistant
    };
    let request = Request {
        model: "model".into(),
        instructions: String::new(),
        messages: vec![assistant],
        tools: vec![],
        effort: None,
        summary: None,
        preferences: String::new(),
        metadata: false,
        max: 0,
    };
    let encoded = value(&request);
    assert_eq!(encoded["messages"][0]["content"], "answer");
    assert_eq!(encoded["messages"][0]["reasoning_content"], "think more");
    assert_eq!(encoded["messages"][0]["reasoning"], "alternate field");
}
