use super::{
    estimate_input_tokens,
    test_support::{Script, finish},
};
use crate::filtering::ReasoningSkipFilter;
use parrot_core::{CancellationToken, FilterProvider, FilterRequest, Message, Role, ToolCall};
use parrot_domain::values::context::TokenEstimator;

fn assistant(replay: String) -> Message {
    Message {
        provider_id: "test".into(),
        provider_content: replay,
        tool_calls: vec![ToolCall {
            id: "call".into(),
            name: "read".into(),
            arguments_json: "{}".into(),
        }],
        ..Message::text(Role::Assistant, "answer")
    }
}

fn estimate(message: &Message) -> i64 {
    let selection = std::sync::Arc::new(Script::default()).selection();
    estimate_input_tokens(
        &selection.canonical_model,
        "",
        &[],
        std::slice::from_ref(message),
    )
    .unwrap()
}

fn assert_reasoning_savings(replay: String, expected: i64) {
    let message = assistant(replay);
    let before = estimate(&message);
    let replacements = finish(ReasoningSkipFilter.filter(
        FilterRequest {
            messages: vec![message.clone()],
        },
        CancellationToken::uncancelled(),
    ))
    .unwrap()
    .expect("reasoning removed");
    assert_eq!(before - estimate(&replacements[0]), expected);
    assert_eq!(replacements[0].contents, message.contents);
    assert_eq!(replacements[0].tool_calls, message.tool_calls);
}

#[test]
fn chat_reasoning_filter_saves_both_reasoning_fields() {
    let thought = "consider the next step ".repeat(500);
    let other = "verify the result";
    let replay = format!(
        "{{\"Version\":1,\"Protocol\":\"chat-completions\",\"ReasoningContent\":{},\"Reasoning\":{},\"ToolCalls\":[]}}",
        serde_json::to_string(&thought).unwrap(),
        serde_json::to_string(other).unwrap(),
    );
    assert_reasoning_savings(
        replay,
        TokenEstimator::estimate_tokens(&thought) + TokenEstimator::estimate_tokens(other),
    );
}

#[test]
fn responses_reasoning_filter_saves_summary_and_content_text() {
    let replay = r#"{"Version":1,"Protocol":"responses","Output":[{"type":"reasoning","summary":[{"type":"summary_text","text":"summary reasoning"}],"content":[{"type":"reasoning_text","text":"detailed reasoning"}]},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]},{"type":"function_call","call_id":"call","name":"read","arguments":"{}"}]}"#;
    assert_reasoning_savings(
        replay.into(),
        TokenEstimator::estimate_tokens("summary reasoning")
            + TokenEstimator::estimate_tokens("detailed reasoning"),
    );
}

#[test]
fn anthropic_reasoning_filter_saves_thinking_without_counting_signature() {
    let replay = format!(
        r#"[{{"type":"thinking","thinking":"visible thought","signature":"{}"}},{{"type":"text","text":"answer"}}]"#,
        "opaque".repeat(500)
    );
    assert_reasoning_savings(replay, TokenEstimator::estimate_tokens("visible thought"));
}

#[test]
fn replay_copies_and_opaque_fields_do_not_add_reasoning_tokens() {
    let baseline = estimate(&assistant(String::new()));
    for replay in [
        r#"{"Version":1,"Protocol":"chat-completions","ToolCalls":[{"Id":"call","ThoughtSignature":"opaque"}]}"#,
        r#"{"Version":1,"Protocol":"responses","Output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]},{"type":"function_call","call_id":"call","name":"read","arguments":"{}"},{"type":"reasoning","encrypted_content":"opaque","summary":[]}]}"#,
        r#"[{"type":"text","text":"answer"},{"type":"redacted_thinking","data":"opaque"}]"#,
        "invalid json",
        r#"{"Version":2,"Protocol":"chat-completions","Reasoning":"unsupported"}"#,
        r#"{"Version":1,"Protocol":"unknown","Reasoning":"unsupported"}"#,
    ] {
        assert_eq!(estimate(&assistant(replay.into())), baseline, "{replay}");
    }
}

#[test]
fn replay_reasoning_requires_an_assistant_from_the_selected_provider() {
    let mut message = assistant(
        r#"{"Version":1,"Protocol":"chat-completions","Reasoning":"visible reasoning"}"#.into(),
    );
    let baseline = estimate(&assistant(String::new()));
    message.provider_id = "other".into();
    assert_eq!(estimate(&message), baseline);
    message.provider_id = "test".into();
    message.role = Role::User;
    assert_eq!(estimate(&message), baseline);
}
