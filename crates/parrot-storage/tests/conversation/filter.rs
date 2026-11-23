use super::conversation_state;
use parrot_core::FilterAmendment;
use parrot_domain::protocol::messages::parrot::Event;
use parrot_storage::conversation::forks::ForkError;
use parrot_storage::conversation::*;
use rusqlite::params;
use parrot_storage::events::{EventError, ToolExecutionStatus, ToolExecutionTerminal, tools};

fn event(id: &str, agent: &str) -> Event {
    Event {
        id: id.into(),
        agent_session_id: agent.into(),
        ..Default::default()
    }
}

fn envelope(output: &str) -> ConversationMessage {
    let mut message = ConversationMessage::text(MessageRole::Assistant, "answer");
    message.origin = ConversationOrigin::Model;
    message.provider_content = ProviderContent {
        provider_id: "provider".into(),
        content: format!(r#"{{"Version":1,"Protocol":"responses","Output":[{output}]}}"#),
    };
    message
}

async fn append(repo: &ConversationRepository, id: &str, value: ConversationMessage) -> i64 {
    repo.events()
        .append(event(id, "agent"), Some(value))
        .await
        .unwrap();
    repo.conversation("agent".into())
        .await
        .unwrap()
        .last()
        .unwrap()
        .sequence
}

fn replacement(content: &str) -> parrot_core::Message {
    parrot_core::Message {
        provider_id: "provider".into(),
        provider_content: content.into(),
        ..Default::default()
    }
}

async fn commit(
    repo: &ConversationRepository,
    start: i64,
    end: i64,
    watermark: i64,
    contents: &[&str],
) -> Result<bool, EventError> {
    let amendment = FilterAmendment {
        start_sequence: start,
        end_sequence: end,
        messages: contents
            .iter()
            .map(|content| replacement(content))
            .collect(),
        watermark,
    };
    repo.commit_filter_amendment("agent".into(), event("filter-audit", "agent"), amendment)
        .await
}

async fn contents(repo: &ConversationRepository, agent: &str) -> Vec<String> {
    let agent = agent.to_owned();
    repo.events()
        .transaction(move |tx| {
            Ok(
                parrot_storage::conversation::reads::read_conversation(tx, &agent, 0)?
                    .into_iter()
                    .map(|item| item.message.provider_content.content)
                    .collect(),
            )
        })
        .await
        .unwrap()
}

async fn kinds(repo: &ConversationRepository, agent: &str) -> Vec<String> {
    let agent = agent.to_owned();
    repo.events()
        .transaction(move |tx| {
            let mut read = tx.prepare(
                "SELECT kind FROM conversation_item WHERE agent_session = ?1 AND kind IN ('message', 'filtered_message') ORDER BY sequence;",
            )?;            let rows = read
                .query_map(params![agent], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .unwrap()
}

async fn watermark(repo: &ConversationRepository, agent: &str) -> i64 {
    repo.filter_watermark(agent.into()).await.unwrap()
}

#[tokio::test]
async fn amendment_rewrites_replay_payloads_and_advances_the_watermark() {
    let fixture = super::Fixture::new();
    let repo = fixture.repository();
    let first = append(&repo, "first", envelope(r#"{"type":"reasoning"}"#)).await;
    let second = append(&repo, "second", envelope(r#"{"type":"message"}"#)).await;
    let committed = commit(
        &repo,
        first,
        second,
        second,
        &["[]", r#"[{"type":"message"}]"#],
    )
    .await
    .unwrap();
    assert!(committed);
    assert_eq!(watermark(&repo, "agent").await, second);
    assert_eq!(
        kinds(&repo, "agent").await,
        vec!["filtered_message".to_owned(), "filtered_message".to_owned()]
    );
    let rows = contents(&repo, "agent").await;
    assert_eq!(
        rows,
        vec!["[]".to_owned(), r#"[{"type":"message"}]"#.to_owned()]
    );
    repo.events().database().close().await.unwrap();
    let reopened = fixture.repository();
    assert_eq!(watermark(&reopened, "agent").await, second);
    assert_eq!(contents(&reopened, "agent").await, rows);
    reopened.events().database().close().await.unwrap();
}

#[tokio::test]
async fn amendment_preserves_parts_and_settlement_metadata() {
    let fixture = super::Fixture::new();
    let repo = fixture.repository();
    let assistant = append(&repo, "assistant", envelope(r#"{"type":"reasoning"}"#)).await;
    let published = event("settlement", "agent");
    let terminal = ToolExecutionTerminal {
        tool_call_id: "call-1".into(),
        tool_name: "read".into(),
        status: ToolExecutionStatus::Finished,
        result_parts: vec![ConversationPart::text("done")],
    };
    repo.events()
        .transaction(move |tx| {
            tools::append_tool_settlement(tx, &published, assistant, &terminal, &|| {
                "2026-01-02T03:04:05.0000000+00:00".into()
            })
        })
        .await
        .unwrap();
    let end = repo
        .conversation("agent".into())
        .await
        .unwrap()
        .last()
        .unwrap()
        .sequence;
    let before = conversation_state(&repo, "agent").await.unwrap();
    commit(
        &repo,
        assistant,
        end,
        end,
        &["[]", r#"[{"type":"message"}]"#],
    )
    .await
    .unwrap();
    let after = conversation_state(&repo, "agent").await.unwrap();
    assert_eq!(before.items.len(), after.items.len());
    for (before, after) in before.items.iter().zip(&after.items) {
        assert_eq!(before.item.message.parts, after.item.message.parts);
        assert_eq!(
            before.item.message.tool_calls,
            after.item.message.tool_calls
        );
        assert_eq!(before.tool_owner_sequence, after.tool_owner_sequence);
        assert_eq!(before.is_compaction_status, after.is_compaction_status);
    }
    assert_eq!(contents(&repo, "agent").await[0], "[]");
}

#[tokio::test]
async fn skipped_batch_advances_the_watermark_without_touching_rows() {
    let fixture = super::Fixture::new();
    let repo = fixture.repository();
    let first = append(&repo, "first", envelope(r#"{"type":"reasoning"}"#)).await;
    let second = append(&repo, "second", envelope(r#"{"type":"message"}"#)).await;
    let originals = contents(&repo, "agent").await;
    let amendment = FilterAmendment {
        start_sequence: first,
        end_sequence: second,
        messages: originals
            .iter()
            .map(|content| replacement(content))
            .collect(),
        watermark: second,
    };
    let committed = repo
        .commit_filter_amendment("agent".into(), event("filter-audit", "agent"), amendment)
        .await
        .unwrap();
    assert!(committed);
    assert_eq!(watermark(&repo, "agent").await, second);
    assert_eq!(contents(&repo, "agent").await, originals);
    assert_eq!(
        kinds(&repo, "agent").await,
        vec!["filtered_message".to_owned(), "filtered_message".to_owned()]
    );
    let duplicate = commit(&repo, first, second, second, &["", ""])
        .await
        .unwrap();
    assert!(!duplicate);
    assert_eq!(watermark(&repo, "agent").await, second);
}

#[tokio::test]
async fn watermark_moves_forward_only() {
    let fixture = super::Fixture::new();
    let repo = fixture.repository();
    let first = append(&repo, "first", envelope(r#"{"type":"message"}"#)).await;
    commit(&repo, first, first, first, &["[]"]).await.unwrap();
    let backwards = commit(&repo, first, first, first - 1, &["[]"]).await;
    assert!(matches!(backwards, Err(EventError::Argument(_))));
    let uncovered = commit(&repo, first, first + 1, first, &["[]"]).await;
    assert!(matches!(uncovered, Err(EventError::Argument(_))));
    assert_eq!(watermark(&repo, "agent").await, first);
}

#[tokio::test]
async fn filter_markers_never_surface_as_conversation() {
    let fixture = super::Fixture::new();
    let repo = fixture.repository();
    let first = append(&repo, "first", envelope(r#"{"type":"reasoning"}"#)).await;
    let second = append(&repo, "second", envelope(r#"{"type":"message"}"#)).await;
    let before = conversation_state(&repo, "agent").await.unwrap();
    let before_history = repo.agent_history("agent".into()).await.unwrap();
    commit(&repo, first, second, second, &["[]", "[]"])
        .await
        .unwrap();
    let after = conversation_state(&repo, "agent").await.unwrap();
    assert_eq!(before.items.len(), after.items.len());
    let after_history = repo.agent_history("agent".into()).await.unwrap();
    assert_eq!(before_history.len(), after_history.len());
}

async fn fork(
    repo: &ConversationRepository,
    source: &str,
    destination: &str,
    boundary: HistoryForkBoundary,
    selection: &str,
) -> Result<(), ForkError> {
    repo.initialize_forked_agent_history(
        source.into(),
        destination.into(),
        boundary,
        HistoryForkSelection::parse(selection),
        None,
    )
    .await
}

#[tokio::test]
async fn full_fork_copies_amended_rows_and_resets_the_watermark() {
    let fixture = super::Fixture::new();
    let repo = fixture.repository();
    let first = append(&repo, "first", envelope(r#"{"type":"reasoning"}"#)).await;
    let second = append(&repo, "second", envelope(r#"{"type":"message"}"#)).await;
    commit(&repo, first, second, second, &["[]", "[]"])
        .await
        .unwrap();
    fork(
        &repo,
        "agent",
        "fork",
        HistoryForkBoundary::AfterCompletedHistory,
        "full",
    )
    .await
    .unwrap();
    let forked = contents(&repo, "fork").await;
    assert_eq!(forked, vec!["[]".to_owned(), "[]".to_owned()]);
    assert_eq!(watermark(&repo, "fork").await, 0);
}

#[tokio::test]
async fn checkpoint_fork_copies_amended_rows_and_resets_the_watermark() {
    let fixture = super::Fixture::new();
    let repo = fixture.repository();
    append(
        &repo,
        "covered",
        envelope(r#"{"type":"reasoning","encrypted_content":"sealed"}"#),
    )
    .await;
    let mut assistant = envelope(r#"{"type":"reasoning"}"#);
    assistant.tool_calls.push(ToolCall {
        id: "checkpoint-1".into(),
        name: "set_checkpoint".into(),
        arguments_json: r#"{"title":"handoff"}"#.into(),
    });
    let assistant_sequence = append(&repo, "checkpoint", assistant).await;
    let published = event("settlement", "agent");
    let terminal = ToolExecutionTerminal {
        tool_call_id: "checkpoint-1".into(),
        tool_name: "set_checkpoint".into(),
        status: ToolExecutionStatus::Finished,
        result_parts: vec![ConversationPart::text("done")],
    };
    repo.events()
        .transaction(move |tx| {
            tools::append_tool_settlement(tx, &published, assistant_sequence, &terminal, &|| {
                "2026-01-02T03:04:05.0000000+00:00".into()
            })
        })
        .await
        .unwrap();
    let end = repo
        .conversation("agent".into())
        .await
        .unwrap()
        .last()
        .unwrap()
        .sequence;
    commit(
        &repo,
        assistant_sequence,
        end,
        end,
        &["[]", r#"[{"type":"message"}]"#],
    )
    .await
    .unwrap();
    fork(
        &repo,
        "agent",
        "fork",
        HistoryForkBoundary::AfterCompletedHistory,
        "handoff",
    )
    .await
    .unwrap();
    let forked = contents(&repo, "fork").await;
    assert_eq!(
        forked,
        vec!["[]".to_owned(), r#"[{"type":"message"}]"#.to_owned()]
    );
    assert_eq!(watermark(&repo, "fork").await, 0);
    assert_eq!(
        repo.conversation("fork".into()).await.unwrap().len(),
        2,
        "the covered pre-checkpoint row stays out of a checkpoint fork"
    );
}
