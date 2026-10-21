use super::{fixture::*, harness::Harness, support::*};
use parrot_runtime::task_scheduler::{TaskConfiguration, TaskRunner};
use std::sync::Arc;

const PRECEDENCE: &str = "This task is your current assignment and supersedes prior task instructions. Inherited parent history is context, not additional assignments.";

#[tokio::test]
async fn delivered_default_assignment_places_precedence_after_dependency_context() {
    let h = Harness::new();
    let root = h.root("owner").await;
    set(&root, vec![task("first"), dependent("current", "first")]).await;
    complete(&root).await;
    let calls = requests(&h.provider, "current");
    assert_eq!(calls.len(), 1);
    let delivered = prompt(&calls[0]);
    let header = format!(
        "Task: current\nDescription: Do current\nAcceptance criteria: current works\nDirect dependency summaries:\n[first] Do first\nResult: answer\n{PRECEDENCE}"
    );
    assert!(delivered.contains(&header), "{delivered}");
    assert_eq!(delivered.matches(PRECEDENCE).count(), 1);
    assert!(
        delivered.contains("Inspect, implement, and verify this instruction:\nWork on current")
    );
    h.close().await;
}

#[test]
fn composite_assignment_uses_the_same_default_precedence_without_reinterpreting_payload() {
    let runner = TaskRunner::new(
        TaskConfiguration::defaults(templates()),
        Arc::new(Parser::default()),
    );
    let current = composite("current", vec![task("inner")]);
    let rendered = runner.build_prompt(&current, &[], &[], &[], None).unwrap();
    assert!(rendered.starts_with(&format!(
        "Task: current\nDescription: Do current\nAcceptance criteria: current works\n{PRECEDENCE}"
    )));
    assert!(rendered.contains("AgentTask role: composite owner"));
    assert!(rendered.contains("Work on inner"));
    assert_eq!(rendered.matches(PRECEDENCE).count(), 1);
}

#[test]
fn user_header_override_remains_authoritative_and_interpolation_stays_literal() {
    let catalog = templates_with_content(
        "prompt_templates:\n  agent-task.header:\n    template: 'CUSTOM {task_name}|{description}|{acceptance_criteria}|{dependencies}'\n",
    );
    let runner = TaskRunner::new(
        TaskConfiguration::defaults(catalog),
        Arc::new(Parser::default()),
    );
    let mut current = task("current");
    current.description = "literal {task_name}".into();
    let rendered = runner.build_prompt(&current, &[], &[], &[], None).unwrap();
    assert!(rendered.starts_with("CUSTOM current|literal {task_name}|current works|"));
    assert!(!rendered.contains(PRECEDENCE));
    assert!(rendered.contains("Inspect, implement, and verify this instruction:\nWork on current"));
}
