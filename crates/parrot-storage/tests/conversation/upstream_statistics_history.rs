use parrot_domain::contracts::paths::{RootOrigin, SelectedSessionRoot};
use parrot_domain::protocol::messages::parrot::{
    Event, RequestUsageRecorded, ToolExecutionStarted,
};
use parrot_storage::conversation::{
    AgentHistoryEntry, CompactionSnapshot, ConversationMessage, ConversationRepository,
    HistoryForkBoundary, HistoryForkSelection, MessageRole,
};
use parrot_storage::database::SessionDatabase;
use parrot_storage::events::EventRepository;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "parrot-t18-original-statistics-history-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn root(&self) -> SelectedSessionRoot {
        SelectedSessionRoot::new(self.0.clone(), RootOrigin::Current).unwrap()
    }
    fn repository(&self, memory: bool) -> ConversationRepository {
        let database = if memory {
            SessionDatabase::open_memory(self.root()).unwrap()
        } else {
            SessionDatabase::open(self.root()).unwrap()
        };
        ConversationRepository::new(EventRepository::new(database))
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
fn requests(entries: &[AgentHistoryEntry]) -> Vec<&AgentHistoryEntry> {
    entries
        .iter()
        .filter(|entry| matches!(entry, AgentHistoryEntry::Request { .. }))
        .collect()
}
fn single_request(entries: &[AgentHistoryEntry]) -> &AgentHistoryEntry {
    let entries = requests(entries);
    assert_eq!(entries.len(), 1, "source OfType<RequestEntry>().Single()");
    entries[0]
}

#[tokio::test]
async fn durable_facts_reopen_without_aggregate_projection_or_current_pricing_history_literals() {
    let fixture = Fixture::new();
    let repository = fixture.repository(false);
    let mut request = event("request", "main");
    request.set_request_usage_recorded(RequestUsageRecorded {
        request_id: "request".into(),
        provider: "provider".into(),
        model: "model".into(),
        input_tokens: 100,
        input_cost: 7.0,
        output_cost: 3.0,
        tool_call_ids: vec!["call".into()],
        ..Default::default()
    });
    repository
        .events()
        .append_usage_fact(request)
        .await
        .unwrap();
    repository.events().database().close().await.unwrap();

    let reopened = fixture.repository(false);
    let _replay = reopened.events().replay_statistics().await.unwrap();
    let sessions = reopened.agent_history_session_ids().await.unwrap();
    assert_eq!(sessions.len(), 1, "source session IDs Single()");
    assert_eq!(sessions[0], "main");
    let history = reopened.agent_history("main".into()).await.unwrap();
    let AgentHistoryEntry::Request {
        input_cost,
        output_cost,
        ..
    } = single_request(&history)
    else {
        unreachable!("single_request filters the request variant")
    };
    assert_eq!(input_cost + output_cost, 10.0);
    reopened.events().database().close().await.unwrap();
}

#[tokio::test]
async fn facts_replay_tree_request_history_null_effort_original_literals() {
    facts_replay_tree_request_history_original_literals(None).await;
}

#[tokio::test]
async fn facts_replay_tree_request_history_high_effort_original_literals() {
    facts_replay_tree_request_history_original_literals(Some("high")).await;
}

async fn facts_replay_tree_request_history_original_literals(effort: Option<&str>) {
    let fixture = Fixture::new();
    let repository = fixture.repository(true);
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
        let request_id = child.id.clone();
        child.mut_request_usage_recorded().request_id = request_id;
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
    let _replay = repository.events().replay_statistics().await.unwrap();

    assert!(
        repository
            .model_history("grandchild".into())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(requests(&repository.agent_history("fork".into()).await.unwrap()).is_empty());
    let grandchild = repository.agent_history("grandchild".into()).await.unwrap();
    let AgentHistoryEntry::Request {
        tool_calls,
        input_cost,
        output_cost,
        effort: recorded_effort,
        ..
    } = single_request(&grandchild)
    else {
        unreachable!("single_request filters the request variant")
    };
    assert_eq!(*tool_calls, 1);
    assert_eq!(input_cost + output_cost, 0.75);
    assert_eq!(recorded_effort.as_deref(), effort);
    let main_history = repository.agent_history("main".into()).await.unwrap();
    assert_eq!(requests(&main_history).len(), 1);
    assert_eq!(
        main_history
            .iter()
            .filter(|entry| matches!(entry, AgentHistoryEntry::Message { .. }))
            .count(),
        1
    );
    repository
        .save_compaction(
            "main".into(),
            CompactionSnapshot {
                summary: "summary".into(),
                watermark: 1,
            },
        )
        .await
        .unwrap();
    let compacted = repository.agent_history("main".into()).await.unwrap();
    let AgentHistoryEntry::Request { input_tokens, .. } = single_request(&compacted) else {
        unreachable!("single_request filters the request variant")
    };
    assert_eq!(*input_tokens, 100);
    repository.events().database().close().await.unwrap();
}
