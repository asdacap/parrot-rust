use super::{fixture::*, harness::Harness, support::*};
use parrot_domain::values::agent_tasks::{ExecutionStatus::*, Task};
use parrot_runtime::children::ChildError;
use std::sync::Arc;

#[tokio::test]
async fn restarted_hidden_task_runs_on_retained_agent_and_hides_again_on_success() {
    let h = Harness::new();
    let owner = h.root("owner").await;
    let gate = Gate::new();
    h.provider
        .script("a", [Reply::text("answer"), Reply::held("answer", &gate)]);
    set(&owner, vec![task("a")]).await;
    assert!(complete(&owner).await[0].hidden);
    let retained = owner.children().find("a").unwrap();
    set(&owner, vec![state("a", Succeeded)]).await;
    assert!(!snapshot(&owner).await[0].hidden);
    set(
        &owner,
        vec![Task {
            hidden: true,
            ..task("a")
        }],
    )
    .await;
    gate.entered().await;
    assert!(snapshot(&owner).await[0].hidden);
    assert_eq!(snapshot(&owner).await[0].state, Running);
    set(&owner, vec![state("a", Running)]).await;
    assert!(!snapshot(&owner).await[0].hidden);
    gate.open();
    assert!(complete(&owner).await[0].hidden);
    assert!(Arc::ptr_eq(&retained, &owner.children().find("a").unwrap()));
    assert_eq!(requests(&h.provider, "a").len(), 2);
    h.close().await;
}

#[tokio::test]
async fn child_creation_result_and_failure_are_preserved_while_diagnostic_fields_exclude_private_payloads()
 {
    for failure in [false, true] {
        let h = Harness::new();
        let owner = h.root("owner").await;
        if failure {
            h.factory.plans.lock().unwrap().insert(
                "secret-child".into(),
                ScopePlan {
                    failure: Some(ChildError::failure("factory", "secret-failure-payload")),
                    ..Default::default()
                },
            );
        }
        set(&owner, vec![task("secret-child")]).await;
        let done = wait_tasks(&owner, |tasks| tasks[0].state != Running).await;
        assert_eq!(done[0].state, if failure { Failed } else { Succeeded });
        if failure {
            assert!(
                done[0]
                    .failure
                    .as_deref()
                    .unwrap()
                    .contains("secret-failure-payload")
            );
        } else {
            assert_eq!(done[0].result.as_deref(), Some("answer"));
        }
        {
            let diagnostics = h.diagnostics.0.lock().unwrap();
            let started = diagnostics
                .iter()
                .find(|entry| entry.event == "child_scope_start")
                .unwrap();
            let completed = diagnostics
                .iter()
                .find(|entry| entry.event == "child_scope_completed")
                .unwrap();
            assert_eq!(
                completed.outcome,
                Some(if failure { "failed" } else { "succeeded" })
            );
            assert_eq!(started.session_id, completed.session_id);
            for entry in diagnostics.iter() {
                assert!(!entry.session_id.contains("secret"));
                assert!(!entry.correlation_id.contains("secret"));
                assert!(!entry.error_code.unwrap_or("").contains("secret"));
            }
        }
        h.close().await;
    }
}
