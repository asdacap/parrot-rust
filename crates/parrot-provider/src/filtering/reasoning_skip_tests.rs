use super::reasoning_skip::{ReasoningSkipFilter, filter_batch};
use parrot_core::{CancellationToken, FilterProvider, FilterRequest, Message, Role};
use std::task::{Context, Waker};

fn assistant(provider_content: &str) -> Message {
    Message {
        role: Role::Assistant,
        provider_content: provider_content.into(),
        ..Message::text(Role::Assistant, "answer")
    }
}

fn user(text: &str) -> Message {
    Message::text(Role::User, text)
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
fn responses_reasoning_items_are_stripped() {
    let raw = responses_envelope(&[
        responses_reasoning(false).as_str(),
        RESPONSES_MESSAGE,
        RESPONSES_CALL,
    ]);
    let batch = vec![assistant(&raw)];
    let filtered = filter_batch(batch).expect("batch changed");
    assert_eq!(
        filtered[0].provider_content,
        responses_envelope(&[RESPONSES_MESSAGE, RESPONSES_CALL])
    );
    assert_eq!(filtered[0].contents, assistant(&raw).contents);
    assert_eq!(filtered[0].tool_calls, assistant(&raw).tool_calls);
}

#[test]
fn responses_encrypted_reasoning_blocks_the_batch() {
    let raw = responses_envelope(&[responses_reasoning(true).as_str(), RESPONSES_MESSAGE]);
    assert_eq!(filter_batch(vec![assistant(&raw)]), None);
}

#[test]
fn responses_null_encrypted_content_is_still_strippable() {
    let raw = responses_envelope(&[responses_reasoning(false).as_str(), RESPONSES_MESSAGE]);
    let filtered = filter_batch(vec![assistant(&raw)]).expect("batch changed");
    assert_eq!(
        filtered[0].provider_content,
        responses_envelope(&[RESPONSES_MESSAGE])
    );
}

#[test]
fn responses_envelope_left_empty_stays_a_valid_envelope() {
    let raw = responses_envelope(&[responses_reasoning(false).as_str()]);
    let filtered = filter_batch(vec![assistant(&raw)]).expect("batch changed");
    assert_eq!(
        filtered[0].provider_content,
        "{\"Version\":1,\"Protocol\":\"responses\",\"Output\":[]}"
    );
}

#[test]
fn anthropic_thinking_blocks_are_stripped() {
    let raw = "[{\"type\":\"text\",\"text\":\"answer\"},{\"type\":\"thinking\",\"thinking\":\"thought\",\"signature\":\"sig\"}]";
    let filtered = filter_batch(vec![assistant(raw)]).expect("batch changed");
    assert_eq!(
        filtered[0].provider_content,
        "[{\"type\":\"text\",\"text\":\"answer\"}]"
    );
}

#[test]
fn anthropic_redacted_thinking_blocks_the_batch() {
    let raw = "[{\"type\":\"text\",\"text\":\"answer\"},{\"type\":\"redacted_thinking\",\"data\":\"cipher\"}]";
    assert_eq!(filter_batch(vec![assistant(raw)]), None);
}

#[test]
fn one_encrypted_message_blocks_the_whole_batch() {
    let strippable = assistant(&responses_envelope(&[
        responses_reasoning(false).as_str(),
        RESPONSES_MESSAGE,
    ]));
    let encrypted = assistant(&responses_envelope(&[
        responses_reasoning(true).as_str(),
        RESPONSES_MESSAGE,
    ]));
    let batch = vec![user("hello"), strippable, encrypted];
    assert_eq!(filter_batch(batch), None);
}

#[test]
fn chat_completions_reasoning_fields_are_stripped() {
    let raw = "{\"Version\":1,\"Protocol\":\"chat-completions\",\"ReasoningContent\":\"thought\",\"Reasoning\":\"other\",\"ToolCalls\":[],\"unknown\":1}";
    let filtered = filter_batch(vec![assistant(raw)]).expect("batch changed");
    assert_eq!(
        filtered[0].provider_content,
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
    let filtered = filter_batch(vec![assistant(raw)]).expect("batch changed");
    let envelope: Envelope = serde_json::from_str(&filtered[0].provider_content)
        .expect("filtered replay must remain valid JSON");
    assert_eq!(envelope.version, 1);
    assert_eq!(envelope.protocol, "chat-completions");
    assert_eq!(filtered[0].contents, assistant(raw).contents);
}

#[test]
fn chat_completions_null_reasoning_fields_are_unchanged() {
    let raw = "{\"Version\":1,\"Protocol\":\"chat-completions\",\"ReasoningContent\":null,\"ToolCalls\":[]}";
    let batch = vec![user("hello"), assistant(raw)];
    assert_eq!(filter_batch(batch), None);
}

#[test]
fn messages_without_reasoning_are_unchanged_and_unchanged_batches_are_none() {
    let chat_replay = "{\"Version\":1,\"Protocol\":\"chat-completions\",\"ToolCalls\":[]}";
    let batch = vec![
        user("hello"),
        assistant(""),
        assistant(chat_replay),
        assistant("not json at all"),
    ];
    assert_eq!(filter_batch(batch), None);
}

#[test]
fn filter_trait_reports_the_replacement_batch() {
    let raw = responses_envelope(&[responses_reasoning(false).as_str(), RESPONSES_MESSAGE]);
    let request = FilterRequest {
        messages: vec![assistant(&raw)],
    };
    let mut future = ReasoningSkipFilter.filter(request, CancellationToken::uncancelled());
    let outcome = match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        std::task::Poll::Ready(value) => value,
        std::task::Poll::Pending => panic!("filter never yields"),
    };
    let replacements = outcome.expect("filter succeeded");
    assert_eq!(replacements.expect("batch changed").len(), 1);
}
