use parrot_domain::contracts::paths::{RootOrigin, SelectedSessionRoot};
use parrot_domain::protocol::messages::parrot::Event;
use parrot_storage::conversation::forks::ForkError;
use parrot_storage::conversation::*;
use parrot_storage::database::SessionDatabase;
use parrot_storage::events::{
    EventRepository, ToolExecutionStatus, ToolExecutionTerminal, projection, tools,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "parrot-t18-forks-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn repository(&self) -> ConversationRepository {
        ConversationRepository::new(EventRepository::new(
            SessionDatabase::open(
                SelectedSessionRoot::new(self.0.clone(), RootOrigin::Current).unwrap(),
            )
            .unwrap(),
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
fn single_text(item: &ConversationItem) -> &str {
    assert_eq!(item.message.parts.len(), 1);
    match &item.message.parts[0] {
        ConversationPart::Text(text) => text,
        _ => panic!("expected the source's single text part"),
    }
}
async fn append(repo: &ConversationRepository, agent: &str, value: ConversationMessage) -> i64 {
    let agent = agent.to_owned();
    repo.events()
        .transaction(move |tx| {
            Ok(
                projection::project(tx, &agent, &value, "2026-01-02T03:04:05.0000000+00:00")?
                    .sequence,
            )
        })
        .await
        .unwrap()
}
async fn assistant(repo: &ConversationRepository, agent: &str, call: &str, name: &str) -> i64 {
    let mut value = message(MessageRole::Assistant, "thinking");
    value.tool_calls.push(ToolCall {
        id: call.into(),
        name: name.into(),
        arguments_json: "{}".into(),
    });
    value.provider_content = ProviderContent {
        provider_id: "provider".into(),
        content: "{\"opaque\":true}".into(),
    };
    append(repo, agent, value).await
}
async fn settle(repo: &ConversationRepository, agent: &str, assistant: i64, call: &str) {
    let published = event(&format!("{agent}-{call}"), agent);
    let terminal = ToolExecutionTerminal {
        tool_call_id: call.into(),
        tool_name: "set_checkpoint".into(),
        status: ToolExecutionStatus::Finished,
        result_parts: vec![
            ConversationPart::text("done"),
            ConversationPart::image("artifact", "image/png", "img"),
        ],
        message: "".into(),
    };
    repo.events()
        .transaction(move |tx| {
            tools::append_tool_settlement(tx, &published, assistant, &terminal, &|| {
                "2026-01-02T03:04:05.0000000+00:00".into()
            })
        })
        .await
        .unwrap();
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
async fn items(repo: &ConversationRepository, agent: &str) -> Vec<ConversationItem> {
    let agent = agent.to_owned();
    repo.events()
        .transaction(move |tx| {
            parrot_storage::conversation::reads::read_conversation(tx, &agent, 0)
        })
        .await
        .unwrap()
}
async fn row_counts(repo: &ConversationRepository, agent: &str) -> Vec<i64> {
    let agent = agent.to_owned();
    repo.events()
        .transaction(move |tx| {
            [
                "event",
                "conversation_item",
                "message",
                "agent_history",
                "history_checkpoint",
                "compaction_snapshot",
                "compaction_status",
                "tool_execution_terminal",
                "tool_batch_result",
                "tool_batch_synthetic",
            ]
            .iter()
            .map(|table| {
                tx.query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE agent_session=?1"),
                    [&agent],
                    |row| row.get(0),
                )
                .map_err(EventError::from)
            })
            .collect()
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn checkpoint_retries_conflicts_latest_before_and_active_compaction_boundary() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    let first = assistant(&repo, "agent", "first", "set_checkpoint").await;
    assert!(
        repo.record_checkpoint("agent".into(), "handoff".into(), first, "first".into())
            .await
            .unwrap()
    );
    assert!(
        repo.record_checkpoint("agent".into(), "handoff".into(), first, "first".into())
            .await
            .unwrap()
    );
    assert!(
        matches!(repo.record_checkpoint("agent".into(),"other".into(),first,"first".into()).await,Err(EventError::Conflict(message)) if message=="checkpoint tool call first was already recorded differently")
    );
    assert!(matches!(
        repo.record_checkpoint(
            "other-agent".into(),
            "handoff".into(),
            first,
            "first".into()
        )
        .await,
        Err(EventError::Conflict(_))
    ));
    let second = assistant(&repo, "agent", "second", "set_checkpoint").await;
    repo.record_checkpoint("agent".into(), "handoff".into(), second, "second".into())
        .await
        .unwrap();
    assert!(
        repo.latest_usable_checkpoint("agent".into(), "handoff".into(), second)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        repo.latest_usable_checkpoint("agent".into(), "handoff".into(), second + 1)
            .await
            .unwrap()
            .unwrap()
            .assistant_sequence,
        second
    );
    assert_eq!(
        repo.active_checkpoint_assistant_sequences("agent".into())
            .await
            .unwrap(),
        [second].into_iter().collect()
    );
    repo.events()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO compaction_snapshot VALUES ('agent','summary',?1,'at')",
                [second],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(
        repo.active_checkpoint_assistant_sequences("agent".into())
            .await
            .unwrap()
            .is_empty()
    );
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn named_fork_clones_complete_group_parts_provider_terminals_synthetic_and_checkpoint() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    append(
        &repo,
        "agent",
        message(MessageRole::User, "excluded earlier prompt"),
    )
    .await;
    let checkpoint = assistant(&repo, "agent", "checkpoint", "set_checkpoint").await;
    repo.record_checkpoint(
        "agent".into(),
        "handoff".into(),
        checkpoint,
        "checkpoint".into(),
    )
    .await
    .unwrap();
    settle(&repo, "agent", checkpoint, "checkpoint").await;
    let published = event("synthetic", "agent");
    repo.events()
        .transaction(move |tx| {
            tools::append_tool_synthetic(
                tx,
                &published,
                checkpoint,
                &[ConversationPart::image("artifact", "image/png", "img")],
                &|| "at".into(),
            )
        })
        .await
        .unwrap();
    append(
        &repo,
        "agent",
        message(MessageRole::Assistant, "after checkpoint"),
    )
    .await;
    let launch = assistant(&repo, "agent", "spawn", "spawn_agent").await;
    let parent_groups = repo
        .effective_conversation_groups("agent".into())
        .await
        .unwrap();
    assert_eq!(parent_groups.groups[1].items.len(), 3);
    let before_events = repo.events().replay_raw().await.unwrap();
    fork(
        &repo,
        "agent",
        "child",
        HistoryForkBoundary::BeforeToolBatch {
            assistant_sequence: launch,
            tool_call_id: "spawn".into(),
        },
        "handoff",
    )
    .await
    .unwrap();
    let cloned = items(&repo, "child").await;
    assert_eq!(cloned.len(), 4);
    assert_eq!(cloned[0].message, parent_groups.groups[1].items[0].message);
    assert_eq!(cloned[1].message, parent_groups.groups[1].items[1].message);
    assert_eq!(cloned[2].message, parent_groups.groups[1].items[2].message);
    assert!(cloned[0].sequence > launch);
    let remapped = repo
        .latest_usable_checkpoint("child".into(), "handoff".into(), i64::MAX)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(remapped.assistant_sequence, cloned[0].sequence);
    assert_eq!(remapped.tool_call_id, "checkpoint");
    let terminals = repo
        .events()
        .tool_terminals_of_calls("child".into(), vec!["checkpoint".into()])
        .await
        .unwrap();
    assert_eq!(
        terminals[0].result_parts,
        vec![
            ConversationPart::text("done"),
            ConversationPart::image("artifact", "image/png", "img")
        ]
    );
    assert!(
        repo.effective_conversation_groups("child".into())
            .await
            .unwrap()
            .groups
            .iter()
            .all(|group| group.is_complete)
    );
    assert_eq!(repo.events().replay_raw().await.unwrap(), before_events);
    repo.events().database().close().await.unwrap();
    let reopened = fixture.repository();
    assert_eq!(items(&reopened, "child").await, cloned);
    reopened.events().database().close().await.unwrap();
}

#[tokio::test]
async fn completed_fork_rejects_incomplete_and_safe_prefix_excludes_crossing_complete_group() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    append(&repo, "agent", message(MessageRole::User, "safe")).await;
    let crossing = assistant(&repo, "agent", "crossing", "read").await;
    let incomplete = assistant(&repo, "agent", "pending", "read").await;
    settle(&repo, "agent", crossing, "crossing").await;
    let groups = repo
        .effective_conversation_groups("agent".into())
        .await
        .unwrap()
        .groups;
    assert!(groups[1].is_complete);
    assert!(!groups[2].is_complete);
    assert!(groups[1].end_watermark > groups[2].start_watermark);
    let before = row_counts(&repo, "rejected").await;
    assert!(
        matches!(fork(&repo,"agent","rejected",HistoryForkBoundary::AfterCompletedHistory,"full").await,Err(ForkError::Argument{message,parameter_name}) if message=="parent history contains an incomplete tool batch" && parameter_name=="boundary")
    );
    assert_eq!(row_counts(&repo, "rejected").await, before);
    fork(
        &repo,
        "agent",
        "safe",
        HistoryForkBoundary::AfterSafeHistoryPrefix,
        "full",
    )
    .await
    .unwrap();
    assert_eq!(items(&repo, "safe").await.len(), 1);
    assert!(matches!(
        fork(
            &repo,
            "agent",
            "bad",
            HistoryForkBoundary::BeforeToolBatch {
                assistant_sequence: incomplete,
                tool_call_id: "wrong".into()
            },
            "full"
        )
        .await,
        Err(ForkError::Argument { .. })
    ));
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn fallback_only_for_absent_effective_history_and_empty_selection_is_noop() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    append(&repo, "parent", message(MessageRole::User, "parent prefix")).await;
    let launch = assistant(&repo, "parent", "launch", "spawn_agent").await;
    let fallback = HistoryForkSource {
        agent_session_id: "parent".into(),
        boundary: HistoryForkBoundary::BeforeToolBatch {
            assistant_sequence: launch,
            tool_call_id: "launch".into(),
        },
    };
    repo.initialize_forked_agent_history(
        "absent".into(),
        "child".into(),
        HistoryForkBoundary::AfterSafeHistoryPrefix,
        HistoryForkSelection::parse("full"),
        Some(fallback.clone()),
    )
    .await
    .unwrap();
    assert_eq!(
        items(&repo, "child").await[0].message.parts,
        vec![ConversationPart::text("parent prefix")]
    );
    assistant(&repo, "incomplete", "pending", "read").await;
    repo.initialize_forked_agent_history(
        "incomplete".into(),
        "empty-child".into(),
        HistoryForkBoundary::AfterSafeHistoryPrefix,
        HistoryForkSelection::parse("full"),
        Some(fallback),
    )
    .await
    .unwrap();
    assert!(items(&repo, "empty-child").await.is_empty());
    repo.events()
        .transaction(|tx| {
            tx.execute(
                "INSERT INTO compaction_snapshot VALUES ('compact-only','summary',7,'at')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    repo.initialize_forked_agent_history(
        "compact-only".into(),
        "compact-child".into(),
        HistoryForkBoundary::AfterSafeHistoryPrefix,
        HistoryForkSelection::parse("full"),
        Some(HistoryForkSource {
            agent_session_id: "bad".into(),
            boundary: HistoryForkBoundary::BeforeToolBatch {
                assistant_sequence: 99,
                tool_call_id: "missing".into(),
            },
        }),
    )
    .await
    .unwrap();
    let effective = repo
        .effective_conversation_groups("compact-child".into())
        .await
        .unwrap();
    assert!(effective.groups.is_empty());
    assert_eq!(
        effective.snapshot,
        Some(CompactionSnapshot {
            summary: "summary".into(),
            watermark: 0
        })
    );
    fork(
        &repo,
        "parent",
        "empty",
        HistoryForkBoundary::BeforeToolBatch {
            assistant_sequence: -1,
            tool_call_id: "missing".into(),
        },
        "empty",
    )
    .await
    .unwrap();
    assert_eq!(row_counts(&repo, "empty").await, vec![0; 10]);
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn full_fork_remaps_effective_snapshot_status_and_never_duplicates_status_in_tail() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    let old = append(&repo, "agent", message(MessageRole::User, "old")).await;
    let status = append(&repo, "agent", message(MessageRole::System, "status")).await;
    append(&repo, "agent", message(MessageRole::User, "tail")).await;
    repo.events()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO compaction_snapshot VALUES ('agent','summary',?1,'at')",
                [old],
            )?;
            tx.execute(
                "INSERT INTO compaction_status VALUES ('agent',?1,?2)",
                rusqlite::params![old, status],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    for (destination, boundary) in [
        ("completed", HistoryForkBoundary::AfterCompletedHistory),
        ("safe", HistoryForkBoundary::AfterSafeHistoryPrefix),
    ] {
        fork(&repo, "agent", destination, boundary, "full")
            .await
            .unwrap();
        let effective = repo
            .effective_conversation_groups(destination.into())
            .await
            .unwrap();
        assert_eq!(
            effective.snapshot,
            Some(CompactionSnapshot {
                summary: "summary".into(),
                watermark: 0
            })
        );
        assert_eq!(
            effective.status.unwrap().message.parts,
            vec![ConversationPart::text("status")]
        );
        assert_eq!(effective.groups.len(), 1);
        assert_eq!(
            effective.groups[0].items[0].message.parts,
            vec![ConversationPart::text("tail")]
        );
    }
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn fork_failure_after_partial_clone_rolls_back_every_destination_row() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    append(&repo, "agent", message(MessageRole::User, "prefix")).await;
    let owner = assistant(&repo, "agent", "call", "read").await;
    settle(&repo, "agent", owner, "call").await;
    repo.events().transaction(|tx| {tx.execute_batch("CREATE TRIGGER fail_child_terminal BEFORE INSERT ON tool_execution_terminal WHEN NEW.agent_session='child' BEGIN SELECT RAISE(ABORT,'fork terminal failed'); END;")?;Ok(())}).await.unwrap();
    let before = repo.events().replay_raw().await.unwrap();
    assert!(matches!(
        fork(
            &repo,
            "agent",
            "child",
            HistoryForkBoundary::AfterCompletedHistory,
            "full"
        )
        .await,
        Err(ForkError::Repository(EventError::Database(_)))
    ));
    assert_eq!(row_counts(&repo, "child").await, vec![0; 10]);
    assert_eq!(repo.events().replay_raw().await.unwrap(), before);
    repo.events()
        .transaction(|tx| {
            tx.execute_batch("DROP TRIGGER fail_child_terminal;")?;
            Ok(())
        })
        .await
        .unwrap();
    fork(
        &repo,
        "agent",
        "child",
        HistoryForkBoundary::AfterCompletedHistory,
        "full",
    )
    .await
    .unwrap();
    assert_eq!(items(&repo, "child").await.len(), 3);
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn complete_message_group_without_terminal_and_duplicate_results_roll_back() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    let owner = assistant(&repo, "agent", "call", "read").await;
    let mut tool = message(MessageRole::Tool, "missing terminal");
    tool.tool_call_id = "call".into();
    let result = append(&repo, "agent", tool).await;
    repo.events()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO tool_batch_result VALUES ('agent',?1,'call',?2)",
                rusqlite::params![owner, result],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(
        matches!(fork(&repo,"agent","child",HistoryForkBoundary::AfterCompletedHistory,"full").await,Err(ForkError::Repository(EventError::Invalid(message))) if message=="a complete tool group has no terminal")
    );
    assert_eq!(row_counts(&repo, "child").await, vec![0; 10]);
    settle(&repo, "agent", owner, "call").await;
    let mut duplicate = message(MessageRole::Tool, "duplicate");
    duplicate.tool_call_id = "call".into();
    let duplicate = append(&repo, "agent", duplicate).await;
    repo.events()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO tool_batch_result VALUES ('agent',?1,'alias',?2)",
                rusqlite::params![owner, duplicate],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(
        matches!(fork(&repo,"agent","child",HistoryForkBoundary::AfterCompletedHistory,"full").await,Err(ForkError::Repository(EventError::Invalid(message))) if message=="Sequence contains more than one matching element")
    );
    assert_eq!(row_counts(&repo, "child").await, vec![0; 10]);
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn original_full_fork_copies_settled_batches_and_checkpoints_both_parameters() {
    for use_safe_prefix in [false, true] {
        let fixture = Fixture::new();
        let repo = fixture.repository();
        for (id, role, text) in [
            ("before", MessageRole::User, "before"),
            ("checkpoint-call", MessageRole::Assistant, ""),
        ] {
            let mut value = message(role, text);
            if role == MessageRole::Assistant {
                value.tool_calls.push(ToolCall {
                    id: "checkpoint".into(),
                    name: "set_checkpoint".into(),
                    arguments_json: "{\"title\":\"handoff\"}".into(),
                });
            }
            repo.events()
                .append(event(id, "agent"), Some(value))
                .await
                .unwrap();
        }
        let owner = items(&repo, "agent").await.last().unwrap().sequence;
        repo.record_checkpoint("agent".into(), "handoff".into(), owner, "checkpoint".into())
            .await
            .unwrap();
        repo.events()
            .append_tool_settlement(
                event("checkpoint-result", "agent"),
                owner,
                ToolExecutionTerminal {
                    tool_call_id: "checkpoint".into(),
                    tool_name: "set_checkpoint".into(),
                    status: ToolExecutionStatus::Finished,
                    result_parts: vec![ConversationPart::text("handoff")],
                    message: "handoff".into(),
                },
            )
            .await
            .unwrap();
        repo.events()
            .append(
                event("after", "agent"),
                Some(message(MessageRole::User, "after")),
            )
            .await
            .unwrap();
        fork(
            &repo,
            "agent",
            "child",
            if use_safe_prefix {
                HistoryForkBoundary::AfterSafeHistoryPrefix
            } else {
                HistoryForkBoundary::AfterCompletedHistory
            },
            "full",
        )
        .await
        .unwrap();
        let child = items(&repo, "child").await;
        assert_eq!(child.len(), 4);
        assert_eq!(
            child.iter().map(single_text).collect::<Vec<_>>().join(","),
            "before,,handoff,after"
        );
        assert_eq!(
            repo.events()
                .tool_terminals("child".into())
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            repo.latest_usable_checkpoint("child".into(), "handoff".into(), i64::MAX)
                .await
                .unwrap()
                .is_some()
        );
        repo.events().database().close().await.unwrap();
    }
}

#[tokio::test]
async fn original_full_fork_copies_effective_compaction_state_both_parameters() {
    for use_safe_prefix in [false, true] {
        let fixture = Fixture::new();
        let repo = fixture.repository();
        repo.events()
            .append(
                event("compacted", "agent"),
                Some(message(MessageRole::User, "compacted away")),
            )
            .await
            .unwrap();
        assert!(
            repo.append_compaction_status(
                event("status", "agent"),
                CompactionSnapshot {
                    summary: "effective summary".into(),
                    watermark: 1
                },
                "effective status".into()
            )
            .await
            .unwrap()
        );
        repo.events()
            .append(
                event("tail", "agent"),
                Some(message(MessageRole::User, "retained tail")),
            )
            .await
            .unwrap();
        fork(
            &repo,
            "agent",
            "child",
            if use_safe_prefix {
                HistoryForkBoundary::AfterSafeHistoryPrefix
            } else {
                HistoryForkBoundary::AfterCompletedHistory
            },
            "full",
        )
        .await
        .unwrap();
        let context = repo
            .compaction_history("child".into())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(context.snapshot.summary, "effective summary");
        assert_eq!(context.snapshot.watermark, 0);
        assert_eq!(
            context.status.unwrap().message.parts,
            vec![ConversationPart::text("effective status")]
        );
        assert_eq!(
            context
                .tail
                .iter()
                .map(single_text)
                .collect::<Vec<_>>()
                .join(","),
            "retained tail"
        );
        assert!(
            !items(&repo, "child")
                .await
                .iter()
                .map(single_text)
                .any(|text| text == "compacted away")
        );
        repo.events().database().close().await.unwrap();
    }
}

#[tokio::test]
async fn original_provider_content_survives_reopen_and_full_fork() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    repo.events()
        .append(
            event("question", "agent"),
            Some(message(MessageRole::User, "question")),
        )
        .await
        .unwrap();
    for (id, text) in [("answer", "answer"), ("follow-up", "follow-up")] {
        let mut value = message(MessageRole::Assistant, text);
        value.provider_content = ProviderContent {
            provider_id: "anthropic".into(),
            content: format!("[\"{text}\"]"),
        };
        repo.events()
            .append(event(id, "agent"), Some(value))
            .await
            .unwrap();
    }
    repo.events().database().close().await.unwrap();
    let restored = fixture.repository();
    fork(
        &restored,
        "agent",
        "child",
        HistoryForkBoundary::AfterCompletedHistory,
        "full",
    )
    .await
    .unwrap();
    let expected = vec![
        ProviderContent::default(),
        ProviderContent {
            provider_id: "anthropic".into(),
            content: "[\"answer\"]".into(),
        },
        ProviderContent {
            provider_id: "anthropic".into(),
            content: "[\"follow-up\"]".into(),
        },
    ];
    assert_eq!(
        items(&restored, "agent")
            .await
            .into_iter()
            .map(|item| item.message.provider_content)
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        items(&restored, "child")
            .await
            .into_iter()
            .map(|item| item.message.provider_content)
            .collect::<Vec<_>>(),
        expected
    );
    restored.events().database().close().await.unwrap();
}

#[tokio::test]
async fn original_fallback_compacted_parent_preserves_boundary_parameter_and_worker_survives() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    let mut launch = message(MessageRole::Assistant, "");
    launch.parts.clear();
    launch.tool_calls.push(ToolCall {
        id: "launch".into(),
        name: "set_agent_tasks".into(),
        arguments_json: "{}".into(),
    });
    repo.events()
        .append(event("launch", "parent"), Some(launch))
        .await
        .unwrap();
    let launch = items(&repo, "parent").await.last().unwrap().sequence;
    repo.events()
        .append_tool_settlement(
            event("launch-result", "parent"),
            launch,
            ToolExecutionTerminal {
                tool_call_id: "launch".into(),
                tool_name: "set_agent_tasks".into(),
                status: ToolExecutionStatus::Finished,
                result_parts: vec![],
                message: "declared".into(),
            },
        )
        .await
        .unwrap();
    let watermark = items(&repo, "parent").await.last().unwrap().sequence;
    repo.append_compaction_status(
        event("summary", "parent"),
        CompactionSnapshot {
            summary: "later parent summary".into(),
            watermark,
        },
        "later status".into(),
    )
    .await
    .unwrap();
    let error = repo
        .initialize_forked_agent_history(
            "absent-source".into(),
            "child".into(),
            HistoryForkBoundary::AfterSafeHistoryPrefix,
            HistoryForkSelection::parse("full"),
            Some(HistoryForkSource {
                agent_session_id: "parent".into(),
                boundary: HistoryForkBoundary::BeforeToolBatch {
                    assistant_sequence: launch,
                    tool_call_id: "launch".into(),
                },
            }),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ForkError::Argument { .. }));
    assert!(error.to_string().contains(
        "captured fork tool batch is unavailable in effective history; it may have been compacted"
    ));
    assert_eq!(error.parameter_name(), Some("boundary"));
    assert!(repo.agent_history("child".into()).await.unwrap().is_empty());
    assert!(
        repo.compaction_history("child".into())
            .await
            .unwrap()
            .is_none()
    );
    repo.events()
        .append(
            event("survivor", "other"),
            Some(message(MessageRole::User, "worker survives")),
        )
        .await
        .unwrap();
    assert_eq!(items(&repo, "other").await.len(), 1);
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn cleanup_keeps_events_usage_and_other_agents_while_removing_only_fork_state() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    let owner = assistant(&repo, "agent", "call", "set_checkpoint").await;
    settle(&repo, "agent", owner, "call").await;
    repo.record_checkpoint("agent".into(), "checkpoint".into(), owner, "call".into())
        .await
        .unwrap();
    append(&repo, "other", message(MessageRole::User, "untouched")).await;
    let mut usage = event("usage-fact", "agent");
    usage.set_request_usage_recorded(
        parrot_domain::protocol::messages::parrot::RequestUsageRecorded {
            request_id: "request".into(),
            provider: "provider".into(),
            model: "model".into(),
            input_tokens: 17,
            output_tokens: 9,
            ..Default::default()
        },
    );
    repo.events().append_usage_fact(usage).await.unwrap();
    let before = repo.events().replay_raw().await.unwrap();
    let anchors_before: i64 = repo
        .events()
        .transaction(|tx| {
            Ok(tx.query_row(
                "SELECT COUNT(*) FROM request_history_anchor WHERE agent_session='agent'",
                [],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    repo.cleanup_forked_agent_history("agent".into())
        .await
        .unwrap();
    let counts = row_counts(&repo, "agent").await;
    assert_eq!(counts[0], 2);
    assert!(counts[1..].iter().all(|count| *count == 0));
    let anchors_after: i64 = repo
        .events()
        .transaction(|tx| {
            Ok(tx.query_row(
                "SELECT COUNT(*) FROM request_history_anchor WHERE agent_session='agent'",
                [],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(anchors_before, 1);
    assert_eq!(anchors_after, anchors_before);
    assert_eq!(items(&repo, "other").await.len(), 1);
    assert_eq!(repo.events().replay_raw().await.unwrap(), before);
    repo.events().database().close().await.unwrap();
}
