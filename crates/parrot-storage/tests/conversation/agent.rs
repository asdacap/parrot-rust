use super::Fixture;
use parrot_domain::contracts::async_ops::BoxFuture;
use parrot_domain::protocol::messages::parrot::{Delivery, Event, InputAdmitted};
use parrot_storage::conversation::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

struct Projection {
    root: parrot_domain::contracts::paths::SelectedSessionRoot,
    agent: String,
    fail_refresh: AtomicBool,
    entries_seen: Mutex<Vec<usize>>,
}
impl Projection {
    fn new(agent: &str, root: parrot_domain::contracts::paths::SelectedSessionRoot) -> Self {
        Self {
            root,
            agent: agent.into(),
            fail_refresh: AtomicBool::new(false),
            entries_seen: Mutex::new(vec![]),
        }
    }
}
impl HistoryProjection for Projection {
    fn selected_root(&self) -> &parrot_domain::contracts::paths::SelectedSessionRoot {
        &self.root
    }
    fn validate_session(&self, agent: &str) -> Result<(), EventError> {
        if agent != self.agent {
            return Err(EventError::Argument(
                "history file belongs to another agent session".into(),
            ));
        }
        Ok(())
    }
    fn refresh<'a>(
        &'a self,
        repo: &'a ConversationRepository,
        agent: &'a str,
    ) -> BoxFuture<'a, Result<(), EventError>> {
        Box::pin(async move {
            let entries = repo.agent_history(agent.into()).await?;
            self.entries_seen.lock().unwrap().push(entries.len());
            if self.fail_refresh.load(Ordering::SeqCst) {
                return Err(EventError::Invalid("injected derived-file failure".into()));
            }
            Ok(())
        })
    }
    fn rebuild<'a>(
        &'a self,
        repo: &'a ConversationRepository,
        agent: &'a str,
    ) -> BoxFuture<'a, Result<(), EventError>> {
        self.refresh(repo, agent)
    }
}
fn event(id: &str, agent: &str) -> Event {
    Event {
        id: id.into(),
        agent_session_id: agent.into(),
        ..Default::default()
    }
}
fn model(role: MessageRole, text: &str) -> ModelMessage {
    ModelMessage {
        role,
        contents: vec![ModelContent::Text(text.into())],
        tool_calls: vec![],
        tool_call_id: String::new(),
        provider_content: ProviderContent::default(),
    }
}

#[tokio::test]
async fn live_and_restored_tool_messages_exclude_images_but_preserve_raw_results() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    let history = repo
        .bind_agent_history(
            "agent".into(),
            Arc::new(Projection::new(
                "agent",
                repo.events().database().selected_root().clone(),
            )),
        )
        .unwrap();
    let mut tool = ConversationMessage::text(MessageRole::Tool, "answer");
    tool.tool_call_id = "call".into();
    tool.parts.push(ConversationPart::image(
        "unresolved-tool-image",
        "image/png",
        "image",
    ));
    let item = history
        .append_conversation(event("tool-image", "agent"), tool.clone())
        .await
        .unwrap();
    let mut conversation = AgentConversation::new(history.clone()).await.unwrap();
    let expected = ModelMessage {
        role: MessageRole::Tool,
        contents: vec![ModelContent::Text("answer".into())],
        tool_calls: vec![],
        tool_call_id: "call".into(),
        provider_content: ProviderContent::default(),
    };
    assert_eq!(conversation.messages(), [expected.clone()]);
    assert_eq!(
        conversation.materialize(item.clone()).await.unwrap(),
        expected
    );
    conversation.reload().await.unwrap();
    assert_eq!(conversation.messages(), [expected]);
    assert_eq!(history.conversation().await.unwrap(), [item]);
    assert_eq!(
        repo.conversation("agent".into()).await.unwrap()[0].message,
        tool
    );
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn bound_raw_conversation_state_reads_only_its_agent_and_does_not_refresh_projection() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    let projection = Arc::new(Projection::new(
        "agent",
        repo.events().database().selected_root().clone(),
    ));
    let history = repo
        .bind_agent_history("agent".into(), projection.clone())
        .unwrap();
    let empty = history.read_conversation_state().await.unwrap();
    assert_eq!(empty.snapshot, None);
    assert!(empty.items.is_empty());
    assert!(empty.checkpoint_sequences.is_empty());
    repo.events()
        .append(
            event("owned", "agent"),
            Some(ConversationMessage::text(MessageRole::User, "owned")),
        )
        .await
        .unwrap();
    repo.events()
        .append(
            event("other", "other"),
            Some(ConversationMessage::text(MessageRole::User, "other")),
        )
        .await
        .unwrap();
    assert_eq!(
        history.read_conversation_state().await.unwrap(),
        repo.read_conversation_state("agent".into()).await.unwrap()
    );
    assert_eq!(
        history.read_conversation_state().await.unwrap().items.len(),
        1
    );
    assert!(projection.entries_seen.lock().unwrap().is_empty());
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn last_visible_turn_is_absent_for_empty_or_system_only_conversations() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    let history = repo
        .bind_agent_history(
            "agent".into(),
            Arc::new(Projection::new(
                "agent",
                repo.events().database().selected_root().clone(),
            )),
        )
        .unwrap();
    let mut conversation = AgentConversation::new(history).await.unwrap();
    assert_eq!(conversation.read_last_turn_messages(), None);
    conversation.add(model(MessageRole::System, "summary"));
    conversation.add(model(MessageRole::System, "status"));
    repo.events().database().close().await.unwrap();
    assert_eq!(conversation.read_last_turn_messages(), None);
    assert!(!conversation.is_answerable());
}

#[tokio::test]
async fn last_visible_turn_retains_tools_candidates_and_trailing_notices_after_latest_user() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    let history = repo
        .bind_agent_history(
            "agent".into(),
            Arc::new(Projection::new(
                "agent",
                repo.events().database().selected_root().clone(),
            )),
        )
        .unwrap();
    let mut conversation = AgentConversation::new(history).await.unwrap();
    conversation.add(model(MessageRole::User, "older question"));
    conversation.add(model(MessageRole::Assistant, "older answer"));
    conversation.add(model(MessageRole::System, "between turns"));
    let mut tool_request = model(MessageRole::Assistant, "tool request");
    tool_request.tool_calls.push(ToolCall {
        id: "call".into(),
        name: "read".into(),
        arguments_json: " { \"raw\" : 1.2300 } ".into(),
    });
    let mut tool_result = model(MessageRole::Tool, "tool result");
    tool_result.tool_call_id = "call".into();
    let expected = vec![
        model(MessageRole::User, "latest question"),
        tool_request,
        tool_result,
        model(MessageRole::Assistant, "retained candidate one"),
        model(MessageRole::System, "retry notice"),
        model(MessageRole::Assistant, "retained candidate two"),
        model(MessageRole::System, "trailing notice"),
    ];
    for message in &expected {
        conversation.add(message.clone());
    }
    repo.events().database().close().await.unwrap();
    assert_eq!(conversation.read_last_turn_messages(), Some(expected));
    assert_eq!(conversation.len(), 10);
    assert!(!conversation.is_answerable());
    let latest = model(MessageRole::User, "newer question");
    conversation.add(latest.clone());
    assert_eq!(conversation.read_last_turn_messages(), Some(vec![latest]));
    assert!(conversation.is_answerable());
}

#[tokio::test]
async fn last_visible_turn_keeps_compacted_context_when_no_user_remains_visible() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    let history = repo
        .bind_agent_history(
            "agent".into(),
            Arc::new(Projection::new(
                "agent",
                repo.events().database().selected_root().clone(),
            )),
        )
        .unwrap();
    let before = history
        .append_conversation(
            event("before", "agent"),
            ConversationMessage::text(MessageRole::User, "compacted question"),
        )
        .await
        .unwrap();
    history
        .append_compaction_status(
            event("compacted", "agent"),
            CompactionSnapshot {
                summary: "visible summary".into(),
                watermark: before.sequence,
            },
            "visible status".into(),
        )
        .await
        .unwrap();
    let mut request = ConversationMessage::text(MessageRole::Assistant, "continued tool call");
    request.tool_calls.push(ToolCall {
        id: "call".into(),
        name: "read".into(),
        arguments_json: "{}".into(),
    });
    history
        .append_conversation(event("request", "agent"), request)
        .await
        .unwrap();
    let mut result = ConversationMessage::text(MessageRole::Tool, "restored result");
    result.tool_call_id = "call".into();
    history
        .append_conversation(event("result", "agent"), result)
        .await
        .unwrap();
    let mut conversation = AgentConversation::new(history).await.unwrap();
    let mut expected = conversation.messages();
    assert!(
        expected
            .iter()
            .all(|message| message.role != MessageRole::User)
    );
    assert!(
        expected
            .iter()
            .any(|message| message.role == MessageRole::Tool)
    );
    let notice = model(MessageRole::System, "notice after restored result");
    conversation.add(notice.clone());
    expected.push(notice);
    repo.events().database().close().await.unwrap();
    assert_eq!(conversation.read_last_turn_messages(), Some(expected));
    assert!(conversation.is_answerable());
}

#[tokio::test]
async fn bound_pending_peek_preserves_raw_image_parts_without_materializing_or_promoting() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    let history = repo
        .bind_agent_history(
            "agent".into(),
            Arc::new(Projection::new(
                "agent",
                repo.events().database().selected_root().clone(),
            )),
        )
        .unwrap();
    let parts = vec![
        ConversationPart::text("prefix"),
        ConversationPart::image("absent-image-artifact", "image/png", "not materialized"),
        ConversationPart::text("suffix"),
    ];
    let admitted = history
        .admit_parts(
            "message".into(),
            parts.clone(),
            Delivery::DELIVERY_QUEUE,
            |input| {
                let mut published = event("admitted", "agent");
                published.set_input_admitted(InputAdmitted {
                    input_id: input.id.clone(),
                    message_id: input.message_id.clone(),
                    content: input.content(),
                    delivery: input.delivery.into(),
                    ..Default::default()
                });
                Ok(published)
            },
        )
        .await
        .unwrap();
    for _ in 0..3 {
        assert_eq!(
            history.peek_pending_input().await.unwrap(),
            Some(admitted.input.clone())
        );
    }
    assert_eq!(admitted.input.parts, parts);
    assert!(history.conversation().await.unwrap().is_empty());
    assert_eq!(repo.events().replay().await.unwrap().len(), 1);
    assert!(history.has_pending_inputs().await.unwrap());
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn bound_projection_validation_precedes_mutation_and_refresh_sees_committed_history() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    assert!(
        repo.bind_agent_history(
            " \t".into(),
            Arc::new(Projection::new(
                "agent",
                repo.events().database().selected_root().clone()
            ))
        )
        .is_err()
    );
    let other = Fixture::new();
    let other_root = parrot_domain::contracts::paths::SelectedSessionRoot::new(
        other.0.clone(),
        parrot_domain::contracts::paths::RootOrigin::Current,
    )
    .unwrap();
    assert!(
        repo.bind_agent_history(
            "agent".into(),
            Arc::new(Projection::new("agent", other_root))
        )
        .is_err()
    );
    let projection = Arc::new(Projection::new(
        "agent",
        repo.events().database().selected_root().clone(),
    ));
    let history = repo
        .bind_agent_history("agent".into(), projection.clone())
        .unwrap();
    assert!(
        history
            .append_conversation(
                event("wrong", "other"),
                ConversationMessage::text(MessageRole::User, "wrong")
            )
            .await
            .is_err()
    );
    assert!(repo.events().replay().await.unwrap().is_empty());
    history
        .append_conversation(
            event("one", "agent"),
            ConversationMessage::text(MessageRole::User, "prompt"),
        )
        .await
        .unwrap();
    assert_eq!(*projection.entries_seen.lock().unwrap(), vec![1]);
    assert_eq!(repo.conversation("agent".into()).await.unwrap().len(), 1);
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn derived_file_failure_propagates_after_commit_without_undoing_authoritative_history() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    let projection = Arc::new(Projection::new(
        "agent",
        repo.events().database().selected_root().clone(),
    ));
    projection.fail_refresh.store(true, Ordering::SeqCst);
    let history = repo
        .bind_agent_history("agent".into(), projection.clone())
        .unwrap();
    assert!(
        matches!(history.append_conversation(event("durable", "agent"),ConversationMessage::text(MessageRole::User,"saved")).await,Err(EventError::Invalid(message)) if message=="injected derived-file failure")
    );
    assert_eq!(repo.events().replay().await.unwrap().len(), 1);
    repo.events().database().close().await.unwrap();
    let reopened = fixture.repository();
    assert_eq!(
        reopened.messages("agent".into()).await.unwrap(),
        vec!["user: saved"]
    );
    projection.fail_refresh.store(false, Ordering::SeqCst);
    reopened
        .bind_agent_history("agent".into(), projection.clone())
        .unwrap()
        .refresh()
        .await
        .unwrap();
    assert_eq!(*projection.entries_seen.lock().unwrap(), vec![1, 1]);
    reopened.events().database().close().await.unwrap();
}

#[tokio::test]
async fn agent_conversation_reload_discards_memory_and_restores_sqlite_compaction_not_jsonl() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    let projection = Arc::new(Projection::new(
        "agent",
        repo.events().database().selected_root().clone(),
    ));
    let history = repo
        .bind_agent_history("agent".into(), projection.clone())
        .unwrap();
    history
        .append_conversation(
            event("prompt", "agent"),
            ConversationMessage::text(MessageRole::User, "before"),
        )
        .await
        .unwrap();
    let mut conversation = AgentConversation::new(history.clone()).await.unwrap();
    assert!(conversation.is_answerable());
    conversation.add(model(MessageRole::System, "ignored for answerability"));
    assert!(conversation.is_answerable());
    conversation.add(model(MessageRole::Assistant, "memory only"));
    assert!(!conversation.is_answerable());
    conversation.reload().await.unwrap();
    assert_eq!(conversation.len(), 1);
    assert!(conversation.is_answerable());
    let watermark = history.conversation().await.unwrap()[0].sequence;
    history
        .append_compaction_status(
            event("compact", "agent"),
            CompactionSnapshot {
                summary: "summary".into(),
                watermark,
            },
            "status".into(),
        )
        .await
        .unwrap();
    let mut assistant = ConversationMessage::text(MessageRole::Assistant, "after");
    assistant.provider_content = ProviderContent {
        provider_id: "provider".into(),
        content: "opaque".into(),
    };
    history
        .append_conversation(event("answer", "agent"), assistant)
        .await
        .unwrap();
    std::fs::write(
        fixture.0.join("history.jsonl"),
        b"corrupt and deliberately unauthoritative\n",
    )
    .unwrap();
    conversation.reload().await.unwrap();
    assert_eq!(
        conversation
            .messages()
            .iter()
            .map(ModelMessage::content)
            .collect::<Vec<_>>(),
        vec!["summary", "status", "after"]
    );
    assert_eq!(
        conversation.messages()[2].provider_content.content,
        "opaque"
    );
    assert!(!conversation.is_answerable());
    repo.events().database().close().await.unwrap();
    let reopened = fixture.repository();
    let restored = AgentConversation::new(
        reopened
            .bind_agent_history("agent".into(), projection)
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(restored.messages(), conversation.messages());
    reopened.events().database().close().await.unwrap();
}

#[tokio::test]
async fn bound_views_share_actual_statistics_cache_and_do_not_refresh_for_unprojected_append() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    let projection = Arc::new(Projection::new(
        "agent",
        repo.events().database().selected_root().clone(),
    ));
    let history = repo
        .bind_agent_history("agent".into(), projection.clone())
        .unwrap();
    let base = repo.events().get_runtime_statistics().await.unwrap();
    let bound = history
        .repository()
        .events()
        .get_runtime_statistics()
        .await
        .unwrap();
    assert!(Arc::ptr_eq(&base, &bound));
    history
        .append(event("fact", "agent"), None, None)
        .await
        .unwrap();
    assert!(projection.entries_seen.lock().unwrap().is_empty());
    assert_eq!(repo.events().replay().await.unwrap().len(), 1);
    assert!(
        history
            .append(
                event("invalid-role", "agent"),
                Some("tool".into()),
                Some("text".into())
            )
            .await
            .is_err()
    );
    assert_eq!(repo.events().replay().await.unwrap().len(), 1);
    repo.events().database().close().await.unwrap();
}
