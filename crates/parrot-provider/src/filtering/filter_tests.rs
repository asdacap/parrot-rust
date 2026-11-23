use super::anthropic::AnthropicReasoningFilter;
use super::chat_completions::ChatCompletionsReasoningFilter;
use super::responses::ResponsesReasoningFilter;
use parrot_core::{Message, ReasoningFilter, ReasoningKind, Role};

fn assistant(provider_content: &str) -> Message {
    Message {
        provider_content: provider_content.into(),
        ..Message::text(Role::Assistant, "answer")
    }
}

fn stripped(message: &Message) -> String {
    message.provider_content.clone()
}

fn responses_envelope(items: &[&str]) -> String {
    format!(
        "{{\"Version\":1,\"Protocol\":\"responses\",\"Output\":[{}]}}",
        items.join(",")
    )
}

fn responses_reasoning(encrypted: bool) -> String {
    let encrypted_field = if encrypted {
        "\"encrypted_content\":\"cipher\""
    } else {
        "\"encrypted_content\":null"
    };
    format!("{{\"id\":\"rs1\",\"type\":\"reasoning\",{encrypted_field},\"summary\":[]}}")
}

const RESPONSES_MESSAGE: &str = "{\"id\":\"m1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"answer\"}]}";
const RESPONSES_CALL: &str =
    "{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"read\",\"arguments\":\"{}\"}";

#[test]
fn responses_plain_reasoning_is_strippable() {
    let raw = responses_envelope(&[
        responses_reasoning(false).as_str(),
        RESPONSES_MESSAGE,
        RESPONSES_CALL,
    ]);
    let message = assistant(&raw);
    assert_eq!(
        ResponsesReasoningFilter.kind(&message),
        ReasoningKind::Plain
    );
    let filtered = ResponsesReasoningFilter
        .strip(&message)
        .expect("plain reasoning is strippable");
    assert_eq!(
        filtered.provider_content,
        responses_envelope(&[RESPONSES_MESSAGE, RESPONSES_CALL])
    );
    assert_eq!(filtered.contents, message.contents);
    assert_eq!(filtered.tool_calls, message.tool_calls);
}

#[test]
fn responses_encrypted_reasoning_is_protected() {
    let raw = responses_envelope(&[responses_reasoning(true).as_str(), RESPONSES_MESSAGE]);
    let message = assistant(&raw);
    assert_eq!(
        ResponsesReasoningFilter.kind(&message),
        ReasoningKind::Protected
    );
    assert_eq!(ResponsesReasoningFilter.strip(&message), None);
}

#[test]
fn responses_envelope_left_empty_stays_a_valid_envelope() {
    let raw = responses_envelope(&[responses_reasoning(false).as_str()]);
    let filtered = ResponsesReasoningFilter
        .strip(&assistant(&raw))
        .expect("batch changed");
    assert_eq!(
        filtered.provider_content,
        "{\"Version\":1,\"Protocol\":\"responses\",\"Output\":[]}"
    );
}

#[test]
fn responses_unrecognized_envelopes_are_absent() {
    for raw in [
        "",
        "not json at all",
        &responses_envelope(&[RESPONSES_MESSAGE]),
        "{\"Version\":1,\"Protocol\":\"responses\"}",
        "{\"Version\":2,\"Protocol\":\"responses\",\"Output\":[]}",
        "{\"Version\":1,\"Protocol\":\"chat-completions\",\"Reasoning\":\"x\"}",
    ] {
        assert_eq!(
            ResponsesReasoningFilter.kind(&assistant(raw)),
            ReasoningKind::Absent,
            "expected absent for {raw}"
        );
        assert_eq!(ResponsesReasoningFilter.strip(&assistant(raw)), None);
    }
}

#[test]
fn anthropic_thinking_blocks_are_strippable() {
    let raw = "[{\"type\":\"text\",\"text\":\"answer\"},{\"type\":\"thinking\",\"thinking\":\"thought\",\"signature\":\"sig\"}]";
    let message = assistant(raw);
    assert_eq!(
        AnthropicReasoningFilter.kind(&message),
        ReasoningKind::Plain
    );
    let filtered = AnthropicReasoningFilter
        .strip(&message)
        .expect("thinking is strippable");
    assert_eq!(
        filtered.provider_content,
        "[{\"type\":\"text\",\"text\":\"answer\"}]"
    );
}

#[test]
fn anthropic_redacted_thinking_is_protected() {
    let raw = "[{\"type\":\"text\",\"text\":\"answer\"},{\"type\":\"redacted_thinking\",\"data\":\"cipher\"}]";
    let message = assistant(raw);
    assert_eq!(
        AnthropicReasoningFilter.kind(&message),
        ReasoningKind::Protected
    );
    assert_eq!(AnthropicReasoningFilter.strip(&message), None);
}

#[test]
fn anthropic_messages_without_thinking_are_absent() {
    for raw in [
        "",
        "not json at all",
        "[{\"type\":\"text\",\"text\":\"answer\"}]",
        "[{\"type\":\"broken\"",
    ] {
        assert_eq!(
            AnthropicReasoningFilter.kind(&assistant(raw)),
            ReasoningKind::Absent,
            "expected absent for {raw}"
        );
    }
}

#[test]
fn chat_completions_reasoning_fields_are_strippable() {
    let raw = "{\"Version\":1,\"Protocol\":\"chat-completions\",\"ReasoningContent\":\"thought\",\"Reasoning\":\"other\",\"ToolCalls\":[],\"unknown\":1}";
    let message = assistant(raw);
    assert_eq!(
        ChatCompletionsReasoningFilter.kind(&message),
        ReasoningKind::Plain
    );
    let filtered = ChatCompletionsReasoningFilter
        .strip(&message)
        .expect("reasoning fields are strippable");
    assert_eq!(
        stripped(&filtered),
        "{\"Version\":1,\"Protocol\":\"chat-completions\",\"ToolCalls\":[],\"unknown\":1}"
    );
}

#[test]
fn chat_completions_reasoning_only_envelope_remains_valid_json() {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Envelope {
        version: i32,
        protocol: String,
    }
    let raw = "{\"Version\":1,\"Protocol\":\"chat-completions\",\"Reasoning\":\"thought\"}";
    let filtered = ChatCompletionsReasoningFilter
        .strip(&assistant(raw))
        .expect("reasoning is strippable");
    let envelope: Envelope = serde_json::from_str(&filtered.provider_content)
        .expect("filtered replay must remain valid JSON");
    assert_eq!(envelope.version, 1);
    assert_eq!(envelope.protocol, "chat-completions");
    assert_eq!(filtered.contents, assistant(raw).contents);
}

#[test]
fn chat_completions_envelopes_without_reasoning_are_absent() {
    for raw in [
        "",
        "not json at all",
        "{\"Version\":1,\"Protocol\":\"chat-completions\",\"ReasoningContent\":null,\"ToolCalls\":[]}",
        "{\"Version\":1,\"Protocol\":\"chat-completions\",\"ToolCalls\":[]}",
        "{\"Version\":1,\"Protocol\":\"responses\",\"Output\":[]}",
    ] {
        assert_eq!(
            ChatCompletionsReasoningFilter.kind(&assistant(raw)),
            ReasoningKind::Absent,
            "expected absent for {raw}"
        );
        assert_eq!(ChatCompletionsReasoningFilter.strip(&assistant(raw)), None);
    }
}
