use super::Fixture;
use parrot_domain::protocol::messages::parrot::Event;
use parrot_storage::conversation::{
    ConversationMessage, ConversationOrigin, ConversationPart, MessageRole, ProviderContent,
    ToolCall,
};

#[tokio::test]
async fn embedded_provider_parts_calls_and_final_phase_survive_reopen() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    let mut message = ConversationMessage::text(MessageRole::Assistant, "unused");
    message.origin = ConversationOrigin::Model;
    message.parts = vec![
        ConversationPart::text("first\0"),
        ConversationPart::image("artifact", "image/webp", "image.webp"),
        ConversationPart::text("last"),
    ];
    message.tool_calls = vec![
        ToolCall {
            id: "z".into(),
            name: "first".into(),
            arguments_json: " { \"x\" : 1 } ".into(),
        },
        ToolCall {
            id: "a".into(),
            name: "second".into(),
            arguments_json: " [1, 2] ".into(),
        },
    ];
    message.provider_content = ProviderContent {
        provider_id: "provider".into(),
        content: " { \"signature\": \" x \", \"opaque\": [1, true] }\0".into(),
    };
    repository
        .append_conversation(
            Event {
                id: "rich".into(),
                agent_session_id: "agent".into(),
                ..Default::default()
            },
            message.clone(),
        )
        .await
        .unwrap();
    assert!(
        repository
            .append_final_provider_request_prompt(
                Event {
                    agent_session_id: "agent".into(),
                    ..Default::default()
                },
                "final instructions".into(),
            )
            .await
            .unwrap()
    );
    repository
        .append_conversation(
            Event {
                id: "tail".into(),
                agent_session_id: "agent".into(),
                ..Default::default()
            },
            ConversationMessage::text(MessageRole::Assistant, "tail"),
        )
        .await
        .unwrap();
    repository.events().database().close().await.unwrap();

    let reopened = fixture.repository();
    let items = reopened.conversation("agent".into()).await.unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0].message, message);
    assert_eq!(
        items[1].message.parts,
        [ConversationPart::text("final instructions")]
    );
    assert_eq!(items[2].message.parts, [ConversationPart::text("tail")]);
    assert_eq!(
        items
            .iter()
            .map(|item| item.final_phase)
            .collect::<Vec<_>>(),
        [false, true, true]
    );
    let retired_tables = reopened
        .events()
        .transaction(|tx| {
            Ok(tx.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name IN ('message','conversation_provider_content')",
                [],
                |row| row.get::<_, i64>(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(retired_tables, 0);
    reopened.events().database().close().await.unwrap();
}

#[tokio::test]
async fn empty_provider_defaults_and_derived_text_view_survive_reopen() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    let mut message = ConversationMessage::text(MessageRole::User, "unused");
    message.parts = vec![
        ConversationPart::text("first\0"),
        ConversationPart::text(""),
        ConversationPart::text("last"),
    ];
    message.provider_content = ProviderContent {
        provider_id: "ignored without content".into(),
        content: String::new(),
    };
    repository
        .append_conversation(
            Event {
                id: "user".into(),
                agent_session_id: "agent".into(),
                ..Default::default()
            },
            message,
        )
        .await
        .unwrap();
    repository
        .append_conversation(
            Event {
                id: "assistant".into(),
                agent_session_id: "agent".into(),
                ..Default::default()
            },
            ConversationMessage::text(MessageRole::Assistant, "done"),
        )
        .await
        .unwrap();
    repository.events().database().close().await.unwrap();

    let reopened = fixture.repository();
    let items = reopened.conversation("agent".into()).await.unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(
        items[0].message.provider_content,
        ProviderContent::default()
    );
    assert_eq!(
        items[1].message.provider_content,
        ProviderContent::default()
    );
    assert_eq!(
        reopened.messages("agent".into()).await.unwrap(),
        ["user: first\0last", "assistant: done"]
    );
    reopened.events().database().close().await.unwrap();
}
