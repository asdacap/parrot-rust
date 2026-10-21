use parrot_domain::contracts::paths::{RootOrigin, SelectedSessionRoot};
use parrot_domain::protocol::messages::parrot::{Event, RequestUsageRecorded};
use parrot_storage::artifacts::ImageArtifactStore;
use parrot_storage::conversation::{
    AgentHistoryEntry, CompactionSnapshot, ConversationItem, ConversationMessage,
    ConversationOrigin, ConversationPart, ConversationRepository, EventError, MessageRole,
    ModelContent, ProviderContent, ToolCall,
};
use parrot_storage::database::{DatabaseError, SessionDatabase};
use parrot_storage::events::EventRepository;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "parrot-t18-reads-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn root(&self) -> SelectedSessionRoot {
        SelectedSessionRoot::new(self.0.clone(), RootOrigin::Current).unwrap()
    }
    fn repository(&self) -> ConversationRepository {
        ConversationRepository::new(EventRepository::new(
            SessionDatabase::open(self.root()).unwrap(),
        ))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn event(id: &str, agent: &str) -> Event {
    Event {
        id: id.into(),
        agent_session_id: agent.into(),
        ..Default::default()
    }
}
fn message(role: MessageRole, text: &str) -> ConversationMessage {
    ConversationMessage::text(role, text)
}
async fn append(
    repo: &ConversationRepository,
    id: &str,
    agent: &str,
    message: ConversationMessage,
) -> ConversationItem {
    repo.events()
        .append(event(id, agent), Some(message))
        .await
        .unwrap()
        .conversation
        .unwrap()
}
fn request(id: &str, agent: &str, effort: Option<&str>) -> Event {
    let mut published = event(id, agent);
    published.set_request_usage_recorded(RequestUsageRecorded {
        request_id: id.into(),
        provider: "provider".into(),
        model: "model".into(),
        effort: effort.map(str::to_owned),
        input_tokens: 101,
        cached_input_tokens: 23,
        output_tokens: 17,
        input_cost: 0.25,
        output_cost: 0.5,
        tool_call_ids: vec!["call-a".into(), "call-b".into()],
        ..Default::default()
    });
    published
}
fn entry_label(entry: &AgentHistoryEntry) -> String {
    match entry {
        AgentHistoryEntry::Message { parts, .. } => {
            format!("message:{}", parrot_storage::events::types::content(parts))
        }
        AgentHistoryEntry::Compaction { summary, .. } => format!("compaction:{summary}"),
        AgentHistoryEntry::Request { request_id, .. } => format!("request:{request_id}"),
    }
}

#[tokio::test]
async fn structured_conversation_reopens_in_order_and_respects_watermark() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    let mut assistant = message(MessageRole::Assistant, "");
    assistant.tool_calls = vec![ToolCall {
        id: "call-1".into(),
        name: "read_image".into(),
        arguments_json: "{}".into(),
    }];
    append(&repo, "assistant", "agent", assistant).await;
    let mut tool = message(MessageRole::Tool, "read image");
    tool.tool_call_id = "call-1".into();
    append(&repo, "tool", "agent", tool).await;
    let mut images = message(MessageRole::User, "");
    images.origin = ConversationOrigin::Tool;
    images.parts = vec![ConversationPart::image(
        "a".repeat(64),
        "image/png",
        "pixel.png",
    )];
    append(&repo, "images", "agent", images).await;
    repo.events().database().close().await.unwrap();
    let restored = fixture.repository();
    let items = restored.conversation("agent".into()).await.unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0].message.tool_calls.len(), 1);
    assert_eq!(items[0].message.tool_calls[0].id, "call-1");
    assert_eq!(items[1].message.tool_call_id, "call-1");
    assert_eq!(
        items[2].message.parts,
        vec![ConversationPart::image(
            "a".repeat(64),
            "image/png",
            "pixel.png"
        )]
    );
    assert_eq!(
        restored
            .conversation_after("agent".into(), items[1].sequence)
            .await
            .unwrap()
            .len(),
        1
    );
    restored.events().database().close().await.unwrap();
}

#[tokio::test]
async fn agent_history_orders_structured_messages_and_retains_compactions() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    let mut assistant = message(MessageRole::Assistant, "before");
    assistant.tool_calls = vec![ToolCall {
        id: "call-1".into(),
        name: "read".into(),
        arguments_json: "{\"path\":\"file\"}".into(),
    }];
    append(&repo, "assistant", "agent", assistant).await;
    assert!(
        repo.save_compaction(
            "agent".into(),
            CompactionSnapshot {
                summary: "summary one".into(),
                watermark: 1
            }
        )
        .await
        .unwrap()
    );
    let mut tool = message(MessageRole::Tool, "result");
    tool.tool_call_id = "call-1".into();
    append(&repo, "tool", "agent", tool).await;
    assert!(
        repo.save_compaction(
            "agent".into(),
            CompactionSnapshot {
                summary: "summary two".into(),
                watermark: 2
            }
        )
        .await
        .unwrap()
    );
    let history = repo.agent_history("agent".into()).await.unwrap();
    assert_eq!(history.len(), 4);
    let kinds: Vec<_> = history
        .iter()
        .map(|entry| match entry {
            AgentHistoryEntry::Message { .. } => "AgentHistoryMessageEntry",
            AgentHistoryEntry::Compaction { .. } => "AgentHistoryCompactionEntry",
            _ => "AgentHistoryRequestEntry",
        })
        .collect();
    assert_eq!(
        kinds.join(","),
        "AgentHistoryMessageEntry,AgentHistoryCompactionEntry,AgentHistoryMessageEntry,AgentHistoryCompactionEntry"
    );
    match &history[0] {
        AgentHistoryEntry::Message {
            role, tool_calls, ..
        } => {
            assert_eq!(role, "assistant");
            assert_eq!(tool_calls.len(), 1);
            assert!(tool_calls[0].arguments_json.contains("file"));
        }
        _ => panic!("expected first message"),
    }
    let summaries: Vec<_> = history
        .iter()
        .filter_map(|entry| match entry {
            AgentHistoryEntry::Compaction { summary, .. } => Some(summary.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(summaries.join(","), "summary one,summary two");
    assert!(
        !repo
            .save_compaction(
                "agent".into(),
                CompactionSnapshot {
                    summary: "duplicate".into(),
                    watermark: 2
                }
            )
            .await
            .unwrap()
    );
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn structured_parts_calls_provider_and_legacy_model_rendering_are_exact() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    let mut projected = message(MessageRole::Assistant, "unused");
    projected.parts = vec![
        ConversationPart::text("first"),
        ConversationPart::text(""),
        ConversationPart::text("last"),
    ];
    projected.tool_calls = vec![
        ToolCall {
            id: "z".into(),
            name: "tool-z".into(),
            arguments_json: "{\"x\":2}".into(),
        },
        ToolCall {
            id: "a".into(),
            name: "tool-a".into(),
            arguments_json: " [ 1 ] ".into(),
        },
    ];
    projected.provider_content = ProviderContent {
        provider_id: "anthropic".into(),
        content: "[\"opaque\", {\"signature\":\" x \"}]".into(),
    };
    let expected = append(&repo, "structured", "agent", projected).await;
    append(&repo, "other", "other", message(MessageRole::User, "other")).await;
    let model = repo.model_history("agent".into()).await.unwrap();
    assert_eq!(model.len(), 1);
    assert_eq!(model[0].role, MessageRole::Assistant);
    assert_eq!(
        model[0].contents,
        vec![
            ModelContent::Text("first".into()),
            ModelContent::Text("".into()),
            ModelContent::Text("last".into())
        ]
    );
    assert_eq!(model[0].tool_calls, expected.message.tool_calls);
    assert_eq!(model[0].provider_content, expected.message.provider_content);
    assert_eq!(
        repo.messages("agent".into()).await.unwrap(),
        ["assistant: firstlast"]
    );
    repo.events().database().close().await.unwrap();
    std::fs::write(
        fixture.0.join("history.jsonl"),
        b"corrupt non-authoritative projection",
    )
    .unwrap();
    let reopened = fixture.repository();
    assert_eq!(
        reopened.conversation("agent".into()).await.unwrap(),
        [expected]
    );
    assert!(matches!(
        reopened.conversation_after("agent".into(), -1).await,
        Err(EventError::Argument(_))
    ));
    reopened.events().database().close().await.unwrap();
}

#[tokio::test]
async fn legacy_model_rejects_images_while_materialize_preserves_text_and_errors() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    let image = ConversationPart::image("a".repeat(64), "image/png", "display");
    let mut projected = message(MessageRole::User, "");
    projected.parts = vec![ConversationPart::text("before"), image.clone()];
    append(&repo, "image", "agent", projected).await;
    for result in [
        repo.model_history("agent".into()).await.map(|_| ()),
        repo.messages("agent".into()).await.map(|_| ()),
    ] {
        assert!(
            matches!(result, Err(EventError::Invalid(message)) if message == "artifact references must be materialized before model history is read")
        );
    }
    assert_eq!(
        repo.materialize(vec![
            ConversationPart::text("a"),
            ConversationPart::text("")
        ])
        .await
        .unwrap(),
        [
            ModelContent::Text("a".into()),
            ModelContent::Text("".into())
        ]
    );
    assert!(
        matches!(repo.materialize(vec![image]).await, Err(EventError::Invalid(message)) if message == "image artifacts cannot be materialized without session resources")
    );
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn materialized_images_use_selected_metadata_but_part_media_type_and_no_file_fallback() {
    let fixture = Fixture::new();
    let store = ImageArtifactStore::for_session(&fixture.root(), "images").unwrap();
    std::fs::create_dir_all(store.directory()).unwrap();
    let id = "a".repeat(64);
    let path = store.directory().join(format!("{id}.png"));
    std::fs::write(
        &path,
        b"materialization resolves existing paths without decoding file bytes",
    )
    .unwrap();
    let repo = fixture.repository().with_image_store(store).unwrap();
    let insert = id.clone();
    repo.events().transaction(move |tx| {
        tx.execute("INSERT INTO image_content (sha256, media_type, byte_length, width, height, frame_count, aggregate_pixels) VALUES (?1, 'image/png', 1, 24, 12, 1, 288)", [&insert])?;
        tx.execute("INSERT INTO image_artifact (artifact_id, sha256, display_name, origin, created_at) VALUES (?1, ?1, 'stored name', 'test', 'at')", [&insert])?; Ok(())
    }).await.unwrap();
    let metadata_only = ConversationRepository::new(repo.events().clone());
    let resolved = metadata_only
        .resolve_image_artifact(id.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            resolved.width,
            resolved.height,
            resolved.media_type.as_str(),
            resolved.display_name.as_str()
        ),
        (24, 12, "image/png", "stored name")
    );
    assert!(
        metadata_only
            .resolve_image_artifact("b".repeat(64))
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        metadata_only.resolve_image_artifact(" ".into()).await,
        Err(EventError::Argument(_))
    ));
    let parts = vec![
        ConversationPart::text("start"),
        ConversationPart::image(id.clone(), "part/media-type", "ignored name"),
        ConversationPart::text("end"),
    ];
    assert_eq!(
        repo.materialize(parts).await.unwrap(),
        [
            ModelContent::Text("start".into()),
            ModelContent::ImageFile {
                path: path.clone(),
                media_type: "part/media-type".into(),
                width: 24,
                height: 12
            },
            ModelContent::Text("end".into())
        ]
    );
    let missing = repo
        .materialize(vec![ConversationPart::image(
            "b".repeat(64),
            "image/png",
            "",
        )])
        .await
        .unwrap_err();
    assert!(
        matches!(missing, EventError::Database(DatabaseError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound)
    );
    std::fs::remove_file(path).unwrap();
    assert!(
        matches!(repo.materialize(vec![ConversationPart::image(id, "image/png", "")]).await, Err(EventError::Database(DatabaseError::Io(error))) if error.kind() == std::io::ErrorKind::NotFound)
    );
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn merged_request_history_stably_interleaves_independent_cursors_and_optional_effort() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    let first = repo
        .events()
        .append_usage_fact(request("before", "agent", None))
        .await
        .unwrap();
    let first_message = append(
        &repo,
        "first-message",
        "agent",
        message(MessageRole::User, "first"),
    )
    .await;
    let r1 = repo
        .events()
        .append_usage_fact(request("same-anchor-1", "agent", None))
        .await
        .unwrap();
    let r2 = repo
        .events()
        .append_usage_fact(request("same-anchor-2", "agent", Some("")))
        .await
        .unwrap();
    append(
        &repo,
        "other-message",
        "other",
        message(MessageRole::User, "other"),
    )
    .await;
    repo.events()
        .append_usage_fact(request("other-request", "other", Some("high")))
        .await
        .unwrap();
    append(
        &repo,
        "last-message",
        "agent",
        message(MessageRole::Assistant, "last"),
    )
    .await;
    repo.events()
        .append(request("legacy-no-anchor", "agent", Some("high")), None)
        .await
        .unwrap();
    repo.events()
        .append_usage_fact(request("only-request", "request-only", None))
        .await
        .unwrap();
    repo.events()
        .append(request("unanchored-only", "unanchored-only", None), None)
        .await
        .unwrap();
    let history = repo.agent_history("agent".into()).await.unwrap();
    assert_eq!(
        history.iter().map(entry_label).collect::<Vec<_>>(),
        [
            "request:before",
            "request:legacy-no-anchor",
            "message:first",
            "request:same-anchor-1",
            "request:same-anchor-2",
            "message:last"
        ]
    );
    let history_sequence = history
        .iter()
        .find_map(|entry| match entry {
            AgentHistoryEntry::Message {
                sequence,
                conversation_sequence,
                ..
            } if *conversation_sequence == first_message.sequence => Some(*sequence),
            _ => None,
        })
        .unwrap();
    let requests: Vec<_> = history
        .iter()
        .filter_map(|entry| match entry {
            AgentHistoryEntry::Request {
                sequence,
                event_sequence,
                request_id,
                provider,
                model,
                effort,
                input_tokens,
                cached_input_tokens,
                output_tokens,
                input_cost,
                output_cost,
                tool_calls,
            } => Some((
                *sequence,
                *event_sequence,
                request_id.as_str(),
                provider.as_str(),
                model.as_str(),
                effort.clone(),
                *input_tokens,
                *cached_input_tokens,
                *output_tokens,
                *input_cost,
                *output_cost,
                *tool_calls,
            )),
            _ => None,
        })
        .collect();
    assert_eq!(requests[0].0, 0);
    assert_eq!(requests[0].1, first);
    assert_eq!(requests[2].0, history_sequence);
    assert_eq!(requests[2].1, r1);
    assert_eq!(requests[3].0, history_sequence);
    assert_eq!(requests[3].1, r2);
    assert_eq!(requests[2].5, None);
    assert_eq!(requests[3].5, Some(String::new()));
    assert_eq!(
        (
            requests[3].3,
            requests[3].4,
            requests[3].6,
            requests[3].7,
            requests[3].8,
            requests[3].9,
            requests[3].10,
            requests[3].11
        ),
        ("provider", "model", 101, 23, 17, 0.25, 0.5, 2)
    );
    assert_eq!(
        repo.agent_history_after("agent".into(), history_sequence, r1)
            .await
            .unwrap()
            .iter()
            .map(entry_label)
            .collect::<Vec<_>>(),
        [
            "request:legacy-no-anchor",
            "request:same-anchor-2",
            "message:last"
        ]
    );
    assert_eq!(
        repo.agent_history_after("agent".into(), -1, -1)
            .await
            .unwrap(),
        history
    );
    assert_eq!(
        repo.agent_history_session_ids().await.unwrap(),
        ["agent", "other", "request-only"]
    );
    repo.events().database().close().await.unwrap();
    let reopened = fixture.repository();
    assert_eq!(
        reopened.agent_history("agent".into()).await.unwrap(),
        history
    );
    reopened.events().database().close().await.unwrap();
}

#[tokio::test]
async fn compaction_tail_excludes_all_associated_status_items_not_just_current_status() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    let first = append(&repo, "first", "agent", message(MessageRole::User, "first")).await;
    let old = append(
        &repo,
        "old-status",
        "agent",
        message(MessageRole::System, "old"),
    )
    .await;
    let last = append(
        &repo,
        "last",
        "agent",
        message(MessageRole::Assistant, "last"),
    )
    .await;
    let current = append(
        &repo,
        "current-status",
        "agent",
        message(MessageRole::System, "current"),
    )
    .await;
    repo.events().transaction(move |tx| { tx.execute("INSERT INTO compaction_status (agent_session, watermark, status_conversation_sequence) VALUES ('agent', 0, ?1), ('agent', 1, ?2)", rusqlite::params![old.sequence, current.sequence])?; Ok(()) }).await.unwrap();
    let tail = repo
        .events()
        .transaction(|tx| parrot_storage::conversation::reads::read_compaction_tail(tx, "agent", 0))
        .await
        .unwrap();
    assert_eq!(tail, [first.clone(), last.clone()]);
    let after = repo
        .events()
        .transaction(move |tx| {
            parrot_storage::conversation::reads::read_compaction_tail(tx, "agent", first.sequence)
        })
        .await
        .unwrap();
    assert_eq!(after, [last]);
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn malformed_conversation_metadata_and_missing_item_are_observable_without_partial_projection()
 {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    repo.events().transaction(|tx| { tx.execute("INSERT INTO message (agent_session, role, content, created_at) VALUES ('agent', 'unexpected', 'legacy', 'at')", [])?; Ok(()) }).await.unwrap();
    assert!(
        matches!(repo.conversation("agent".into()).await, Err(EventError::Invalid(message)) if message == "unknown conversation role unexpected")
    );
    let counts = repo
        .events()
        .transaction(|tx| {
            Ok((
                tx.query_row("SELECT COUNT(*) FROM conversation_item", [], |row| {
                    row.get::<_, i64>(0)
                })?,
                tx.query_row(
                    "SELECT COUNT(*) FROM projection_version WHERE name = 'conversation'",
                    [],
                    |row| row.get::<_, i64>(0),
                )?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(counts, (0, 0));
    assert!(
        matches!(repo.events().transaction(|tx| parrot_storage::conversation::reads::read_conversation_item(tx, 42)).await, Err(EventError::Invalid(message)) if message == "Conversation item 42 does not exist.")
    );
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn delegated_event_projection_has_exactly_one_readable_prompt() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    append(
        &repo,
        "e1",
        "agent",
        message(MessageRole::User, "the prompt"),
    )
    .await;
    repo.events()
        .append(event("e2", "agent"), None)
        .await
        .unwrap();
    let messages = repo.messages("agent".into()).await.unwrap();
    assert_eq!(messages.len(), 1);
    assert!(messages[0].contains("the prompt"));
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn delegated_legacy_upgrade_request_history_keeps_single_original_tool_call() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.0.join("session.db"),
        include_bytes!("../database/fixtures/legacy-canonical-before.sqlite"),
    )
    .unwrap();
    let repo = fixture.repository();
    let mut duplicate = repo
        .events()
        .replay()
        .await
        .unwrap()
        .into_iter()
        .find(|event| event.id == "request")
        .unwrap();
    duplicate.id = "duplicate-request".into();
    assert!(matches!(
        repo.events().append_usage_fact(duplicate).await,
        Err(EventError::Invalid(_))
    ));
    let history = repo.agent_history("agent".into()).await.unwrap();
    let requests: Vec<_> = history
        .iter()
        .filter_map(|entry| match entry {
            AgentHistoryEntry::Request { tool_calls, .. } => Some(*tool_calls),
            _ => None,
        })
        .collect();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0], 1);
    repo.events().database().close().await.unwrap();
}
