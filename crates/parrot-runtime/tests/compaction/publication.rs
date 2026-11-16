//! Real storage false/error paths must never create a committed status publication.
use super::*;
use parrot_domain::protocol::messages::parrot::{CompactionPhase, CompactionTrigger};

#[tokio::test]
async fn saving_publication_failure_leaves_history_unchanged_and_reports_failure() {
    let fixture = Fixture::new();
    let history = fixture.history();
    user(&history, "old").await;
    user(&history, "tail").await;
    let provider = Scripted::default();
    let selection = provider.selection(10000);
    let sessions = ProviderSessions::new();
    let mut conversation = AgentConversation::new(history.clone()).await.unwrap();
    let before = conversation.messages().to_vec();
    let prompt = EpochPrompt::default();
    let factory = parrot_runtime::active_work::status::StatusProvidersFactory::new(
        crate::context::templates(),
        vec![Arc::new(Status::default())],
    )
    .unwrap();
    let events = Events::new(history.clone());
    events.fail_saving.store(true, Ordering::Relaxed);
    let mut controller = fixture.controller(history, 90);

    let error = controller
        .compact_epoch(
            EpochInput {
                selection: &selection,
                tools: &[],
                conversation: &mut conversation,
                agent_state: &mut parrot_core::AgentState::default(),
                prompt: &prompt.prompt,
                sessions: &sessions,
                factory: &factory,
                query: parrot_runtime::active_work::status::StatusQuery {
                    session_id: "agent".into(),
                    parent_session_id: String::new(),
                    parent_session_name: String::new(),
                    profile: "build".into(),
                    requested_model: "provider/model".into(),
                },
                profile_prompt: String::new(),
                events: &events,
            },
            None,
            &NeverCancelled,
        )
        .await
        .unwrap_err();

    assert!(matches!(error, ContextError::Operation(_)));
    assert_eq!(error.to_string(), "saving publication failed");
    assert_eq!(conversation.messages(), before);
    assert!(
        fixture
            .repository
            .compaction("agent".into())
            .await
            .unwrap()
            .is_none()
    );
    let committed = events.committed.lock().unwrap().clone();
    assert!(committed.last().unwrap().has_compaction_failed());
    assert!(
        !committed
            .iter()
            .any(|event| event.has_compaction_finished())
    );
    assert!(!committed.iter().any(|event| event.has_agent_notice()));
    sessions.close().await.unwrap();
    fixture
        .repository
        .events()
        .database()
        .close()
        .await
        .unwrap();
}

#[tokio::test]
async fn committed_status_publication_requires_true_success_not_sqlite_failure() {
    let fixture = Fixture::new();
    let history = fixture.history();
    user(&history, "old").await;
    user(&history, "tail").await;
    fixture
        .repository
        .events()
        .transaction(|tx| {
            tx.execute_batch("CREATE TRIGGER fail_t45_snapshot BEFORE INSERT ON conversation_item WHEN NEW.kind = 'compaction' BEGIN SELECT RAISE(ABORT, 'synthetic snapshot failure'); END;")?;
            Ok(())
        })
        .await
        .unwrap();
    let provider = Scripted::default();
    let selection = provider.selection(10000);
    let sessions = ProviderSessions::new();
    let mut conversation = AgentConversation::new(history.clone()).await.unwrap();
    let before = conversation.messages().to_vec();
    let prompt = EpochPrompt::default();
    let factory = parrot_runtime::active_work::status::StatusProvidersFactory::new(
        crate::context::templates(),
        vec![Arc::new(Status::default())],
    )
    .unwrap();
    let events = Events::new(history.clone());
    let mut controller = fixture.controller(history.clone(), 90);
    let error = controller
        .compact(
            EpochInput {
                selection: &selection,
                tools: &[],
                conversation: &mut conversation,
                agent_state: &mut parrot_core::AgentState::default(),
                prompt: &prompt.prompt,
                sessions: &sessions,
                factory: &factory,
                query: parrot_runtime::active_work::status::StatusQuery {
                    session_id: "agent".into(),
                    parent_session_id: String::new(),
                    parent_session_name: String::new(),
                    profile: "build".into(),
                    requested_model: "provider/model".into(),
                },
                profile_prompt: String::new(),
                events: &events,
            },
            None,
            &NeverCancelled,
        )
        .await
        .unwrap_err();
    assert!(matches!(&error, ContextError::History(_)));
    assert!(error.to_string().contains("synthetic snapshot failure"));
    assert_eq!(conversation.messages(), before);
    let committed = events.committed.lock().unwrap().clone();
    assert_eq!(committed.len(), 5);
    assert!(committed[0].has_compaction_started());
    assert_eq!(
        committed[0].compaction_started().trigger,
        CompactionTrigger::COMPACTION_TRIGGER_USER_REQUESTED.into()
    );
    for event in &committed[1..3] {
        assert_eq!(
            event.compaction_progress().phase,
            CompactionPhase::COMPACTION_PHASE_SUMMARIZING.into()
        );
        assert_eq!(event.compaction_progress().total_groups, 1);
    }
    assert_eq!(committed[1].compaction_progress().completed_groups, 0);
    assert_eq!(committed[2].compaction_progress().completed_groups, 1);
    assert_eq!(
        committed[3].compaction_progress().phase,
        CompactionPhase::COMPACTION_PHASE_SAVING.into()
    );
    assert!(committed[4].has_compaction_failed());
    assert!(!committed.iter().any(|event| event.has_agent_notice()));
    assert!(
        fixture
            .repository
            .compaction("agent".into())
            .await
            .unwrap()
            .is_none()
    );
    sessions.close().await.unwrap();
    fixture
        .repository
        .events()
        .database()
        .close()
        .await
        .unwrap();
}

#[tokio::test]
async fn committed_status_publication_failure_preserves_replacement_without_resetting_calibration()
{
    let fixture = Fixture::new();
    let history = fixture.history();
    user(&history, "old").await;
    user(&history, "tail").await;
    let provider = Scripted::default();
    let selection = provider.selection(10000);
    let sessions = ProviderSessions::new();
    let mut conversation = AgentConversation::new(history.clone()).await.unwrap();
    let mut prompt = EpochPrompt::default();
    prompt
        .agent_state
        .last_request_statistic
        .observe_usage("scripted/model", 100, 1000);
    let calibration = prompt.agent_state.last_request_statistic.clone();
    let factory = parrot_runtime::active_work::status::StatusProvidersFactory::new(
        crate::context::templates(),
        vec![Arc::new(Status::default())],
    )
    .unwrap();
    let events = Events::new(history.clone());
    events.fail_status.store(true, Ordering::Relaxed);
    let mut controller = fixture.controller(history.clone(), 90);
    let error = controller
        .compact_epoch(
            input(
                &selection,
                &mut conversation,
                &mut prompt,
                &sessions,
                &factory,
                &events,
            ),
            None,
            &NeverCancelled,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ContextError::Operation(_)));
    assert_eq!(error.to_string(), "status publication failed");
    assert_eq!(prompt.agent_state.last_request_statistic, calibration);
    assert!(
        history
            .repository()
            .compaction("agent".into())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        AgentConversation::new(history).await.unwrap().messages(),
        conversation.messages()
    );
    let published = events.published.lock().unwrap().clone();
    assert_eq!(
        published
            .iter()
            .filter(|event| event.has_compaction_failed())
            .count(),
        1
    );
    assert!(
        !published
            .iter()
            .any(|event| event.has_compaction_finished())
    );
    assert!(!published.iter().any(|event| event.has_agent_notice()));
    sessions.close().await.unwrap();
    fixture
        .repository
        .events()
        .database()
        .close()
        .await
        .unwrap();
}
