use super::Fixture;
use parrot_domain::contracts::async_ops::BoxFuture;
use parrot_domain::protocol::messages::parrot::Event;
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
