use parrot_domain::contracts::paths::{RootOrigin, SelectedSessionRoot};
use parrot_domain::protocol::messages::parrot::{Event, RequestUsageRecorded};
use parrot_storage::artifacts::{ImageArtifactRepository, ImageArtifactStore};
use parrot_storage::conversation::{
    AgentHistoryEntry, CompactionSnapshot, ConversationItem, ConversationMessage,
    ConversationOrigin, ConversationPart, ConversationRepository, EventError, MessageRole,
    ModelContent, ProviderContent, ToolCall,
};
use parrot_storage::database::{DatabaseError, SessionDatabase};
use parrot_storage::events::{EventRepository, NeverCancelled};
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
        let events = EventRepository::new(SessionDatabase::open(self.root()).unwrap());
        let root = events.database().selected_root().clone();
        let id = parrot_domain::values::identifiers::UserSessionId::parse(
            root.path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        )
        .unwrap();
        let metadata =
            parrot_storage::history_files::SessionIndex::new(root.clone(), id, root.path())
                .unwrap();
        ConversationRepository::new(events, metadata).unwrap()
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
async fn latest_assistant_is_session_scoped_and_ignores_trailing_non_model_messages() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    assert_eq!(
        repo.read_latest_assistant("agent".into()).await.unwrap(),
        None
    );
    let mut batch = message(MessageRole::Assistant, "tools");
    batch.tool_calls.push(ToolCall {
        id: "old-call".into(),
        name: "read".into(),
        arguments_json: "{}".into(),
    });
    let old = append(&repo, "old", "agent", batch).await;
    assert_eq!(
        repo.read_latest_assistant("agent".into()).await.unwrap(),
        Some(old)
    );
    let latest = append(
        &repo,
        "latest",
        "agent",
        message(MessageRole::Assistant, "done"),
    )
    .await;
    let other = append(
        &repo,
        "other",
        "other",
        message(MessageRole::Assistant, "other"),
    )
    .await;
    for (id, role) in [
        ("input", MessageRole::User),
        ("result", MessageRole::Tool),
        ("status", MessageRole::System),
    ] {
        append(&repo, id, "agent", message(role, id)).await;
    }
    let mut non_model = message(MessageRole::Assistant, "not a provider response");
    non_model.origin = ConversationOrigin::System;
    append(&repo, "non-model", "agent", non_model).await;
    repo.save_compaction(
        "agent".into(),
        CompactionSnapshot {
            summary: "summary".into(),
            watermark: latest.sequence,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        repo.read_latest_assistant("agent".into()).await.unwrap(),
        Some(latest.clone())
    );
    assert_eq!(
        repo.read_latest_assistant("other".into()).await.unwrap(),
        Some(other)
    );
    assert_eq!(
        repo.read_latest_assistant("empty".into()).await.unwrap(),
        None
    );
    repo.events()
        .transaction(|tx| {
            tx.execute_batch("DROP INDEX conversation_latest_assistant;")?;
            Ok(())
        })
        .await
        .unwrap();
    repo.events().database().close().await.unwrap();
    let reopened = fixture.repository();
    assert_eq!(
        reopened
            .read_latest_assistant("agent".into())
            .await
            .unwrap(),
        Some(latest)
    );
    let plan = reopened.events().transaction(|tx| {
        let mut statement = tx.prepare("EXPLAIN QUERY PLAN SELECT sequence FROM conversation_item WHERE agent_session = ?1 AND origin = 'model' AND role = 'assistant' AND kind <> 'compaction' ORDER BY sequence DESC LIMIT 1;")?;
        Ok(statement.query_map(["agent"], |row| row.get::<_, String>(3))?.collect::<Result<Vec<_>, _>>()?)
    }).await.unwrap();
    println!("latest assistant query plan: {plan:?}");
    assert!(plan.iter().any(|detail| {
        detail.contains("SEARCH conversation_item USING INDEX conversation_latest_assistant")
    }));
    assert!(
        !plan
            .iter()
            .any(|detail| detail.contains("SCAN") || detail.contains("TEMP B-TREE"))
    );
    reopened.events().database().close().await.unwrap();
}

#[tokio::test]
async fn structured_conversation_reopens_in_order_and_respects_watermark() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    let mut assistant = message(MessageRole::Assistant, "before");
    assistant.parts.push(super::Fixture::image(
        "a".repeat(64),
        "image/png",
        "pixel.png",
    ));
    assistant.parts.push(ConversationPart::text("after"));
    assistant.tool_calls = vec![
        ToolCall {
            id: "call-1".into(),
            name: "read_image".into(),
            arguments_json: "{}".into(),
        },
        ToolCall {
            id: "call-2".into(),
            name: "read".into(),
            arguments_json: " { \"path\": \"file\" } ".into(),
        },
    ];
    append(&repo, "assistant", "agent", assistant.clone()).await;
    let mut tool = message(MessageRole::Tool, "read image");
    tool.tool_call_id = "call-1".into();
    append(&repo, "tool", "agent", tool).await;
    let mut images = message(MessageRole::User, "");
    images.origin = ConversationOrigin::Tool;
    images.parts = vec![super::Fixture::image(
        "a".repeat(64),
        "image/png",
        "pixel.png",
    )];
    append(&repo, "images", "agent", images).await;
    repo.events().database().close().await.unwrap();
    let restored = fixture.repository();
    let items = restored.conversation("agent".into()).await.unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0].message, assistant);
    assert_eq!(items[1].message.tool_call_id, "call-1");
    assert_eq!(
        items[2].message.parts,
        vec![super::Fixture::image(
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
    let image = super::Fixture::image("a".repeat(64), "image/png", "display");
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
async fn legacy_model_history_excludes_tool_images_without_removing_raw_result_parts() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    let mut tool = message(MessageRole::Tool, "before");
    tool.tool_call_id = "call".into();
    tool.parts.push(super::Fixture::image(
        "unresolved-tool-image",
        "image/png",
        "image",
    ));
    tool.parts.push(ConversationPart::text("after"));
    let item = append(&repo, "tool-image", "agent", tool.clone()).await;
    assert_eq!(item.message, tool);
    let model = repo.model_history("agent".into()).await.unwrap();
    assert_eq!(model.len(), 1);
    assert_eq!(model[0].role, MessageRole::Tool);
    assert_eq!(model[0].tool_call_id, "call");
    assert_eq!(
        model[0].contents,
        [
            ModelContent::Text("before".into()),
            ModelContent::Text("after".into())
        ]
    );
    assert_eq!(repo.conversation("agent".into()).await.unwrap(), [item]);
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn materialized_images_use_part_metadata_without_decoding_bytes_or_file_fallback() {
    let fixture = Fixture::new();
    let store = ImageArtifactStore::for_session(&fixture.root(), "images").unwrap();
    let repo = fixture
        .repository()
        .with_image_store(store.clone())
        .unwrap();
    let images =
        ImageArtifactRepository::new(repo.events().database().clone(), store.clone()).unwrap();
    let artifact = images
        .persist(
            std::io::Cursor::new(include_bytes!("../artifacts/fixtures/rectangle.png")),
            "stored name",
            "test",
            None,
            None,
            &NeverCancelled,
        )
        .await
        .unwrap();
    let id = artifact.artifact_id;
    let path = store.path_of(&id).unwrap();
    std::fs::write(
        &path,
        b"materialization resolves existing paths without decoding file bytes",
    )
    .unwrap();
    let parts = vec![
        ConversationPart::text("start"),
        super::Fixture::image(id.clone(), "part/media-type", "ignored name"),
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
        .materialize(vec![super::Fixture::image("b".repeat(64), "image/png", "")])
        .await
        .unwrap_err();
    assert!(
        matches!(missing, EventError::Database(DatabaseError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound)
    );
    std::fs::remove_file(path).unwrap();
    assert!(
        matches!(repo.materialize(vec![super::Fixture::image(id, "image/png", "")]).await, Err(EventError::Database(DatabaseError::Io(error))) if error.kind() == std::io::ErrorKind::NotFound)
    );
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn image_materialization_does_not_require_upload_or_artifact_metadata_rows() {
    let fixture = Fixture::new();
    let store = ImageArtifactStore::for_session(&fixture.root(), "artifacts").unwrap();
    let repo = fixture
        .repository()
        .with_image_store(store.clone())
        .unwrap();
    let images =
        ImageArtifactRepository::new(repo.events().database().clone(), store.clone()).unwrap();
    let artifact = images
        .persist(
            std::io::Cursor::new(include_bytes!("../artifacts/fixtures/rectangle.png")),
            "rectangle.png",
            "test",
            None,
            None,
            &NeverCancelled,
        )
        .await
        .unwrap();
    let part = ConversationPart::image(artifact.clone());
    repo.events()
        .database()
        .admit("remove-image-registries", &NeverCancelled, |tx| {
            tx.execute_batch(
                "DROP TABLE IF EXISTS image_upload; DROP TABLE IF EXISTS image_artifact;",
            )?;
            Ok(())
        })
        .unwrap()
        .into_completion()
        .await
        .unwrap();
    assert_eq!(
        repo.materialize(vec![part]).await.unwrap(),
        [ModelContent::ImageFile {
            path: store.path_of(&artifact.artifact_id).unwrap(),
            media_type: "image/png".into(),
            width: 24,
            height: 12,
        }]
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
    repo.save_compaction(
        "agent".into(),
        CompactionSnapshot {
            summary: "summary".into(),
            watermark: first_message.sequence,
        },
    )
    .await
    .unwrap();
    repo.events()
        .append_usage_fact(request("after-compaction", "agent", None))
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
            "compaction:summary",
            "request:after-compaction",
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
            "compaction:summary",
            "request:after-compaction",
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
    for (summary, watermark, status) in [
        ("old", 0, old.sequence),
        ("current", first.sequence, current.sequence),
    ] {
        repo.save_compaction(
            "agent".into(),
            CompactionSnapshot {
                summary: summary.into(),
                watermark,
            },
        )
        .await
        .unwrap();
        repo.events().transaction(move |tx| {
            tx.execute("UPDATE conversation_item SET compaction_status_sequence = ?1 WHERE agent_session = 'agent' AND kind = 'compaction' AND compaction_watermark = ?2", rusqlite::params![status, watermark])?;
            Ok(())
        }).await.unwrap();
    }
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
async fn malformed_conversation_metadata_and_missing_item_are_observable_without_mutation() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    repo.events().transaction(|tx| {
        tx.execute("INSERT INTO conversation_item (agent_session,origin,role,tool_call_id,created_at) VALUES ('agent','model','unexpected','','at')", [])?;
        Ok(())
    }).await.unwrap();
    assert!(
        matches!(repo.conversation("agent".into()).await, Err(EventError::Invalid(message)) if message == "unknown conversation role unexpected")
    );
    let state = repo.events().transaction(|tx| {
        Ok(tx.query_row("SELECT role,(SELECT count(*) FROM conversation_item),(SELECT count(*) FROM conversation_part) FROM conversation_item", [], |row| Ok((row.get::<_, String>(0)?,row.get::<_, i64>(1)?,row.get::<_, i64>(2)?)))?)
    }).await.unwrap();
    assert_eq!(state, ("unexpected".into(), 1, 0));
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
