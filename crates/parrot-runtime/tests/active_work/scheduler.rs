//! Exercise the T52 adapter against the actual accepted scheduler and retained scopes.
// These are existing fixture modules only, not another target's test cases.
#[allow(dead_code)]
#[path = "../task_scheduler/admission_publisher.rs"]
mod admission_publisher;
#[allow(dead_code)]
#[path = "../task_scheduler/fixture.rs"]
mod fixture;
#[allow(dead_code)]
#[path = "../task_scheduler/harness.rs"]
mod harness;
#[allow(dead_code)]
#[path = "../task_scheduler/support.rs"]
mod support;

use fixture::*;
use harness::Harness;
use parrot_domain::values::agent_tasks::ExecutionStatus::*;
use parrot_runtime::active_work::blockers::{ActiveWorkBlocker, TaskBlocker};
use support::*;

#[tokio::test]
async fn actual_scheduler_snapshot_blocks_owned_hidden_running_pending_and_failed_until_owner_decides()
 {
    let h = Harness::new();
    let owner = h.root("owner").await;
    let other = h.root("other").await;
    let gate = Gate::new();
    h.provider.script("a", [Reply::held("answer", &gate)]);
    let mut hidden = state("failed", Failed);
    hidden.hidden = true;
    set(&owner, vec![task("a"), dependent("b", "a"), hidden]).await;
    gate.entered().await;
    set(&other, vec![state("foreign", Failed)]).await;
    let blocker = TaskBlocker {
        tasks: owner.get_service::<Scheduler>().unwrap(),
        templates: templates(),
    };
    let observed = blocker.observe().await.unwrap().unwrap();
    let text = observed.work_section.unwrap();
    assert!(text.contains("a [running] (name: Do a)"));
    assert!(text.contains("b [pending] (name: Do b)"));
    assert!(text.contains("failed [failed] (name: Do failed)"));
    assert!(!text.contains("foreign"));
    assert_eq!(observed.reminder, None);
    assert_eq!(
        snapshot(&owner)
            .await
            .iter()
            .map(|task| task.state)
            .collect::<Vec<_>>(),
        [Running, Pending, Failed]
    );
    assert!(blocker.observe().await.unwrap().is_some());
    set(
        &owner,
        vec![state("a", Canceled), state("failed", Succeeded)],
    )
    .await;
    assert_eq!(blocker.observe().await.unwrap(), None);
    assert!(
        snapshot(&owner)
            .await
            .iter()
            .all(|task| matches!(task.state, Canceled | Succeeded))
    );
    assert_eq!(snapshot(&other).await[0].state, Failed);
    h.close().await;
}
