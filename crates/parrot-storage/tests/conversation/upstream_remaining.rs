use base64::{Engine, engine::general_purpose::STANDARD};
use parrot_domain::contracts::async_ops::BoxFuture;
use parrot_domain::contracts::paths::{RootOrigin, SelectedSessionRoot};
use parrot_domain::protocol::messages::parrot::Event;
use parrot_storage::artifacts::{ImageArtifactRepository, ImageArtifactStore};
use parrot_storage::conversation::*;
use parrot_storage::database::SessionDatabase;
use parrot_storage::events::{
    EventRepository, NeverCancelled, ToolExecutionStatus, ToolExecutionTerminal,
};
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
            "parrot-t18-literals-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn root(&self) -> SelectedSessionRoot {
        SelectedSessionRoot::new(self.0.clone(), RootOrigin::Current).unwrap()
    }
    fn repo(&self) -> ConversationRepository {
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
struct Projection {
    root: SelectedSessionRoot,
    current: String,
    blocked_directory: Option<PathBuf>,
}
impl HistoryProjection for Projection {
    fn selected_root(&self) -> &SelectedSessionRoot {
        &self.root
    }
    fn validate_session(&self, agent: &str) -> Result<(), EventError> {
        if agent != self.current {
            Err(EventError::Invalid(
                "the history file belongs to another agent session".into(),
            ))
        } else {
            Ok(())
        }
    }
    fn refresh<'a>(
        &'a self,
        _: &'a ConversationRepository,
        agent: &'a str,
    ) -> BoxFuture<'a, Result<(), EventError>> {
        Box::pin(async move {
            self.validate_session(agent)?;
            if let Some(directory) = self.blocked_directory.clone() {
                tokio::task::spawn_blocking(move || {
                    assert!(
                        std::fs::create_dir_all(directory).is_err(),
                        "source blocked projection directory must fail"
                    )
                })
                .await
                .unwrap();
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
fn bind(repo: &ConversationRepository, agent: &str) -> AgentHistoryView {
    repo.bind_agent_history(
        agent.into(),
        Arc::new(Projection {
            root: repo.events().database().selected_root().clone(),
            current: agent.into(),
            blocked_directory: None,
        }),
    )
    .unwrap()
}
fn event(id: &str, agent: &str) -> Event {
    Event {
        id: id.into(),
        agent_session_id: agent.into(),
        ..Default::default()
    }
}
fn text(item: &ConversationItem) -> &str {
    assert_eq!(item.message.parts.len(), 1, "source Parts.Single()");
    match &item.message.parts[0] {
        ConversationPart::Text(text) => text,
        _ => panic!("expected source text part"),
    }
}
fn single<T>(values: &[T]) -> &T {
    assert_eq!(values.len(), 1, "source Single()");
    &values[0]
}
async fn append(
    repo: &ConversationRepository,
    id: &str,
    agent: &str,
    role: MessageRole,
    parts: Vec<ConversationPart>,
    calls: Vec<ToolCall>,
) -> i64 {
    let mut message = ConversationMessage::text(role, "");
    message.parts = parts;
    message.tool_calls = calls;
    repo.append_conversation(event(id, agent), message)
        .await
        .unwrap();
    repo.conversation(agent.into())
        .await
        .unwrap()
        .last()
        .unwrap()
        .sequence
}
async fn user(repo: &ConversationRepository, id: &str, agent: &str, value: &str) -> i64 {
    append(
        repo,
        id,
        agent,
        MessageRole::User,
        vec![ConversationPart::text(value)],
        vec![],
    )
    .await
}
fn call(id: &str, name: &str, args: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments_json: args.into(),
    }
}
async fn settle(
    repo: &ConversationRepository,
    id: &str,
    agent: &str,
    sequence: i64,
    call_id: &str,
    name: &str,
    value: &str,
) {
    repo.events()
        .append_tool_settlement(
            event(id, agent),
            sequence,
            ToolExecutionTerminal {
                tool_call_id: call_id.into(),
                tool_name: name.into(),
                status: ToolExecutionStatus::Finished,
                result_parts: vec![ConversationPart::text(value)],
                message: value.into(),
            },
        )
        .await
        .unwrap();
}
async fn fork(
    repo: &ConversationRepository,
    source: &str,
    destination: &str,
    boundary: HistoryForkBoundary,
    selection: &str,
    fallback: Option<HistoryForkSource>,
) -> Result<(), ForkError> {
    repo.initialize_forked_agent_history(
        source.into(),
        destination.into(),
        boundary,
        HistoryForkSelection::parse(selection),
        fallback,
    )
    .await
}

async fn safe_fallback(has_incomplete_history: bool) {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    user(&repo, "parent-before", "parent", "parent-before").await;
    let launch = append(
        &repo,
        "parent-launch",
        "parent",
        MessageRole::Assistant,
        vec![],
        vec![call("launch", "set_agent_tasks", "{}")],
    )
    .await;
    if has_incomplete_history {
        append(
            &repo,
            "unfinished",
            "source",
            MessageRole::Assistant,
            vec![],
            vec![call("unfinished", "exec_command", "{}")],
        )
        .await;
    }
    fork(
        &repo,
        "source",
        "child",
        HistoryForkBoundary::AfterSafeHistoryPrefix,
        "full",
        Some(HistoryForkSource {
            agent_session_id: "parent".into(),
            boundary: HistoryForkBoundary::BeforeToolBatch {
                assistant_sequence: launch,
                tool_call_id: "launch".into(),
            },
        }),
    )
    .await
    .unwrap();
    let child = repo.conversation("child".into()).await.unwrap();
    // source-line:588
    assert_eq!(child.len(), if has_incomplete_history { 0 } else { 1 });
    if !has_incomplete_history {
        // source-line:591
        assert_eq!(text(single(&child)), "parent-before");
    }
    // source-line:594
    assert!(
        repo.events()
            .tool_terminals("child".into())
            .await
            .unwrap()
            .is_empty()
    );
    repo.events().database().close().await.unwrap();
}
#[tokio::test]
async fn original_safe_fallback_absent() {
    safe_fallback(false).await;
}
#[tokio::test]
async fn original_safe_fallback_incomplete() {
    safe_fallback(true).await;
}

#[tokio::test]
async fn original_latest_duplicate_checkpoint_no_fallback() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    for id in ["first", "latest"] {
        let assistant = append(
            &repo,
            &format!("{id}-assistant"),
            "agent",
            MessageRole::Assistant,
            vec![ConversationPart::text("")],
            vec![call(id, "set_checkpoint", "{\"title\":\"same\"}")],
        )
        .await;
        repo.record_checkpoint("agent".into(), "same".into(), assistant, id.into())
            .await
            .unwrap();
        if id == "first" {
            settle(
                &repo,
                "first-result",
                "agent",
                assistant,
                id,
                "set_checkpoint",
                "same",
            )
            .await;
        }
    }
    let current = repo
        .conversation("agent".into())
        .await
        .unwrap()
        .last()
        .unwrap()
        .sequence;
    // source-line:866
    assert!(matches!(
        fork(
            &repo,
            "agent",
            "child",
            HistoryForkBoundary::BeforeToolBatch {
                assistant_sequence: current,
                tool_call_id: "latest".into()
            },
            "same",
            None
        )
        .await,
        Err(ForkError::Argument { .. })
    ));
    // source-line:872
    assert!(repo.conversation("child".into()).await.unwrap().is_empty());
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn original_completed_history_rejects_incomplete() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    append(
        &repo,
        "incomplete",
        "agent",
        MessageRole::Assistant,
        vec![ConversationPart::text("")],
        vec![call("unfinished", "agent_spawn", "{}")],
    )
    .await;
    // source-line:443
    assert!(matches!(
        fork(
            &repo,
            "agent",
            "child",
            HistoryForkBoundary::AfterCompletedHistory,
            "full",
            None
        )
        .await,
        Err(ForkError::Argument { .. })
    ));
    // source-line:449
    assert!(repo.conversation("child".into()).await.unwrap().is_empty());
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn original_bound_concurrent_counts_and_cross_agent_mutation() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    let first = bind(&repo, "first");
    let second = bind(&repo, "second");
    let mut tasks = vec![];
    for index in 0..24 {
        let agent = if index % 2 == 0 {
            first.clone()
        } else {
            second.clone()
        };
        let session = if index % 2 == 0 { "first" } else { "second" };
        tasks.push(tokio::spawn(async move {
            agent
                .append_conversation(
                    event(&format!("message-{index}"), session),
                    ConversationMessage::text(MessageRole::User, format!("{session}-{index}")),
                )
                .await
                .unwrap();
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    for session in ["first", "second"] {
        // source-line:715
        assert_eq!(repo.conversation(session.into()).await.unwrap().len(), 12);
    }
    let first_projection = Arc::new(Projection {
        root: fixture.root(),
        current: "first".into(),
        blocked_directory: None,
    });
    // source-line:718
    assert!(matches!(
        repo.bind_agent_history("second".into(), first_projection)
            .unwrap()
            .cleanup_fork()
            .await,
        Err(EventError::Invalid(_))
    ));
    // source-line:719
    assert_eq!(repo.conversation("second".into()).await.unwrap().len(), 12);
    first.cleanup_fork().await.unwrap();
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn original_named_checkpoint_fork_remaps_settlement() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    let parent = bind(&repo, "agent");
    let child_view = bind(&repo, "child");
    user(parent.repository(), "before", "agent", "before").await;
    let checkpoint = append(
        parent.repository(),
        "checkpoint-call",
        "agent",
        MessageRole::Assistant,
        vec![ConversationPart::text("")],
        vec![call(
            "checkpoint",
            "set_checkpoint",
            "{\"title\":\"handoff\"}",
        )],
    )
    .await;
    parent
        .record_checkpoint("handoff".into(), checkpoint, "checkpoint".into())
        .await
        .unwrap();
    parent
        .append_tool_settlement(
            event("checkpoint-result", "agent"),
            checkpoint,
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
    user(parent.repository(), "between", "agent", "between").await;
    let spawn = append(
        parent.repository(),
        "spawn-call",
        "agent",
        MessageRole::Assistant,
        vec![ConversationPart::text("")],
        vec![call("spawn", "agent_spawn", "{}")],
    )
    .await;
    child_view
        .initialize_fork(
            "agent".into(),
            HistoryForkBoundary::BeforeToolBatch {
                assistant_sequence: spawn,
                tool_call_id: "spawn".into(),
            },
            HistoryForkSelection::parse("handoff"),
            None,
        )
        .await
        .unwrap();
    let child = repo.conversation("child".into()).await.unwrap();
    // source-line:265
    assert_eq!(
        child
            .iter()
            .map(|item| match item.message.role {
                MessageRole::Assistant => "Assistant",
                MessageRole::Tool => "Tool",
                MessageRole::User => "User",
                MessageRole::System => "System",
            })
            .collect::<Vec<_>>()
            .join(","),
        "Assistant,Tool,User"
    );
    // source-line:267
    assert_eq!(single(&child[0].message.tool_calls).name, "set_checkpoint");
    // source-line:268
    assert_eq!(text(&child[1]), "handoff");
    // source-line:269
    assert_eq!(text(&child[2]), "between");
    // source-line:270
    assert_eq!(
        single(&repo.events().tool_terminals("child".into()).await.unwrap()).tool_name,
        "set_checkpoint"
    );
    // source-line:272
    assert!(
        repo.latest_usable_checkpoint("child".into(), "handoff".into(), i64::MAX)
            .await
            .unwrap()
            .is_some()
    );
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn original_cleanup_fork_sqlite_empty() {
    let fixture = Fixture::new();
    let repo = fixture
        .repo()
        .with_image_store(ImageArtifactStore::for_session(&fixture.root(), "artifacts").unwrap())
        .unwrap();
    let history = bind(&repo, "agent");
    history
        .append_conversation(
            event("message", "agent"),
            ConversationMessage::text(MessageRole::User, "durable"),
        )
        .await
        .unwrap();
    history.cleanup_fork().await.unwrap();
    // source-line:829
    assert!(history.conversation().await.unwrap().is_empty());
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn original_projection_failure_retains_committed_sqlite() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    // Exercise the source's directory-blocking I/O failure and best-effort hook,
    // without implementing or claiming the separately owned JSONL formatting.
    let directory = fixture.0.join("blocked-history");
    std::fs::create_dir(&directory).unwrap();
    std::fs::remove_dir(&directory).unwrap();
    std::fs::write(&directory, b"blocks history directory").unwrap();
    let history = repo
        .bind_agent_history(
            "agent".into(),
            Arc::new(Projection {
                root: fixture.root(),
                current: "agent".into(),
                blocked_directory: Some(directory),
            }),
        )
        .unwrap();
    history
        .append_conversation(
            event("message", "agent"),
            ConversationMessage::text(MessageRole::User, "committed despite projection failure"),
        )
        .await
        .unwrap();
    // source-line:776
    assert_eq!(repo.conversation("agent".into()).await.unwrap().len(), 1);
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn original_compaction_only_history_is_present() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    let compacted = user(&repo, "compacted", "source", "compacted").await;
    repo.append_compaction_status(
        event("status", "source"),
        CompactionSnapshot {
            summary: "dependency summary".into(),
            watermark: compacted,
        },
        "dependency status".into(),
    )
    .await
    .unwrap();
    // source-line:615
    assert!(
        repo.effective_conversation_groups("source".into())
            .await
            .unwrap()
            .groups
            .is_empty()
    );
    fork(
        &repo,
        "source",
        "child",
        HistoryForkBoundary::AfterSafeHistoryPrefix,
        "full",
        Some(HistoryForkSource {
            agent_session_id: "absent-parent".into(),
            boundary: HistoryForkBoundary::BeforeToolBatch {
                assistant_sequence: 100,
                tool_call_id: "absent".into(),
            },
        }),
    )
    .await
    .unwrap();
    let compaction = repo.compaction_history("child".into()).await.unwrap();
    // source-line:625
    assert_eq!(
        compaction
            .as_ref()
            .map(|value| value.snapshot.summary.as_str()),
        Some("dependency summary")
    );
    // source-line:626
    assert_eq!(
        compaction
            .as_ref()
            .and_then(|value| value.status.as_ref())
            .map(text),
        Some("dependency status")
    );
    // source-line:627
    assert!(
        repo.effective_conversation_groups("child".into())
            .await
            .unwrap()
            .groups
            .is_empty()
    );
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn original_safe_prefix_excludes_crossing_complete_batches() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    user(&repo, "before", "agent", "before").await;
    let checkpoint = append(
        &repo,
        "checkpoint",
        "agent",
        MessageRole::Assistant,
        vec![],
        vec![call("checkpoint", "set_checkpoint", "{}")],
    )
    .await;
    repo.record_checkpoint(
        "agent".into(),
        "handoff".into(),
        checkpoint,
        "checkpoint".into(),
    )
    .await
    .unwrap();
    settle(
        &repo,
        "checkpoint-result",
        "agent",
        checkpoint,
        "checkpoint",
        "set_checkpoint",
        "settled",
    )
    .await;
    let mut crossing = vec![];
    for id in ["early", "later", "incomplete"] {
        let sequence = append(
            &repo,
            id,
            "agent",
            MessageRole::Assistant,
            vec![],
            vec![call(id, "exec_command", "{}")],
        )
        .await;
        if id != "incomplete" {
            crossing.push((sequence, id));
        }
    }
    user(&repo, "after", "agent", "after").await;
    for (sequence, id) in crossing {
        settle(
            &repo,
            &format!("result-{id}"),
            "agent",
            sequence,
            id,
            "exec_command",
            id,
        )
        .await;
    }
    fork(
        &repo,
        "agent",
        "child",
        HistoryForkBoundary::AfterSafeHistoryPrefix,
        "full",
        None,
    )
    .await
    .unwrap();
    let child = repo.conversation("child".into()).await.unwrap();
    // source-line:532
    assert_eq!(child.len(), 3);
    // source-line:533
    assert_eq!(text(&child[0]), "before");
    // source-line:534
    assert_eq!(text(child.last().unwrap()), "settled");
    // source-line:535
    assert_eq!(
        repo.events()
            .tool_terminals("child".into())
            .await
            .unwrap()
            .iter()
            .map(|terminal| terminal.tool_call_id.as_str())
            .collect::<Vec<_>>(),
        vec!["checkpoint"]
    );
    // source-line:537
    assert!(
        repo.effective_conversation_groups("child".into())
            .await
            .unwrap()
            .groups
            .iter()
            .all(|group| group.is_complete)
    );
    let remapped = repo
        .latest_usable_checkpoint("child".into(), "handoff".into(), i64::MAX)
        .await
        .unwrap();
    // source-line:539
    assert_eq!(
        remapped.as_ref().map(|value| value.assistant_sequence),
        Some(child[1].sequence)
    );
    // source-line:540
    assert_ne!(
        remapped.as_ref().map(|value| value.assistant_sequence),
        Some(checkpoint)
    );
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn original_materialize_durable_image_at_provider_boundary() {
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";
    let fixture = Fixture::new();
    let store = ImageArtifactStore::for_session(&fixture.root(), "artifacts").unwrap();
    let bytes = STANDARD.decode(PNG).unwrap();
    let writer = store.clone();
    let artifact = tokio::task::spawn_blocking(move || {
        writer.persist(
            &mut std::io::Cursor::new(bytes),
            "pixel.png",
            "test",
            None,
            None,
            &NeverCancelled,
        )
    })
    .await
    .unwrap()
    .unwrap();
    let repo = fixture.repo().with_image_store(store.clone()).unwrap();
    let images = ImageArtifactRepository::new(repo.events().database().clone(), store).unwrap();
    images
        .record(artifact.clone(), "upload-1", &NeverCancelled)
        .await
        .unwrap();
    let contents = repo
        .materialize(vec![
            ConversationPart::text("before"),
            ConversationPart::image(
                artifact.artifact_id,
                artifact.media_type,
                artifact.display_name,
            ),
        ])
        .await
        .unwrap();
    // source-line:890
    assert_eq!(
        contents
            .iter()
            .map(|content| match content {
                ModelContent::Text(_) => "Text",
                ModelContent::ImageFile { .. } => "Image",
            })
            .collect::<Vec<_>>()
            .join(","),
        "Text,Image"
    );
    let ModelContent::ImageFile {
        path,
        media_type,
        width,
        height,
    } = &contents[1]
    else {
        panic!("source expected image content")
    };
    // source-line:892
    assert!(path.is_file());
    // source-line:893
    assert_eq!(STANDARD.encode(std::fs::read(path).unwrap()), PNG);
    // source-line:895
    assert_eq!(media_type, "image/png");
    // source-line:896
    assert_eq!((*width, *height), (1, 1));
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn original_earlier_occupant_cannot_refresh_current_projection() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    for session in ["first", "second"] {
        user(
            &repo,
            &format!("message-{session}"),
            session,
            &format!("{session} occupant"),
        )
        .await;
    }
    let history = Arc::new(Projection {
        root: fixture.root(),
        current: "second".into(),
        blocked_directory: None,
    });
    let current = repo
        .bind_agent_history("second".into(), history.clone())
        .unwrap();
    // source-line:746
    assert!(matches!(
        repo.bind_agent_history("first".into(), history)
            .unwrap()
            .refresh()
            .await,
        Err(EventError::Invalid(_))
    ));
    current.refresh().await.unwrap();
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn original_fallback_failure_leaves_destination_empty() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    // source-line:636
    assert!(matches!(
        fork(
            &repo,
            "absent",
            "child",
            HistoryForkBoundary::AfterSafeHistoryPrefix,
            "full",
            Some(HistoryForkSource {
                agent_session_id: "parent".into(),
                boundary: HistoryForkBoundary::BeforeToolBatch {
                    assistant_sequence: 1,
                    tool_call_id: "missing".into()
                }
            })
        )
        .await,
        Err(ForkError::Argument { .. })
    ));
    // source-line:643
    assert!(repo.conversation("child".into()).await.unwrap().is_empty());
    // source-line:644
    assert!(repo.agent_history("child".into()).await.unwrap().is_empty());
    // source-line:645
    assert!(
        repo.compaction_history("child".into())
            .await
            .unwrap()
            .is_none()
    );
    repo.events().database().close().await.unwrap();
}
