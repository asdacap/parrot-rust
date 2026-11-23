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

fn filter_provider_batch(messages: Vec<Message>) -> Option<Vec<Message>> {
    use parrot_foundation::config::{models::ModelSettings, node::Node};

    let root = Node::mapping(vec![
        (
            Node::scalar("provider_model_alias_defaults"),
            Node::mapping(vec![]),
        ),
        (
            Node::scalar("providers"),
            Node::mapping(
                [
                    ("openai", "chat-completions"),
                    ("chatgpt", "responses"),
                    ("other", "responses"),
                ]
                .into_iter()
                .map(|(id, protocol)| {
                    (
                        Node::scalar(id),
                        Node::mapping(vec![(Node::scalar("protocol"), Node::scalar(protocol))]),
                    )
                })
                .collect(),
            ),
        ),
    ]);
    let settings = ModelSettings::read(&root).expect("provider settings are valid");
    let filter = ReasoningSkipFilter::new(&settings.providers);
    let mut future = filter.filter(FilterRequest { messages }, CancellationToken::uncancelled());
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        std::task::Poll::Ready(result) => result.expect("filter succeeded"),
        std::task::Poll::Pending => panic!("filter never yields"),
    }
}

fn provider_assistant(provider_id: &str, content: &str) -> Message {
    Message {
        provider_id: provider_id.into(),
        ..assistant(content)
    }
}

#[test]
fn openai_and_chatgpt_history_retains_reasoning_in_every_representation() {
    let responses = responses_envelope(&[responses_reasoning(false).as_str(), RESPONSES_MESSAGE]);
    let encrypted = responses_envelope(&[responses_reasoning(true).as_str(), RESPONSES_MESSAGE]);
    let chat = "{\"Version\":1,\"Protocol\":\"chat-completions\",\"ReasoningContent\":\"thought\",\"Reasoning\":\"other\",\"ToolCalls\":[]}";
    let thinking = "[{\"type\":\"thinking\",\"thinking\":\"thought\",\"signature\":\"sig\"}]";
    for provider_id in ["openai", "chatgpt"] {
        for content in [
            responses.as_str(),
            encrypted.as_str(),
            chat,
            thinking,
            "unknown replay",
        ] {
            assert_eq!(
                filter_provider_batch(vec![provider_assistant(provider_id, content)]),
                None,
                "reasoning from {provider_id} must remain intact"
            );
        }
    }
}

#[test]
fn an_openai_origin_keeps_a_mixed_history_batch_intact() {
    let responses = responses_envelope(&[responses_reasoning(false).as_str(), RESPONSES_MESSAGE]);
    let chat = "{\"Version\":1,\"Protocol\":\"chat-completions\",\"Reasoning\":\"thought\"}";
    let messages = vec![
        user("switch to another provider"),
        provider_assistant("other", chat),
        provider_assistant("openai", &responses),
    ];
    assert_eq!(filter_provider_batch(messages), None);
}

#[test]
fn other_compatible_provider_history_still_loses_plain_reasoning() {
    let raw = responses_envelope(&[responses_reasoning(false).as_str(), RESPONSES_MESSAGE]);
    let filtered = filter_provider_batch(vec![provider_assistant("other", &raw)])
        .expect("other provider reasoning is removable");
    assert_eq!(
        filtered[0].provider_content,
        responses_envelope(&[RESPONSES_MESSAGE])
    );
}

#[test]
fn filter_trait_reports_the_replacement_batch() {
    let raw = responses_envelope(&[responses_reasoning(false).as_str(), RESPONSES_MESSAGE]);
    let request = FilterRequest {
        messages: vec![assistant(&raw)],
    };
    let filter = ReasoningSkipFilter::new(&Default::default());
    let mut future = filter.filter(request, CancellationToken::uncancelled());
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
