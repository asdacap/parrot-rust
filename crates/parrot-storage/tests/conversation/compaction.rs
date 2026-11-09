use parrot_domain::contracts::paths::{RootOrigin, SelectedSessionRoot};
use parrot_domain::protocol::messages::parrot::Event;
use parrot_storage::conversation::{
    AgentHistoryEntry, CompactionSnapshot, ContextReminderCheckpoint, ConversationMessage,
    ConversationOrigin, ConversationRepository, MessageRole,
};
use parrot_storage::database::SessionDatabase;
use parrot_storage::events::{EventError, EventRepository};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "parrot-t18-compaction-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn repository(&self) -> ConversationRepository {
        let root = SelectedSessionRoot::new(self.0.clone(), RootOrigin::Current).unwrap();
        {
            let events = EventRepository::new(SessionDatabase::open(root).unwrap());
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
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn event(id: &str) -> Event {
    Event {
        id: id.into(),
        agent_session_id: "agent".into(),
        ..Default::default()
    }
}
fn snapshot(summary: &str, watermark: i64) -> CompactionSnapshot {
    CompactionSnapshot {
        summary: summary.into(),
        watermark,
    }
}
fn checkpoint(model: &str, limit: i32, percentage: i32) -> ContextReminderCheckpoint {
    ContextReminderCheckpoint {
        canonical_model: model.into(),
        context_limit: limit,
        percentage,
    }
}
async fn append(repo: &ConversationRepository, id: &str, role: MessageRole, content: &str) {
    let mut message = ConversationMessage::text(role, content);
    message.origin = ConversationOrigin::Model;
    repo.events()
        .append(event(id), Some(message))
        .await
        .unwrap();
}
async fn count(repo: &ConversationRepository, table: &'static str) -> i64 {
    repo.events()
        .transaction(move |tx| {
            Ok(tx.query_row(&format!("SELECT COUNT(*) FROM {table};"), [], |r| r.get(0))?)
        })
        .await
        .unwrap()
}
async fn fail_projection(repo: &ConversationRepository) {
    repo.events().transaction(|tx| { tx.execute_batch("CREATE TRIGGER fail_t18_projection BEFORE INSERT ON conversation_part BEGIN SELECT RAISE(ABORT, 't18 projection failure'); END;")?; Ok(()) }).await.unwrap();
}
async fn resume_projection(repo: &ConversationRepository) {
    repo.events()
        .transaction(|tx| {
            tx.execute_batch("DROP TRIGGER fail_t18_projection;")?;
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn compaction_is_conversation_owned_without_a_separate_history_table() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    append(&repo, "covered", MessageRole::User, "covered").await;
    let covered = repo.conversation("agent".into()).await.unwrap()[0].sequence;
    append(&repo, "retained", MessageRole::Assistant, "retained").await;
    repo.save_compaction("agent".into(), snapshot("summary", covered))
        .await
        .unwrap();
    let tables = repo
        .events()
        .transaction(|tx| {
            let mut query = tx.prepare(
                "SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT GLOB 'sqlite_*' ORDER BY name;",
            )?;
            Ok(query.query_map([], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap();
    assert!(!tables.iter().any(|table| table == "agent_history"));
    assert_eq!(tables.len(), 7);
    let state = repo.read_conversation_state("agent".into()).await.unwrap();
    assert_eq!(state.snapshot, Some(snapshot("summary", covered)));
    assert_eq!(state.items.len(), 1);
    assert_eq!(
        parrot_storage::events::types::content(&state.items[0].item.message.parts),
        "retained"
    );
    let conversation = repo.conversation("agent".into()).await.unwrap();
    assert_eq!(conversation.len(), 2);
    assert_eq!(repo.model_history("agent".into()).await.unwrap().len(), 2);
    let history = repo.agent_history("agent".into()).await.unwrap();
    let summary_sequence = history.last().unwrap().sequence();
    assert!(matches!(
        history.last(),
        Some(AgentHistoryEntry::Compaction { .. })
    ));
    assert!(
        repo.execution_message("agent".into(), summary_sequence)
            .await
            .unwrap()
            .is_none()
    );
    repo.events().database().close().await.unwrap();
    let reopened = fixture.repository();
    assert_eq!(
        reopened
            .read_conversation_state("agent".into())
            .await
            .unwrap(),
        state
    );
    reopened.events().database().close().await.unwrap();
}

#[tokio::test]
async fn agent_history_retains_messages_and_compaction_markers() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    let mut assistant = ConversationMessage::text(MessageRole::Assistant, "before");
    assistant.tool_calls.push(parrot_storage::events::ToolCall {
        id: "call-1".into(),
        name: "read".into(),
        arguments_json: "{\"path\":\"file\"}".into(),
    });
    repo.events()
        .append(event("assistant"), Some(assistant))
        .await
        .unwrap();
    repo.save_compaction("agent".into(), snapshot("summary one", 1))
        .await
        .unwrap();
    let mut tool = ConversationMessage::text(MessageRole::Tool, "result");
    tool.tool_call_id = "call-1".into();
    repo.events()
        .append(event("tool"), Some(tool))
        .await
        .unwrap();
    let covered = repo
        .conversation("agent".into())
        .await
        .unwrap()
        .last()
        .unwrap()
        .sequence;
    repo.save_compaction("agent".into(), snapshot("summary two", covered))
        .await
        .unwrap();
    let history = repo.agent_history("agent".into()).await.unwrap();
    assert_eq!(history.len(), 4);
    assert_eq!(
        history
            .iter()
            .map(|entry| match entry {
                AgentHistoryEntry::Message { .. } => "AgentHistoryMessageEntry",
                AgentHistoryEntry::Compaction { .. } => "AgentHistoryCompactionEntry",
                AgentHistoryEntry::Request { .. } => "AgentHistoryRequestEntry",
            })
            .collect::<Vec<_>>()
            .join(","),
        "AgentHistoryMessageEntry,AgentHistoryCompactionEntry,AgentHistoryMessageEntry,AgentHistoryCompactionEntry"
    );
    let AgentHistoryEntry::Message {
        role, tool_calls, ..
    } = &history[0]
    else {
        panic!("expected message")
    };
    assert_eq!(role, "assistant");
    assert_eq!(tool_calls.len(), 1);
    assert!(tool_calls[0].arguments_json.contains("file"));
    assert_eq!(
        history
            .iter()
            .filter_map(|entry| match entry {
                AgentHistoryEntry::Compaction { summary, .. } => Some(summary.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(","),
        "summary one,summary two"
    );
    assert!(
        !repo
            .save_compaction("agent".into(), snapshot("duplicate", covered))
            .await
            .unwrap()
    );
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn compaction_status_is_atomic_and_canonical_history_excludes_associated_statuses() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    append(&repo, "before", MessageRole::User, "before").await;
    assert!(
        repo.append_compaction_status(
            event("first-status"),
            snapshot("first summary", 0),
            "first status".into()
        )
        .await
        .unwrap()
    );
    append(&repo, "between", MessageRole::User, "between").await;
    let covered = repo
        .conversation("agent".into())
        .await
        .unwrap()
        .last()
        .unwrap()
        .sequence;
    assert!(
        repo.append_context_reminder(
            event("reminder"),
            checkpoint("provider/model", 10_000, 75),
            "reminder".into()
        )
        .await
        .unwrap()
    );
    assert!(
        repo.latest_context_reminder("agent".into())
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        repo.append_compaction_status(
            event("second-status"),
            snapshot("second summary", covered),
            "second status".into()
        )
        .await
        .unwrap()
    );
    assert!(
        repo.latest_context_reminder("agent".into())
            .await
            .unwrap()
            .is_none()
    );
    append(&repo, "after", MessageRole::User, "after").await;
    let context = repo
        .compaction_history("agent".into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(context.snapshot.summary, "second summary");
    assert_eq!(
        context.status.as_ref().unwrap().message.parts,
        vec![parrot_storage::events::ConversationPart::text(
            "second status"
        )]
    );
    assert!(
        context
            .tail
            .iter()
            .all(|item| item.message.parts.len() == 1)
    );
    assert_eq!(
        context
            .tail
            .iter()
            .map(|item| parrot_storage::events::types::content(&item.message.parts))
            .collect::<Vec<_>>()
            .join(","),
        "reminder,after"
    );
    let ids = repo
        .events()
        .replay()
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.id)
        .collect::<Vec<_>>()
        .join(",");
    assert!(ids.contains("first-status"));
    assert!(ids.contains("second-status"));
    assert!(
        !repo
            .append_compaction_status(
                event("duplicate"),
                snapshot("duplicate", covered),
                "duplicate status".into()
            )
            .await
            .unwrap()
    );
    repo.events().database().close().await.unwrap();
    let reopened = fixture.repository();
    assert_eq!(
        reopened.compaction_history("agent".into()).await.unwrap(),
        Some(context)
    );
    reopened.events().database().close().await.unwrap();
}

#[tokio::test]
async fn compaction_projection_failure_rolls_back_snapshot_event_history_and_reminder_clear() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    assert!(
        repo.append_context_reminder(
            event("reminder"),
            checkpoint("provider/model", 10_000, 75),
            "reminder".into()
        )
        .await
        .unwrap()
    );
    let before = repo.agent_history("agent".into()).await.unwrap();
    fail_projection(&repo).await;
    assert!(
        repo.append_compaction_status(
            event("failed"),
            snapshot("must roll back", 1),
            "failed status".into()
        )
        .await
        .is_err()
    );
    assert_eq!(repo.compaction("agent".into()).await.unwrap(), None);
    assert_eq!(repo.agent_history("agent".into()).await.unwrap(), before);
    assert_eq!(count(&repo, "event").await, 0);
    let associations = repo
        .events()
        .transaction(|tx| {
            Ok(tx.query_row(
                "SELECT count(*) FROM conversation_item WHERE compaction_status_sequence IS NOT NULL",
                [],
                |row| row.get::<_, i64>(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(associations, 0);
    assert_eq!(
        repo.latest_context_reminder("agent".into()).await.unwrap(),
        Some(checkpoint("provider/model", 10_000, 75))
    );
    resume_projection(&repo).await;
    assert!(
        repo.append_compaction_status(
            event("failed"),
            snapshot("committed", 1),
            "saved status".into()
        )
        .await
        .unwrap()
    );
    assert_eq!(count(&repo, "event").await, 1);
    assert!(
        repo.latest_context_reminder("agent".into())
            .await
            .unwrap()
            .is_none()
    );
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn context_reminder_checkpoint_belongs_to_its_conversation_message_after_reopen() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    assert!(
        repo.append_context_reminder(
            event("reminder"),
            checkpoint("provider/model", 10_000, 50),
            "rendered warning".into(),
        )
        .await
        .unwrap()
    );
    let items = repo
        .read_conversation_state("agent".into())
        .await
        .unwrap()
        .items;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].item.message.role, MessageRole::System);
    assert_eq!(
        items[0].item.message.parts,
        vec![parrot_storage::events::ConversationPart::text(
            "rendered warning"
        )]
    );
    let sequence = items[0].item.sequence;
    repo.events().database().close().await.unwrap();

    let reopened = fixture.repository();
    assert_eq!(
        reopened
            .latest_context_reminder("agent".into())
            .await
            .unwrap(),
        Some(checkpoint("provider/model", 10_000, 50))
    );
    assert!(
        !reopened
            .append_context_reminder(
                event("duplicate"),
                checkpoint("provider/model", 10_000, 50),
                "duplicate".into(),
            )
            .await
            .unwrap()
    );
    reopened
        .events()
        .transaction(move |tx| {
            tx.execute(
                "DELETE FROM conversation_item WHERE sequence = ?1;",
                [sequence],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(
        reopened
            .latest_context_reminder("agent".into())
            .await
            .unwrap()
            .is_none()
    );
    reopened.events().database().close().await.unwrap();
}

#[tokio::test]
async fn compaction_resets_reminders_even_when_the_message_is_retained() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    assert!(
        repo.append_context_reminder(
            event("reminder"),
            checkpoint("provider/model", 10_000, 75),
            "retained reminder".into(),
        )
        .await
        .unwrap()
    );
    assert!(
        repo.append_compaction_status(event("status"), snapshot("summary", 0), "compacted".into())
            .await
            .unwrap()
    );
    let status_sequence = repo
        .compaction_history("agent".into())
        .await
        .unwrap()
        .unwrap()
        .status
        .unwrap()
        .sequence;
    repo.events()
        .transaction(move |tx| {
            tx.execute(
                "DELETE FROM conversation_item WHERE sequence = ?1;",
                [status_sequence],
            )?;
            let kind: String = tx.query_row(
                "SELECT kind FROM conversation_item WHERE agent_session = 'agent' ORDER BY sequence LIMIT 1;",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(kind, "context_reminder");
            Ok(())
        })
        .await
        .unwrap();
    repo.save_compaction("agent".into(), snapshot("statusless summary", 1))
        .await
        .unwrap();
    assert!(
        repo.latest_context_reminder("agent".into())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        repo.append_context_reminder(
            event("next-reminder"),
            checkpoint("provider/model", 10_000, 50),
            "new cycle".into(),
        )
        .await
        .unwrap()
    );
    repo.save_compaction("agent".into(), snapshot("another statusless summary", 2))
        .await
        .unwrap();
    assert_eq!(
        repo.latest_context_reminder("agent".into()).await.unwrap(),
        Some(checkpoint("provider/model", 10_000, 50))
    );
    assert!(
        !repo
            .append_context_reminder(
                event("duplicate"),
                checkpoint("provider/model", 10_000, 50),
                "duplicate".into(),
            )
            .await
            .unwrap()
    );
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn context_reminders_are_monotone_per_model_limit_and_project_only() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    assert!(matches!(
        repo.latest_context_reminder(" \t".into()).await,
        Err(EventError::Argument(_))
    ));
    assert!(
        repo.append_context_reminder(event("same"), checkpoint("A", 1000, 75), "first".into())
            .await
            .unwrap()
    );
    assert!(
        !repo
            .append_context_reminder(event("same"), checkpoint("A", 1000, 75), "duplicate".into())
            .await
            .unwrap()
    );
    assert!(
        !repo
            .append_context_reminder(event("same"), checkpoint("A", 1000, 50), "lower".into())
            .await
            .unwrap()
    );
    assert!(
        repo.append_context_reminder(event("same"), checkpoint("A", 1000, 90), "higher".into())
            .await
            .unwrap()
    );
    assert!(
        repo.append_context_reminder(
            event("same"),
            checkpoint("a", 1000, 50),
            "case-sensitive".into()
        )
        .await
        .unwrap()
    );
    assert!(
        repo.append_context_reminder(
            event("same"),
            checkpoint("A", 2000, 50),
            "different limit".into()
        )
        .await
        .unwrap()
    );
    assert_eq!(count(&repo, "event").await, 0);
    assert_eq!(
        repo.read_conversation_state("agent".into())
            .await
            .unwrap()
            .items
            .len(),
        4
    );
    assert_eq!(
        repo.latest_context_reminder("agent".into()).await.unwrap(),
        Some(checkpoint("A", 2000, 50))
    );
    let before = repo.read_conversation_state("agent".into()).await.unwrap();
    assert!(
        repo.append_context_reminder(event("invalid"), checkpoint("A", 0, 90), "invalid".into())
            .await
            .is_err()
    );
    assert_eq!(
        repo.read_conversation_state("agent".into()).await.unwrap(),
        before
    );
    fail_projection(&repo).await;
    assert!(
        repo.append_context_reminder(event("same"), checkpoint("A", 2000, 90), "failed".into())
            .await
            .is_err()
    );
    assert_eq!(
        repo.read_conversation_state("agent".into())
            .await
            .unwrap()
            .items
            .len(),
        4
    );
    assert_eq!(
        repo.latest_context_reminder("agent".into()).await.unwrap(),
        Some(checkpoint("A", 2000, 50))
    );
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn final_request_messages_and_tool_restoration_roll_back_with_projected_messages() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    assert!(
        !repo
            .append_tool_availability_restored_prompt(event("same"), "absent".into())
            .await
            .unwrap()
    );
    repo.append_final_provider_request_prompt(event("same"), "final".into())
        .await
        .unwrap();
    assert!(
        !repo
            .append_final_provider_request_prompt(event("same"), "duplicate".into())
            .await
            .unwrap()
    );
    assert_eq!(count(&repo, "conversation_item").await, 1);
    fail_projection(&repo).await;
    assert!(
        repo.append_tool_availability_restored_prompt(event("same"), "failed".into())
            .await
            .is_err()
    );
    assert!(
        repo.conversation("agent".into())
            .await
            .unwrap()
            .last()
            .unwrap()
            .final_phase
    );
    assert_eq!(count(&repo, "conversation_item").await, 1);
    resume_projection(&repo).await;
    assert!(
        repo.append_tool_availability_restored_prompt(event("same"), "restored".into())
            .await
            .unwrap()
    );
    assert!(
        !repo
            .append_tool_availability_restored_prompt(event("same"), "duplicate".into())
            .await
            .unwrap()
    );
    repo.append_system_prompt(event("same"), "system".into())
        .await
        .unwrap();
    assert_eq!(count(&repo, "conversation_item").await, 3);
    assert_eq!(count(&repo, "event").await, 0);
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn final_request_state_survives_later_messages_and_reopen_without_duplicate_prompts() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    repo.append_final_provider_request_prompt(event("final"), "final".into())
        .await
        .unwrap();
    repo.save_compaction("agent".into(), snapshot("summary", 0))
        .await
        .unwrap();
    for (id, role) in [
        ("assistant", MessageRole::Assistant),
        ("tool", MessageRole::Tool),
        ("user", MessageRole::User),
        ("system", MessageRole::System),
    ] {
        append(&repo, id, role, id).await;
    }
    repo.append_final_provider_request_prompt(event("duplicate"), "duplicate".into())
        .await
        .unwrap();
    let messages = repo.conversation("agent".into()).await.unwrap();
    assert_eq!(messages.len(), 5);
    assert!(messages.iter().all(|item| item.final_phase));
    let state = repo.read_conversation_state("agent".into()).await.unwrap();
    assert_eq!(state.items.len(), 5);
    assert!(state.items.iter().all(|item| item.item.final_phase));
    repo.events().database().close().await.unwrap();

    let reopened = fixture.repository();
    assert_eq!(
        reopened.conversation("agent".into()).await.unwrap(),
        messages
    );
    assert!(
        reopened
            .append_tool_availability_restored_prompt(event("restored"), "restored".into())
            .await
            .unwrap()
    );
    assert!(
        !reopened
            .append_tool_availability_restored_prompt(event("duplicate"), "duplicate".into())
            .await
            .unwrap()
    );
    let restored = reopened.conversation("agent".into()).await.unwrap();
    assert_eq!(restored.len(), 6);
    assert!(!restored.last().unwrap().final_phase);
    append(&reopened, "later", MessageRole::Assistant, "later").await;
    assert!(
        !reopened
            .conversation("agent".into())
            .await
            .unwrap()
            .last()
            .unwrap()
            .final_phase
    );
    assert!(
        reopened
            .append_final_provider_request_prompt(event("next-final"), "next final".into())
            .await
            .unwrap()
    );
    reopened.events().database().close().await.unwrap();
}

#[tokio::test]
async fn retained_reminder_atomically_records_event_assistant_and_system_then_reopens() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    fail_projection(&repo).await;
    assert!(
        repo.append_retained_reminder(
            event("retained"),
            ConversationMessage::text(MessageRole::Assistant, "answer"),
            "reminder".into()
        )
        .await
        .is_err()
    );
    assert_eq!(count(&repo, "event").await, 0);
    assert_eq!(count(&repo, "conversation_item").await, 0);
    resume_projection(&repo).await;
    repo.append_retained_reminder(
        event("retained"),
        ConversationMessage::text(MessageRole::Assistant, "answer"),
        "reminder".into(),
    )
    .await
    .unwrap();
    let conversation = repo.conversation("agent".into()).await.unwrap();
    assert_eq!(conversation.len(), 2);
    assert_eq!(conversation[0].message.origin, ConversationOrigin::Model);
    assert_eq!(conversation[0].message.role, MessageRole::Assistant);
    assert_eq!(conversation[1].message.origin, ConversationOrigin::System);
    assert_eq!(conversation[1].message.role, MessageRole::System);
    assert_eq!(count(&repo, "event").await, 1);
    repo.events().database().close().await.unwrap();
    let reopened = fixture.repository();
    assert_eq!(
        reopened.conversation("agent".into()).await.unwrap(),
        conversation
    );
    assert_eq!(reopened.events().replay().await.unwrap()[0].id, "retained");
    reopened.events().database().close().await.unwrap();
}

struct FailingProjection {
    root: SelectedSessionRoot,
    refreshes: AtomicU64,
}
impl parrot_storage::conversation::HistoryProjection for FailingProjection {
    fn selected_root(&self) -> &SelectedSessionRoot {
        &self.root
    }
    fn validate_session(&self, agent: &str) -> Result<(), EventError> {
        if agent != "agent" {
            return Err(EventError::Argument("cross-agent projection".into()));
        }
        Ok(())
    }
    fn refresh<'a>(
        &'a self,
        repository: &'a ConversationRepository,
        agent: &'a str,
    ) -> parrot_domain::contracts::async_ops::BoxFuture<'a, Result<(), EventError>> {
        Box::pin(async move {
            assert_eq!(
                repository.compaction(agent.into()).await?,
                Some(snapshot("durable", 99))
            );
            if self.refreshes.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(std::io::Error::other("derived projection failed after commit").into());
            }
            Ok(())
        })
    }
    fn rebuild<'a>(
        &'a self,
        repository: &'a ConversationRepository,
        agent: &'a str,
    ) -> parrot_domain::contracts::async_ops::BoxFuture<'a, Result<(), EventError>> {
        self.refresh(repository, agent)
    }
}

#[tokio::test]
async fn projection_failure_is_postcommit_and_duplicate_compaction_does_not_refresh() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    let projection = Arc::new(FailingProjection {
        root: repo.events().database().selected_root().clone(),
        refreshes: AtomicU64::new(0),
    });
    let bound = repo
        .bind_agent_history("agent".into(), projection.clone())
        .unwrap();
    assert!(
        bound
            .repository()
            .save_compaction("agent".into(), snapshot("durable", 99))
            .await
            .is_err()
    );
    assert_eq!(
        repo.compaction("agent".into()).await.unwrap(),
        Some(snapshot("durable", 99))
    );
    assert_eq!(projection.refreshes.load(Ordering::SeqCst), 1);
    assert!(
        !bound
            .repository()
            .save_compaction("agent".into(), snapshot("ignored", 99))
            .await
            .unwrap()
    );
    assert_eq!(projection.refreshes.load(Ordering::SeqCst), 1);
    bound
        .repository()
        .refresh_agent_history("agent")
        .await
        .unwrap();
    assert_eq!(projection.refreshes.load(Ordering::SeqCst), 2);
    assert!(matches!(
        bound
            .repository()
            .save_compaction("other".into(), snapshot("wrong agent", 100))
            .await,
        Err(EventError::Argument(_))
    ));
    let compactions = repo
        .events()
        .transaction(|tx| {
            Ok(tx.query_row(
                "SELECT count(*) FROM conversation_item WHERE kind = 'compaction'",
                [],
                |row| row.get::<_, i64>(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(compactions, 1);
    repo.events().database().close().await.unwrap();
    let reopened = fixture.repository();
    assert_eq!(
        reopened.compaction("agent".into()).await.unwrap(),
        Some(snapshot("durable", 99))
    );
    reopened.events().database().close().await.unwrap();
}

#[tokio::test]
async fn latest_compaction_uses_history_order_and_retries_preserve_current_after_reopen() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    assert!(
        repo.save_compaction("agent".into(), snapshot("earlier", 20))
            .await
            .unwrap()
    );
    assert!(
        repo.save_compaction("agent".into(), snapshot("latest", 3))
            .await
            .unwrap()
    );
    assert!(
        repo.save_compaction("other".into(), snapshot("other agent", 99))
            .await
            .unwrap()
    );
    assert!(
        !repo
            .save_compaction("agent".into(), snapshot("ignored retry", 20))
            .await
            .unwrap()
    );
    assert_eq!(
        repo.compaction("agent".into()).await.unwrap(),
        Some(snapshot("latest", 3))
    );
    let history = repo.agent_history("agent".into()).await.unwrap();
    let compactions = history
        .iter()
        .filter_map(|entry| match entry {
            AgentHistoryEntry::Compaction {
                summary, watermark, ..
            } => Some((summary.as_str(), *watermark)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(compactions, [("earlier", 20), ("latest", 3)]);
    repo.events().database().close().await.unwrap();
    let reopened = fixture.repository();
    assert_eq!(
        reopened.compaction("agent".into()).await.unwrap(),
        Some(snapshot("latest", 3))
    );
    assert_eq!(
        reopened.agent_history("agent".into()).await.unwrap(),
        history
    );
    reopened.events().database().close().await.unwrap();
}

#[tokio::test]
async fn deleting_status_preserves_historical_compactions_and_tail_after_reopen() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    append(&repo, "before", MessageRole::User, "before").await;
    assert!(
        repo.append_compaction_status(
            event("first-status"),
            snapshot("first", 0),
            "old status".into()
        )
        .await
        .unwrap()
    );
    append(&repo, "ordinary", MessageRole::System, "ordinary").await;
    assert!(
        repo.append_compaction_status(
            event("second-status"),
            snapshot("second", 1),
            "current status".into()
        )
        .await
        .unwrap()
    );
    append(&repo, "after", MessageRole::Assistant, "after").await;
    let current = repo
        .compaction_history("agent".into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        current
            .tail
            .iter()
            .map(|item| parrot_storage::events::types::content(&item.message.parts))
            .collect::<Vec<_>>(),
        ["ordinary", "after"]
    );
    let status_sequence = current.status.unwrap().sequence;
    repo.events()
        .transaction(move |tx| {
            tx.execute(
                "DELETE FROM conversation_item WHERE sequence = ?1",
                [status_sequence],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let retained = repo
        .compaction_history("agent".into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retained.snapshot, snapshot("second", 1));
    assert!(retained.status.is_none());
    assert_eq!(
        retained
            .tail
            .iter()
            .map(|item| parrot_storage::events::types::content(&item.message.parts))
            .collect::<Vec<_>>(),
        ["ordinary", "after"]
    );
    assert!(
        !repo
            .append_compaction_status(
                event("retry-status"),
                snapshot("ignored retry", 1),
                "ignored status".into()
            )
            .await
            .unwrap()
    );
    let history = repo.agent_history("agent".into()).await.unwrap();
    assert_eq!(
        history
            .iter()
            .filter_map(|entry| match entry {
                AgentHistoryEntry::Compaction { summary, .. } => Some(summary.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    repo.events().database().close().await.unwrap();
    let reopened = fixture.repository();
    assert_eq!(
        reopened.compaction_history("agent".into()).await.unwrap(),
        Some(retained)
    );
    assert_eq!(
        reopened.agent_history("agent".into()).await.unwrap(),
        history
    );
    reopened.events().database().close().await.unwrap();
}
