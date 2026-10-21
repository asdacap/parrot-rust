//! Original WaitToolTests predicates exercised through a test-local AgentRuntime
//! bridge, not canned wrapper responses. The production P08 composition is open.
#[path = "support_wait/mod.rs"]
mod support;

use std::{sync::Arc, time::Duration};

use parrot_domain::contracts::{
    async_ops::OperationError,
    status::{ActiveWorkObservation, ActiveWorkState, ProcessStatusSnapshot, ProcessStatusSource},
};
use parrot_domain::protocol::messages::parrot as protocol;
use parrot_provider::sessions;
use parrot_runtime::{
    active_work::blockers::ProcessBlocker,
    agent_turn::lifecycle::{DrainBackend, DrainState, IncomingActivity, Signal},
};
use parrot_storage::events::{ConversationPart, NeverCancelled};

use super::NEVER;
use support::{Harness, bounded, done_completion, wait_completion};

#[tokio::test]
async fn already_pending_durable_input_wakes_the_real_wait_tool() {
    let h = Harness::new(vec![]).await;
    h.session
        .backend()
        .admit(
            vec![ConversationPart::text("already here")],
            "pending".into(),
            protocol::Delivery::DELIVERY_STEER,
            protocol::InputSource::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        bounded(h.invoke_wait("{}", &NEVER)).await.unwrap(),
        "wait interrupted"
    );
    assert!(h.provider.requests.lock().unwrap().is_empty());
    h.close().await;
}

#[tokio::test]
async fn real_wait_tool_preserves_agent_task_and_process_completion_causes() {
    for (name, cause) in [
        ("researcher", "researcher completion"),
        ("graph-call", "AgentTask graph graph-call completion"),
        ("compiler", "process compiler completion"),
    ] {
        let h = Harness::new(vec![]).await;
        let provider = h.provider.step(done_completion());
        let waiting = h.invoke_wait("{}", &NEVER);
        tokio::pin!(waiting);
        let (sent, result) = tokio::join!(
            async {
                h.bridge.entered().await;
                h.send(
                    "completed",
                    "completion",
                    IncomingActivity {
                        name: name.into(),
                        cause: Some(cause.into()),
                        source: protocol::InputSource::default(),
                    },
                )
                .await
            },
            bounded(&mut waiting)
        );
        assert_eq!(result.unwrap(), format!("wait interrupted due to {cause}"));
        provider.open();
        bounded(sent.execution.join()).await.unwrap();
        h.close().await;
    }
}

#[tokio::test(start_paused = true)]
async fn cancellation_of_real_wait_tool_clears_the_session_registration() {
    let h = Harness::new(vec![]).await;
    let cancelled = Signal::new();
    let waiting = h.invoke_wait("{}", &cancelled);
    tokio::pin!(waiting);
    let (_, result) = tokio::join!(
        async {
            h.bridge.entered().await;
            cancelled.cancel();
        },
        &mut waiting
    );
    assert_eq!(result, Err(OperationError::Cancelled));
    // A leftover registration makes this fail immediately as already active;
    // the real second registration must instead live to its own timer deadline.
    let resumed = tokio::time::Instant::now();
    assert_eq!(
        h.session
            .wait_for_incoming_input(Duration::from_secs(1), &NEVER)
            .await
            .unwrap(),
        None
    );
    assert_eq!(resumed.elapsed(), Duration::from_secs(1));
    h.close().await;
}

struct Processes;
impl ProcessStatusSource for Processes {
    fn active(&self) -> Vec<ActiveWorkObservation> {
        vec![
            ActiveWorkObservation {
                id: "agent/z".into(),
                name: "zebra".into(),
                state: ActiveWorkState::Running,
            },
            ActiveWorkObservation {
                id: "agent/compiler".into(),
                name: "compiler".into(),
                state: ActiveWorkState::Running,
            },
        ]
    }
    fn snapshot(&self) -> Vec<ProcessStatusSnapshot> {
        vec![]
    }
}

#[tokio::test(start_paused = true)]
async fn real_wait_tool_timeout_includes_actual_process_blocker_section() {
    // Process observation is a test domain port, not an OS process adapter claim.
    // Formatting/observation and timer behavior are the actual runtime services.
    let h = Harness::new(vec![Arc::new(ProcessBlocker {
        processes: Arc::new(Processes),
    })])
    .await;
    let started = tokio::time::Instant::now();
    assert_eq!(
        h.invoke_wait("{}", &NEVER).await.unwrap(),
        "Wait timed out after 300000 ms.\nRunning processes:\n- agent/compiler (name: compiler)\n- agent/z (name: zebra)"
    );
    assert_eq!(started.elapsed(), Duration::from_millis(300000));
    h.close().await;
}

fn assert_waking_request(h: &Harness, expected: &str) {
    let requests = h.provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["wait"]
    );
    let next = &requests[1];
    assert_eq!(
        next.messages
            .iter()
            .filter(|message| message.role == sessions::Role::User && message.content() == expected)
            .count(),
        1
    );
    let results = next
        .messages
        .iter()
        .filter(|message| message.role == sessions::Role::Tool)
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].tool_call_id, "wait-call");
    assert_eq!(results[0].content(), "wait interrupted");
    assert_eq!(h.session.state(), DrainState::Running);
    assert_eq!(h.activity.capture().state, DrainState::Running);
}

async fn assert_one_tool_lifecycle(h: &Harness) {
    let events = h.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_tool_started())
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_tool_finished())
            .count(),
        1
    );
    assert!(
        !events
            .iter()
            .any(|event| event.has_tool_error() || event.has_tool_cancelled())
    );
    assert_eq!(h.session.state(), DrainState::Idle);
    assert_eq!(h.activity.capture().state, DrainState::Idle);
}

#[tokio::test]
async fn retained_wait_round_ignores_duplicate_and_promotes_new_message_once() {
    let h = Harness::new(vec![]).await;
    let first = h.provider.step(wait_completion());
    let second = h.provider.step(done_completion());
    h.send("first", "message-1", IncomingActivity::default())
        .await;
    first.entered().await;
    first.open();
    h.bridge.entered().await;
    assert_eq!(h.activity.capture().current_tool.as_deref(), Some("wait"));
    h.send("first", "message-1", IncomingActivity::default())
        .await;
    assert!(
        tokio::time::timeout(Duration::from_millis(20), second.entered())
            .await
            .is_err(),
        "a duplicate message woke the retained wait"
    );
    assert_eq!(h.provider.requests.lock().unwrap().len(), 1);
    assert_eq!(h.activity.capture().current_tool.as_deref(), Some("wait"));
    let sent = h
        .send("wakeup", "message-2", IncomingActivity::default())
        .await;
    second.entered().await;
    assert_waking_request(&h, "wakeup");
    second.open();
    let result = bounded(sent.execution.join()).await.unwrap();
    assert_eq!(result.output, "done");
    h.close().await;
    assert_one_tool_lifecycle(&h).await;
}

#[tokio::test]
async fn queued_send_and_wait_wakes_retained_wait_and_returns_done() {
    let h = Harness::new(vec![]).await;
    let first = h.provider.step(wait_completion());
    let second = h.provider.step(done_completion());
    h.send("first", "message-1", IncomingActivity::default())
        .await;
    first.entered().await;
    first.open();
    h.bridge.entered().await;
    let session = h.session.clone();
    let queued = tokio::spawn(async move {
        session
            .send_and_wait(
                vec![ConversationPart::text("queued")],
                "message-2".into(),
                None,
                None,
                Arc::new(NeverCancelled),
            )
            .await
    });
    second.entered().await;
    assert_waking_request(&h, "queued");
    second.open();
    assert_eq!(bounded(queued).await.unwrap().unwrap(), "done");
    h.close().await;
    assert_one_tool_lifecycle(&h).await;
}
