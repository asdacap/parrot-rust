use super::{fixture::*, harness::Harness, support::*};
use parrot_domain::values::agent_tasks::ExecutionStatus::Canceled;
use parrot_runtime::task_scheduler::PROGRESS_KIND;
use std::{future::Future, task::Poll};

// Intentional retained-join strengthening, not a claim that source removes its
// execution bookkeeping after the final stop Record (it removes it before Record).
#[tokio::test]
async fn settlement_joins_stopped_generation_until_actual_stop_record_is_durable() {
    let h = Harness::new();
    let owner = h.root("owner").await;
    let provider = Gate::new();
    let stop_record = Gate::new();
    h.provider.script("a", [Reply::held("answer", &provider)]);
    set(&owner, vec![task("a")]).await;
    provider.entered().await;
    let worker = h.factory.get("a");
    *worker.task_session.record_gate.lock().unwrap() = Some(stop_record.clone());
    set(&owner, vec![state("a", Canceled)]).await;
    stop_record.entered().await;
    assert!(
        !h.histories
            .repository
            .events()
            .replay()
            .await
            .unwrap()
            .iter()
            .any(
                |event| event.agent_session_id == worker.history.agent_session_id()
                    && event.has_input_admitted()
                    && event
                        .input_admitted()
                        .content
                        .contains("Your AgentTask turn was stopped:")
            )
    );
    let revision = h
        .histories
        .repository
        .events()
        .replay()
        .await
        .unwrap()
        .iter()
        .filter(|event| {
            event.has_tool_snapshot()
                && event.agent_session_id == owner.identity().session.session_id
                && event.tool_snapshot().kind == PROGRESS_KIND
        })
        .map(|event| event.tool_snapshot().revision)
        .max()
        .unwrap();
    let scheduler = owner.get_service::<Scheduler>().unwrap();
    let settlement = scheduler.settle();
    // Observe settlement's actual durable Publish before checking its join, so this
    // does not pass merely because an unpolled settlement future is still pending.
    bounded(async {
        loop {
            let events = h.histories.repository.events().replay().await.unwrap();
            if events.iter().any(|event| {
                event.has_tool_snapshot()
                    && event.agent_session_id == owner.identity().session.session_id
                    && event.tool_snapshot().kind == PROGRESS_KIND
                    && event.tool_snapshot().revision > revision
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    let mut joined = Box::pin(settlement.join());
    assert!(
        std::future::poll_fn(|cx| Poll::Ready(matches!(joined.as_mut().poll(cx), Poll::Pending)))
            .await,
        "settlement completed while the old generation's real stop record was blocked"
    );
    stop_record.open();
    bounded(joined).await.unwrap();
    assert!(
        h.histories
            .repository
            .events()
            .replay()
            .await
            .unwrap()
            .iter()
            .any(
                |event| event.agent_session_id == worker.history.agent_session_id()
                    && event.has_input_admitted()
                    && event
                        .input_admitted()
                        .content
                        .contains("Your AgentTask turn was stopped: the owner canceled this task.")
            )
    );
    h.close().await;
}
