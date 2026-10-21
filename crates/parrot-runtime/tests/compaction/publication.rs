//! Real storage false/error paths must never create a committed status publication.
use super::*;
use parrot_storage::conversation::CompactionSnapshot;

struct RacingStatus {
    history: AgentHistoryView,
}
impl ContextStatus for RacingStatus {
    fn observe<'a>(
        &'a self,
        _: &'a ContextSnapshot,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<String>> {
        Box::pin(async { Ok("status".into()) })
    }
    fn observe_fitted<'a>(
        &'a self,
        estimate: &'a (dyn Fn(&str) -> ContextResult<ContextSnapshot> + Send + Sync),
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<String>> {
        Box::pin(async move {
            let _ = estimate("fitted")?;
            // A second real writer wins this watermark while status is being fitted.
            assert!(
                self.history
                    .append_compaction_status(
                        event("agent"),
                        CompactionSnapshot {
                            summary: "winner".into(),
                            watermark: 1
                        },
                        "winner status".into()
                    )
                    .await?
            );
            Ok("fitted".into())
        })
    }
}

#[tokio::test]
async fn committed_status_publication_requires_true_success_not_duplicate_or_sqlite_failure() {
    for sqlite_failure in [false, true] {
        let fixture = Fixture::new();
        let history = fixture.history();
        user(&history, "old").await;
        user(&history, "tail").await;
        if sqlite_failure {
            fixture.repository.events().transaction(|tx| {
                tx.execute_batch("CREATE TRIGGER fail_t45_snapshot BEFORE INSERT ON compaction_snapshot BEGIN SELECT RAISE(ABORT, 'synthetic snapshot failure'); END;")?;
                Ok(())
            }).await.unwrap();
        }
        let provider = Scripted::default();
        let selection = provider.selection(10000);
        let sessions = ProviderSessions::new();
        let mut conversation = AgentConversation::new(history.clone()).await.unwrap();
        let before = conversation.messages().to_vec();
        let prompt = EpochPrompt::default();
        let status: Box<dyn ContextStatus> = if sqlite_failure {
            Box::new(Status::default())
        } else {
            Box::new(RacingStatus {
                history: history.clone(),
            })
        };
        let events = Events::new(history.clone());
        let mut controller = fixture.controller(history.clone(), 90);
        let error = controller
            .compact(
                EpochInput {
                    selection: &selection,
                    tools: &[],
                    conversation: &mut conversation,
                    prompt: &prompt.prompt,
                    sessions: &sessions,
                    status: status.as_ref(),
                    events: &events,
                },
                None,
                &NeverCancelled,
            )
            .await
            .unwrap_err();
        if sqlite_failure {
            assert!(matches!(&error, ContextError::History(_)));
            assert!(error.to_string().contains("synthetic snapshot failure"));
        } else {
            assert!(matches!(&error, ContextError::InvalidOperation(_)));
            assert_eq!(
                error.to_string(),
                "compaction snapshot was already persisted"
            );
        }
        assert_eq!(conversation.messages(), before);
        let committed = events.committed.lock().unwrap().clone();
        assert_eq!(committed.len(), 2);
        assert!(committed[0].has_compaction_started());
        assert!(committed[1].has_compaction_failed());
        assert!(!committed.iter().any(|event| event.has_agent_notice()));
        let snapshot = fixture.repository.compaction("agent".into()).await.unwrap();
        if sqlite_failure {
            assert!(snapshot.is_none());
        } else {
            assert_eq!(snapshot.unwrap().summary, "winner");
        }
        sessions.close().await.unwrap();
        fixture
            .repository
            .events()
            .database()
            .close()
            .await
            .unwrap();
    }
}
