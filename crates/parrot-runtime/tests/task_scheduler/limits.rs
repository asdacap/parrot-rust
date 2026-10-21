use super::{fixture::*, harness::Harness, support::*};
use parrot_domain::values::agent_tasks::{
    AcceptanceVerdict::*, ExecutionStatus::*, LeafResponse, TaskPayload,
};
use parrot_runtime::task_scheduler::{TaskConfiguration, TaskRunner};
use std::sync::Arc;

#[tokio::test]
async fn response_repairs_reset_for_each_rejected_attempt_and_preserve_carried_result_on_repair_exhaustion()
 {
    let h = Harness::new();
    let mut configuration = TaskConfiguration::defaults(templates());
    configuration.maximum_attempts = 3;
    configuration.maximum_response_repairs = 1;
    *h.factory.configuration.lock().unwrap() = Some(configuration);
    let root = h.root("owner").await;
    h.factory.parser.error("bad-one", "bad first attempt");
    h.factory.parser.error("bad-two", "bad second attempt");
    h.factory
        .parser
        .error("bad-three", "still bad second attempt");
    h.factory.parser.answer(
        "retry",
        LeafResponse {
            result: "prior result".into(),
            verdict: RejectAndRetry {
                feedback: "retry feedback".into(),
                payload: None,
                replacement_result: Some("carried replacement".into()),
            },
        },
    );
    h.provider.script(
        "a",
        [
            Reply::text("bad-one"),
            Reply::text("retry"),
            Reply::text("bad-two"),
            Reply::text("bad-three"),
        ],
    );
    set(&root, vec![task("a")]).await;
    let done = wait_tasks(&root, |tasks| tasks[0].state == Failed).await;
    assert_eq!(done[0].result.as_deref(), Some("carried replacement"));
    assert_eq!(
        done[0].failure.as_deref(),
        Some("leaf response invalid: still bad second attempt")
    );
    let calls = requests(&h.provider, "a");
    assert_eq!(calls.len(), 4);
    assert!(prompt(&calls[1]).contains("bad first attempt"));
    assert!(prompt(&calls[2]).contains("carried replacement"));
    assert!(prompt(&calls[2]).contains("retry feedback"));
    assert!(prompt(&calls[3]).contains("bad second attempt"));
    assert_eq!(
        h.factory.parser.leaf_inputs.lock().unwrap().as_slice(),
        ["bad-one", "retry", "bad-two", "bad-three"]
    );
    h.close().await;
}

#[tokio::test]
async fn actual_retained_turn_bounds_result_feedback_and_dependency_summary_by_utf16_then_appends_notice()
 {
    let h = Harness::new();
    let root = h.root("owner").await;
    let result = format!("{}😀tail", "x".repeat(16 * 1024 - 1));
    let feedback = "é".repeat(16 * 1024 + 5);
    h.factory.parser.answer(
        "large-result",
        LeafResponse {
            result: result.clone(),
            verdict: RejectAndHalt {
                feedback: feedback.clone(),
            },
        },
    );
    h.provider.script("a", [Reply::text("large-result")]);
    set(&root, vec![task("a"), dependent("b", "a")]).await;
    let done = wait_tasks(&root, |tasks| tasks[0].state == Failed).await;
    let expected = format!("{}�\n[truncated]", "x".repeat(16 * 1024 - 1));
    assert_eq!(done[0].result.as_deref(), Some(expected.as_str()));
    assert_eq!(
        done[0].failure.as_deref(),
        Some(format!("{}\n[truncated]", "é".repeat(16 * 1024)).as_str())
    );
    let manual = format!("{}tail", "😀".repeat(8 * 1024));
    let mut update = state("a", Succeeded);
    update.result = Some(manual);
    set(&root, vec![update]).await;
    complete(&root).await;
    let b = requests(&h.provider, "b");
    assert!(prompt(&b[0]).contains(&format!("Result: {}\n[truncated]", "😀".repeat(8 * 1024))));
    assert_eq!(
        h.factory.parser.leaf_inputs.lock().unwrap()[0],
        "large-result"
    );
    h.close().await;
}

#[test]
fn oversized_leaf_and_composite_suffixes_remain_whole_even_when_larger_than_prompt_budget() {
    let configuration = TaskConfiguration::defaults(templates());
    let runner = TaskRunner::new(configuration, Arc::new(Parser::default()));
    let instruction = format!("{}TAIL-MARKER", "😀".repeat(140 * 1024));
    let mut leaf = task("a");
    leaf.payload = TaskPayload::Instruction(instruction.clone());
    let prompt = runner.build_prompt(&leaf, &[], &[], &[], None).unwrap();
    assert!(prompt.starts_with("AgentTask role: payload executor"));
    assert!(prompt.contains(&instruction));
    assert!(!prompt.contains("Task: a\n"));
    assert!(!prompt.contains("[truncated]"));
    assert!(prompt.encode_utf16().count() > 256 * 1024);
    let mut nested = task("inner");
    nested.payload = TaskPayload::Instruction(instruction);
    let composite = composite("owner", vec![nested]);
    let prompt = runner
        .build_prompt(&composite, &[], &[], &[], None)
        .unwrap();
    assert!(prompt.starts_with("AgentTask role: composite owner"));
    assert!(prompt.contains("TAIL-MARKER"));
    assert!(!prompt.contains("[truncated]"));
    assert!(prompt.encode_utf16().count() > 256 * 1024);
}

#[tokio::test]
async fn provider_execution_failure_is_terminal_without_invoking_parser() {
    let h = Harness::new();
    let root = h.root("owner").await;
    h.provider
        .failures
        .lock()
        .unwrap()
        .push_back(parrot_provider::sessions::Error::Cancelled);
    set(&root, vec![task("a")]).await;
    let done = wait_tasks(&root, |tasks| tasks[0].state == Failed).await;
    assert!(
        done[0]
            .failure
            .as_deref()
            .unwrap()
            .contains("execute agent")
    );
    assert!(h.factory.parser.leaf_inputs.lock().unwrap().is_empty());
    assert_eq!(requests(&h.provider, "a").len(), 1);
    h.close().await;
}
