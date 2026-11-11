//! Runtime bindings for the exact delegated literals in work-items.jsonl.
//! The scratch/blob-directory literal is intentionally not imitated by a path join:
//! its real API belongs to parrot-foundation, absent from this crate's dependencies.
use super::AgentHistoryFile;
use super::json::serialize_entry;
use crate::conversation::{
    AgentHistoryEntry, AgentHistoryView, CompactionSnapshot, ConversationMessage, ConversationPart,
    ConversationRepository, HistoryForkBoundary, HistoryForkSelection, MessageRole, ToolCall,
};
use crate::database::SessionDatabase;
use crate::events::{EventRepository, ToolExecutionStatus, ToolExecutionTerminal};
use parrot_domain::contracts::paths::{RootOrigin, SelectedSessionRoot};
use parrot_domain::protocol::messages::parrot::{
    Delivery, Event, RequestUsageRecorded, ToolExecutionStarted,
};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "parrot-t19-exact24-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn root(&self) -> SelectedSessionRoot {
        SelectedSessionRoot::new(self.0.clone(), RootOrigin::Current).unwrap()
    }
    fn repository(&self) -> ConversationRepository {
        ConversationRepository::new(
            EventRepository::new(SessionDatabase::open(self.root()).unwrap()),
            crate::history_files::tests::metadata(&self.root()),
        )
        .unwrap()
    }
    fn projection(&self, name: &str, occupants: &[&str]) -> Arc<AgentHistoryFile> {
        Arc::new(
            AgentHistoryFile::new(
                self.root(),
                self.0.join("root-agents").join(name).join("history.jsonl"),
                occupants.iter().map(|id| (*id).into()).collect(),
            )
            .unwrap(),
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn event(id: &str, agent: &str) -> Event {
    Event {
        id: id.into(),
        agent_session_id: agent.into(),
        ..Default::default()
    }
}

async fn append(view: &AgentHistoryView, id: &str, text: &str) {
    view.append_conversation(
        event(id, view.agent_session_id()),
        ConversationMessage::text(MessageRole::User, text),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn delegated24_concurrent_exact_jsonl_cleanup_and_isolation() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    let first_file = fixture.projection("first", &["first"]);
    let second_file = fixture.projection("second", &["second"]);
    let first = repository
        .bind_agent_history("first".into(), first_file.clone())
        .unwrap();
    let second = repository
        .bind_agent_history("second".into(), second_file.clone())
        .unwrap();
    let mut tasks = Vec::new();
    for index in 0..24 {
        let view = if index % 2 == 0 {
            first.clone()
        } else {
            second.clone()
        };
        tasks.push(tokio::spawn(async move {
            let agent = view.agent_session_id().to_owned();
            append(
                &view,
                &format!("message-{index}"),
                &format!("{agent}-{index}"),
            )
            .await;
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    for (file, agent) in [(&first_file, "first"), (&second_file, "second")] {
        let mut expected = Vec::new();
        for entry in repository.agent_history(agent.into()).await.unwrap() {
            expected.extend(serialize_entry(&entry).unwrap());
            expected.push(b'\n');
        }
        assert_eq!(fs::read(file.path()).unwrap(), expected);
        assert_eq!(
            repository.conversation(agent.into()).await.unwrap().len(),
            12
        );
    }
    let wrong = repository
        .bind_agent_history("second".into(), first_file.clone())
        .unwrap();
    assert!(
        wrong
            .repository()
            .cleanup_forked_agent_history(wrong.agent_session_id().into())
            .await
            .is_err()
    );
    assert_eq!(
        repository
            .conversation("second".into())
            .await
            .unwrap()
            .len(),
        12
    );
    first
        .repository()
        .cleanup_forked_agent_history(first.agent_session_id().into())
        .await
        .unwrap();
    assert!(fs::read(first_file.path()).unwrap().is_empty());
    assert!(
        fs::read_to_string(second_file.path())
            .unwrap()
            .contains("second-")
    );
    repository.events().database().close().await.unwrap();
}

#[tokio::test]
async fn delegated24_named_checkpoint_fork_jsonl_literals() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    let parent_file = fixture.projection("agent", &["agent"]);
    let child_file = fixture.projection("child", &["child"]);
    let parent = repository
        .bind_agent_history("agent".into(), parent_file.clone())
        .unwrap();
    let child = repository
        .bind_agent_history("child".into(), child_file.clone())
        .unwrap();
    append(&parent, "before", "before").await;
    let mut checkpoint = ConversationMessage::text(MessageRole::Assistant, "");
    checkpoint.tool_calls.push(ToolCall {
        id: "checkpoint".into(),
        name: "set_checkpoint".into(),
        arguments_json: "{\"title\":\"handoff\"}".into(),
    });
    parent
        .append_conversation(event("checkpoint-call", "agent"), checkpoint)
        .await
        .unwrap();
    let checkpoint_assistant = parent.conversation().await.unwrap()[1].sequence;
    parent
        .append_tool_settlement(
            event("checkpoint-result", "agent"),
            checkpoint_assistant,
            ToolExecutionTerminal {
                tool_call_id: "checkpoint".into(),
                tool_name: "set_checkpoint".into(),
                status: ToolExecutionStatus::Finished,
                result_parts: vec![ConversationPart::Text("handoff".into())],
            },
        )
        .await
        .unwrap();
    append(&parent, "between", "between").await;
    let mut spawn = ConversationMessage::text(MessageRole::Assistant, "");
    spawn.tool_calls.push(ToolCall {
        id: "spawn".into(),
        name: "agent_spawn".into(),
        arguments_json: "{}".into(),
    });
    parent
        .append_conversation(event("spawn-call", "agent"), spawn)
        .await
        .unwrap();
    let spawn_assistant = parent
        .conversation()
        .await
        .unwrap()
        .last()
        .unwrap()
        .sequence;
    child
        .repository()
        .initialize_forked_agent_history(
            "agent".into(),
            child.agent_session_id().into(),
            HistoryForkBoundary::BeforeToolBatch {
                assistant_sequence: spawn_assistant,
                tool_call_id: "spawn".into(),
            },
            HistoryForkSelection::parse("handoff"),
            None,
        )
        .await
        .unwrap();
    assert!(
        fs::read_to_string(parent_file.path())
            .unwrap()
            .contains("before")
    );
    let child_text = fs::read_to_string(child_file.path()).unwrap();
    assert!(
        child_text.contains("handoff")
            && child_text.contains("between")
            && !child_text.contains("before")
    );
    repository.events().database().close().await.unwrap();
}

#[tokio::test]
async fn delegated24_cleanup_jsonl_durable_then_empty() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    let file = fixture.projection("agent", &["agent"]);
    let view = repository
        .bind_agent_history("agent".into(), file.clone())
        .unwrap();
    append(&view, "message", "durable").await;
    assert!(fs::read_to_string(file.path()).unwrap().contains("durable"));
    view.repository()
        .cleanup_forked_agent_history(view.agent_session_id().into())
        .await
        .unwrap();
    assert_eq!(fs::read_to_string(file.path()).unwrap(), "");
    repository.events().database().close().await.unwrap();
}

#[tokio::test]
async fn delegated24_promoted_input_compaction_and_destination_fork_jsonl() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    let parent_file = fixture.projection("parent", &["parent"]);
    let child_file = fixture.projection("child", &["child"]);
    let parent = repository
        .bind_agent_history("parent".into(), parent_file.clone())
        .unwrap();
    let child = repository
        .bind_agent_history("child".into(), child_file.clone())
        .unwrap();
    parent
        .admit_parts(
            "input".into(),
            vec![ConversationPart::Text("promoted input".into())],
            Delivery::DELIVERY_QUEUE,
            |input| Ok(event(&input.id, "parent")),
        )
        .await
        .unwrap();
    parent
        .promote_next_queue(|input| Ok(event(&format!("promoted-{}", input.id), "parent")))
        .await
        .unwrap();
    assert!(
        fs::read_to_string(parent_file.path())
            .unwrap()
            .contains("promoted input")
    );
    let watermark = parent
        .conversation()
        .await
        .unwrap()
        .last()
        .unwrap()
        .sequence;
    parent
        .append_compaction_status(
            event("compaction", "parent"),
            CompactionSnapshot {
                summary: "saved summary".into(),
                watermark,
            },
            "status".into(),
        )
        .await
        .unwrap();
    let parent_text = fs::read_to_string(parent_file.path()).unwrap();
    assert!(parent_text.contains("saved summary"));
    child
        .repository()
        .initialize_forked_agent_history(
            "parent".into(),
            child.agent_session_id().into(),
            HistoryForkBoundary::AfterCompletedHistory,
            HistoryForkSelection::parse("full"),
            None,
        )
        .await
        .unwrap();
    assert!(
        fs::read_to_string(child_file.path())
            .unwrap()
            .contains("saved summary")
    );
    child
        .repository()
        .cleanup_forked_agent_history(child.agent_session_id().into())
        .await
        .unwrap();
    assert!(fs::read_to_string(child_file.path()).unwrap().is_empty());
    assert_eq!(fs::read_to_string(parent_file.path()).unwrap(), parent_text);
    repository.events().database().close().await.unwrap();
}

#[tokio::test]
async fn delegated24_earlier_occupant_then_current_exact_literals() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    for agent in ["first", "second"] {
        repository
            .append_conversation(
                event(&format!("message-{agent}"), agent),
                ConversationMessage::text(MessageRole::User, format!("{agent} occupant")),
            )
            .await
            .unwrap();
    }
    let file = fixture.projection("main/worker", &["first", "second"]);
    let current = repository
        .bind_agent_history("second".into(), file.clone())
        .unwrap();
    assert!(
        repository
            .bind_agent_history("first".into(), file.clone())
            .unwrap()
            .refresh()
            .await
            .is_err()
    );
    current.refresh().await.unwrap();
    let text = fs::read_to_string(file.path()).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("first occupant"));
    assert!(lines[1].contains("second occupant"));
    repository.events().database().close().await.unwrap();
}

#[tokio::test]
async fn delegated24_request_round_trip_null_effort() {
    request_round_trip(None, ".1.1.0").await;
}
#[tokio::test]
async fn delegated24_request_round_trip_high_effort() {
    request_round_trip(Some("high"), ".2.1.0").await;
}
async fn request_round_trip(effort: Option<&str>, _parameter: &str) {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    let mut main = event("main-request", "main");
    main.set_request_usage_recorded(RequestUsageRecorded {
        request_id: "main-request".into(),
        provider: "provider".into(),
        model: "model".into(),
        input_tokens: 100,
        cached_input_tokens: 20,
        output_tokens: 10,
        input_cost: 0.5,
        output_cost: 0.25,
        context_size: 100,
        context_limit: 1000,
        tool_call_ids: vec!["call".into()],
        effort: effort.map(str::to_owned),
        ..Default::default()
    });
    repository
        .events()
        .append_usage_fact(main.clone())
        .await
        .unwrap();
    repository
        .events()
        .append_usage_fact(main.clone())
        .await
        .unwrap();
    repository
        .append_conversation(
            event("message", "main"),
            ConversationMessage::text(MessageRole::Assistant, "answer"),
        )
        .await
        .unwrap();
    repository
        .initialize_forked_agent_history(
            "main".into(),
            "fork".into(),
            HistoryForkBoundary::AfterCompletedHistory,
            HistoryForkSelection::parse("full"),
            None,
        )
        .await
        .unwrap();
    for (agent, parent) in [
        ("child", "main"),
        ("grandchild", "child"),
        ("sibling", "main"),
    ] {
        let mut child = main.clone();
        child.id = format!("{agent}-request");
        child.agent_session_id = agent.into();
        child.mut_request_usage_recorded().request_id = child.id.clone();
        child.mut_request_usage_recorded().parent_agent_session_id = parent.into();
        repository.events().append_usage_fact(child).await.unwrap();
    }
    let mut tool = event("tool-start", "grandchild");
    tool.set_tool_execution_started(ToolExecutionStarted {
        request_id: "grandchild-request".into(),
        assistant_sequence: 1,
        tool_call_id: "call".into(),
        tool_name: "status".into(),
        parent_agent_session_id: "child".into(),
        provider: "provider".into(),
        model: "model".into(),
        effort: effort.map(str::to_owned),
        ..Default::default()
    });
    repository
        .events()
        .append_usage_fact(tool.clone())
        .await
        .unwrap();
    repository.events().append_usage_fact(tool).await.unwrap();
    repository.events().replay_statistics().await.unwrap();
    let requests: Vec<_> = repository
        .agent_history("grandchild".into())
        .await
        .unwrap()
        .into_iter()
        .filter(|entry| matches!(entry, AgentHistoryEntry::Request { .. }))
        .collect();
    assert_eq!(requests.len(), 1);
    let serialized: serde_json::Value =
        serde_json::from_slice(&serialize_entry(&requests[0]).unwrap()).unwrap();
    assert_eq!(serialized["effort"], serde_json::json!(effort));
    let file = fixture.projection("grandchild", &["grandchild"]);
    repository
        .bind_agent_history("grandchild".into(), file.clone())
        .unwrap()
        .refresh()
        .await
        .unwrap();
    repository.events().database().close().await.unwrap();
}
