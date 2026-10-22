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
fn assignment_context_formats_empty_and_multiple_siblings_and_dependencies() {
    let runner = TaskRunner::new(
        TaskConfiguration::defaults(templates()),
        Arc::new(Parser::default()),
    );
    let current = task("current");
    let siblings = [task("sibling-one"), task("sibling-two")];
    let mut dependencies = [task("first"), task("second")];
    dependencies[0].result = Some("answer {{ literal }}\nnext".into());
    for include_siblings in [false, true] {
        for include_dependencies in [false, true] {
            let siblings = if include_siblings { &siblings[..] } else { &[] };
            let dependencies = if include_dependencies {
                &dependencies[..]
            } else {
                &[]
            };
            let rendered = runner
                .build_prompt(&current, siblings, dependencies, &[], None)
                .unwrap();
            let sibling_text = if include_siblings {
                "Sibling tasks (out of scope):\nThese tasks belong to other agents. Do not implement or duplicate their work. You may consume dependency results for your assigned task.\n[sibling-one] Do sibling-one\n[sibling-two] Do sibling-two\n"
            } else {
                ""
            };
            let dependency_text = if include_dependencies {
                "\nDirect dependency summaries:\n[first] Do first\nResult: answer {{ literal }}\nnext\n[second] Do second\nResult: "
            } else {
                ""
            };
            let expected = format!(
                "{sibling_text}Task: current\nDescription: Do current\nAcceptance criteria: current works{dependency_text}\n{PRECEDENCE}AgentTask role: payload executor"
            );
            assert!(rendered.starts_with(&expected), "{rendered}");
        }
    }
}

#[test]
fn assignment_context_bounds_sibling_descriptions_before_rendering() {
    let runner = TaskRunner::new(
        TaskConfiguration::defaults(templates()),
        Arc::new(Parser::default()),
    );
    let mut sibling = task("sibling");
    sibling.description = format!("{}tail", "😀".repeat(8 * 1024));
    let rendered = runner
        .build_prompt(&task("current"), &[sibling], &[], &[], None)
        .unwrap();
    assert!(rendered.contains(&format!(
        "[sibling] {}\n[truncated]\nTask: current",
        "😀".repeat(8 * 1024)
    )));
    assert!(!rendered.contains("tail"));
}

#[test]
fn assignment_context_override_receives_lists_and_interpolation_stays_literal() {
    let catalog = templates_with_content(
        "prompt_templates:\n  agent-task.assignment-context:\n    template: 'CUSTOM {{ task_name }}|{{ description }}|{{ acceptance_criteria }}|{{ dependencies|length }}|{{ siblings|length }}'\n",
    );
    let runner = TaskRunner::new(
        TaskConfiguration::defaults(catalog),
        Arc::new(Parser::default()),
    );
    let mut current = task("current");
    current.description = "literal {task_name}".into();
    let rendered = runner.build_prompt(&current, &[], &[], &[], None).unwrap();
    assert!(rendered.starts_with("CUSTOM current|literal {task_name}|current works|0|0"));
    assert!(!rendered.contains(PRECEDENCE));
    assert!(rendered.contains("Inspect, implement, and verify this instruction:\nWork on current"));
}
