use super::{harness::Harness, support::*};
use parrot_domain::protocol::messages::parrot::Event;
use parrot_runtime::children::*;
use parrot_storage::{
    conversation::{
        AgentHistoryView, ConversationMessage, HistoryForkSelection, MessageRole, ToolCall,
    },
    events::{ConversationPart, ToolExecutionStatus, ToolExecutionTerminal},
};

async fn append(history: &AgentHistoryView, message: ConversationMessage) -> i64 {
    history
        .append_conversation(
            Event {
                id: id(),
                agent_session_id: history.agent_session_id().into(),
                ..Default::default()
            },
            message,
        )
        .await
        .unwrap();
    history
        .conversation()
        .await
        .unwrap()
        .last()
        .unwrap()
        .sequence
}
async fn tool(history: &AgentHistoryView, text: &str, call: &str) -> i64 {
    let mut message = ConversationMessage::text(MessageRole::Assistant, text);
    message.tool_calls.push(ToolCall {
        id: call.into(),
        name: "set_checkpoint".into(),
        arguments_json: "{}".into(),
    });
    append(history, message).await
}
async fn text(history: &AgentHistoryView) -> Vec<String> {
    history
        .conversation()
        .await
        .unwrap()
        .into_iter()
        .flat_map(|item| item.message.parts.into_iter())
        .filter_map(|part| {
            if let ConversationPart::Text(value) = part {
                Some(value)
            } else {
                None
            }
        })
        .collect()
}
#[tokio::test]
async fn child_launch_uses_actual_empty_full_and_named_checkpoint_sqlite_forks() {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        h.histories
            .append(root.history(), "before checkpoint")
            .await;
        let checkpoint = tool(root.history(), "checkpoint assistant", "checkpoint-call").await;
        root.history()
            .record_checkpoint("handoff".into(), checkpoint, "checkpoint-call".into())
            .await
            .unwrap();
        root.history()
            .append_tool_settlement(
                Event {
                    id: id(),
                    agent_session_id: root.history().agent_session_id().into(),
                    ..Default::default()
                },
                checkpoint,
                ToolExecutionTerminal {
                    tool_call_id: "checkpoint-call".into(),
                    tool_name: "set_checkpoint".into(),
                    status: ToolExecutionStatus::Finished,
                    result_parts: vec![ConversationPart::text("checkpoint done")],
                    message: String::new(),
                },
            )
            .await
            .unwrap();
        h.histories.append(root.history(), "after checkpoint").await;
        let original = text(root.history()).await;
        let empty = h.spawn(&root, "empty").await;
        assert!(text(empty.history()).await.is_empty());
        let mut full = h.request(&root, "full");
        full.fork = HistoryForkSelection::parse("full");
        let full = bounded(root.spawner().spawn(full).unwrap().join())
            .await
            .unwrap();
        assert_eq!(text(full.history()).await, original);
        let mut named = h.request(&root, "named");
        named.fork = HistoryForkSelection::parse("handoff");
        let named = bounded(root.spawner().spawn(named).unwrap().join())
            .await
            .unwrap();
        assert_eq!(
            text(named.history()).await,
            [
                "checkpoint assistant",
                "checkpoint done",
                "after checkpoint"
            ]
        );
        let remapped = named
            .history()
            .repository()
            .latest_usable_checkpoint(
                named.history().agent_session_id().into(),
                "handoff".into(),
                i64::MAX,
            )
            .await
            .unwrap()
            .unwrap();
        assert_ne!(remapped.assistant_sequence, checkpoint);
        assert_eq!(remapped.tool_call_id, "checkpoint-call");
        assert!(
            std::fs::read_to_string(
                h.histories
                    .path
                    .join(format!("{}.jsonl", named.identity().session.session_id))
            )
            .unwrap()
            .contains("after checkpoint")
        );
        let mut missing = h.request(&root, "missing");
        missing.fork = HistoryForkSelection::parse("absent-checkpoint");
        assert!(
            bounded(root.spawner().spawn(missing).unwrap().join())
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("unavailable")
        );
        assert!(root.children().find("missing").is_none());
        assert_eq!(text(root.history()).await, original);
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn sibling_launch_uses_safe_prefix_empty_sibling_falls_back_and_foreign_source_is_rejected() {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        h.histories.append(root.history(), "parent fallback").await;
        let sibling = h.spawn(&root, "sibling").await;
        let mut fallback = h.request(&root, "fallback");
        fallback.fork = HistoryForkSelection::parse("full");
        fallback.history_source = HistorySource::Sibling(sibling.session().clone());
        let fallback = bounded(root.spawner().spawn(fallback).unwrap().join())
            .await
            .unwrap();
        assert_eq!(text(fallback.history()).await, ["parent fallback"]);
        h.histories
            .append(sibling.history(), "safe sibling prefix")
            .await;
        tool(sibling.history(), "incomplete sibling tool", "pending").await;
        let mut safe = h.request(&root, "safe");
        safe.fork = HistoryForkSelection::parse("full");
        safe.history_source = HistorySource::Sibling(sibling.session().clone());
        let safe = bounded(root.spawner().spawn(safe).unwrap().join())
            .await
            .unwrap();
        assert_eq!(text(safe.history()).await, ["safe sibling prefix"]);
        let other_root = h.root("other-root").await;
        let foreign = h.spawn(&other_root, "foreign").await;
        let mut rejected = h.request(&root, "rejected");
        rejected.history_source = HistorySource::Sibling(foreign.session().clone());
        rejected.fork = HistoryForkSelection::parse("full");
        assert!(
            bounded(root.spawner().spawn(rejected).unwrap().join())
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("registered sibling")
        );
        assert!(root.children().find("rejected").is_none());
        h.close().await;
    })
    .await;
}
