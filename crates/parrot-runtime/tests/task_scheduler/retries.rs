use super::{fixture::*, harness::Harness, support::*};
use parrot_domain::values::agent_tasks::{AcceptanceVerdict::*, ExecutionStatus::*, LeafResponse};
use parrot_runtime::task_scheduler::TaskConfiguration;

#[tokio::test]
async fn retry_attempt_limit_replacement_instruction_carried_result_and_feedback_are_real_turns() {
    let h = Harness::new();
    let mut configuration = TaskConfiguration::defaults(templates());
    configuration.maximum_attempts = 3;
    *h.factory.configuration.lock().unwrap() = Some(configuration);
    let root = h.root("owner").await;
    for index in 1..=3 {
        h.factory.parser.answer(
            &format!("retry-{index}"),
            LeafResponse {
                result: format!("a {index}"),
                verdict: RejectAndRetry {
                    feedback: format!("fix {index}"),
                    payload: Some("Work again".into()),
                    replacement_result: Some(format!("replacement {index}")),
                },
            },
        );
    }
    h.provider.script(
        "a",
        (1..=3).map(|index| Reply::text(&format!("retry-{index}"))),
    );
    set(&root, vec![task("a")]).await;
    let done = wait_tasks(&root, |tasks| tasks[0].state == Failed).await;
    assert_eq!(done[0].result.as_deref(), Some("replacement 3"));
    assert_eq!(
        done[0].failure.as_deref(),
        Some("rejected after 3 attempts: fix 3")
    );
    let calls = requests(&h.provider, "a");
    assert_eq!(calls.len(), 3);
    assert!(prompt(&calls[1]).contains("Work again"));
    assert!(prompt(&calls[1]).contains("replacement 1"));
    assert!(prompt(&calls[2]).contains("Retry feedback:\n- fix 1\n- fix 2"));
    assert!(prompt(&calls[2]).contains("replacement 2"));
    assert_eq!(
        h.factory.parser.leaf_inputs.lock().unwrap().as_slice(),
        ["retry-1", "retry-2", "retry-3"]
    );
    let history = h.factory.get("a").history.conversation().await.unwrap();
    assert_eq!(history.iter().filter(|item| item.message.role == parrot_storage::conversation::MessageRole::Assistant).count(), 3);
    assert_eq!(h.registry.snapshot_scopes().len(), 2);
    h.close().await;
}

#[tokio::test]
async fn parser_repair_limit_is_separate_from_attempt_limit_and_invalid_responses_are_not_accepted()
{
    for succeeds in [false, true] {
        let h = Harness::new();
        let mut configuration = TaskConfiguration::defaults(templates());
        configuration.maximum_attempts = 1;
        configuration.maximum_response_repairs = 1;
        *h.factory.configuration.lock().unwrap() = Some(configuration);
        let root = h.root("owner").await;
        h.factory
            .parser
            .error("invalid-one", "scripted invalid first envelope");
        h.factory
            .parser
            .error("invalid-two", "scripted invalid second envelope");
        h.provider.script(
            "a",
            [
                Reply::text("invalid-one"),
                Reply::text(if succeeds { "answer" } else { "invalid-two" }),
            ],
        );
        set(&root, vec![task("a")]).await;
        let done = wait_tasks(&root, |tasks| tasks[0].state != Running).await;
        assert_eq!(done[0].state, if succeeds { Succeeded } else { Failed });
        assert_eq!(
            done[0].failure.as_deref(),
            if succeeds {
                None
            } else {
                Some("leaf response invalid: scripted invalid second envelope")
            }
        );
        let calls = requests(&h.provider, "a");
        assert_eq!(calls.len(), 2);
        assert!(prompt(&calls[1]).contains("Your previous response was not accepted"));
        assert!(prompt(&calls[1]).contains("scripted invalid first envelope"));
        assert_eq!(
            h.factory.parser.leaf_inputs.lock().unwrap().as_slice(),
            [
                "invalid-one",
                if succeeds { "answer" } else { "invalid-two" }
            ]
        );
        h.close().await;
    }
}

#[tokio::test]
async fn halt_and_accept_terminate_without_consuming_another_attempt() {
    for accept_final in [false, true] {
        let h = Harness::new();
        let root = h.root("owner").await;
        h.factory.parser.answer(
            "terminal",
            LeafResponse {
                result: "terminal result".into(),
                verdict: if accept_final {
                    Accept
                } else {
                    RejectAndHalt {
                        feedback: "nope".into(),
                    }
                },
            },
        );
        h.provider.script("a", [Reply::text("terminal")]);
        set(&root, vec![task("a")]).await;
        let done = wait_tasks(&root, |tasks| tasks[0].state != Running).await;
        assert_eq!(done[0].state, if accept_final { Succeeded } else { Failed });
        assert_eq!(done[0].result.as_deref(), Some("terminal result"));
        assert_eq!(
            done[0].failure.as_deref(),
            if accept_final { None } else { Some("nope") }
        );
        assert_eq!(requests(&h.provider, "a").len(), 1);
        h.close().await;
    }
}
